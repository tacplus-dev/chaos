# chaos

The chaos system that every tacplus fleet deploys by default (tacplus-dev/operator#18; Jody, 2026-11-01). It injects faults into its
**own fleet only** (pod kills by default; network and stress faults as opt-in levels, members and nodes later) and tags every fault,
so a failure can be matched to the fault behind it: one that recovered is logged and ignored, anything else is an incident. Turn it
off per fleet in `fleet.yaml`:

```yaml
chaos:
  enabled: false
```

It is **informed by Chaos Mesh, not built on it**: [docs/chaos-mesh-survey.md](docs/chaos-mesh-survey.md) describes what Chaos Mesh
does (v2.8.7) and decides, feature by feature, what we adopt now, later or never. The design is approved on #1. It never acts outside
its fleet's namespaces, nor on `letsencrypt` or secret material.

## Two services

Both are tacplus Rust services, generated from [proto/](proto/) (`tacplus: "2.5"`), each in its own generation so the privileged
binary carries no other service's surface:

| | **chaos-controller** ([controller/](controller/)) | **chaos-wedge** ([wedge/](wedge/)) |
|---|---|---|
| Runs | Deployment, 1 replica, in the fleet's namespace | DaemonSet, one per node **per fleet**, in the fleet's namespace (#1 Q1) |
| API | `chaoscontroller.Controller`: `CreateRound`, `UpdateSchedule` (pause/resume), `ListRounds` | `chaoswedge.Wedge`: `CreateNetworkDelay`, `CreatePacketLoss`, `CreateContainerKill`, `CreateStress`, `DeleteFault`, `ListFaults`; callers: chaos-controller only |
| Does | schedules one round per interval; pod kills through the Kubernetes API; everything else through the wedge on the target's node; checks the target recovers | applies and reverts the faults on its node, after checking the target itself |
| Privilege | none: a Role in the fleet's namespaces (pods `get list watch delete`, events `create`) | see [Security](#security) |

Every action is a tacplus gRPC call (so it is in the event trace) and a `chaosv1.FaultEvent` record on the durable topic
`chaosv1.FaultEventBus.stream`: fleet, target, service, kind, parameters, round, fault id, start, end, outcome (`INJECTED`, `REVERTED`,
`REFUSED` with its reason, `RECOVERED`, `NOT_RECOVERED`), and the `trace_id` of the call that started the fault, which is where the
record and the (ephemeral, tacplus#265) trace meet (#1 Q2). The controller also puts a Kubernetes Event (`ChaosFault`) with the tag on
the target pod. Triage (fleet-seatrials#26) reads the topic.

The compiled-in bounds ([policy/src/bounds.rs](policy/src/bounds.rs)), refused, never clamped: `duration_s` 1–300; `latency_ms`
1–2000; `jitter_ms` ≤ latency; `loss_pct` 1–50; `correlation_pct` ≤ 100; `cpu_workers` ≤ the container's CPU limit rounded up (1
without one); `cpu_load_pct` 1–100; `memory_pct_of_limit` 1–50 (none without a memory limit); a pod kill's grace period 0–30 s; one
live fault per pod, at most 4 per node. Config can only tighten them, and only add to the exclusion list
([policy/src/exclusion.rs](policy/src/exclusion.rs)): `kube-*` and the platform's namespaces, any namespace not labelled as the
fleet's by nsproxy (`tacplus.dev/deployment`, `nsproxy.tacplus.dev/deployment`), `letsencrypt`, the chaos services, any pod that mounts
a Kubernetes Secret volume (the fleet's secret material lives only in Secrets).

The nightly **upstream watch** ([upstream-watch/](upstream-watch/), `.github/workflows/upstream-watch.yml`) compares Chaos Mesh's newer
releases with the survey and writes a suggestion (Type: Resiliency) for each new `Added` feature, to be filed with tacplus-dev/looper#50's
filing command, and proposes the survey's new pin as a PR.

## Security

The wedge is the only privileged component (the nsproxy pattern): **Jody approved its privileges as designed on 2026-11-01** (#1 Q3),
without a round-table:

- capabilities `NET_ADMIN` (netem through `tc`), `SYS_ADMIN` (`setns` into a pod's network namespace, and writing the target's
  cgroup), `KILL` (ending its own stress process); **not** `privileged: true`;
- `hostPID` (the pods' processes, for their network namespaces and cgroups);
- the containerd socket;
- a hostPath journal, `/var/lib/tacplus-chaos/<fleet>/`;
- a ClusterRole with `get namespaces` only (nsproxy's fleet labels).

**Caveat: the code is the boundary.** A socket mounted read-only still allows every runtime call: whoever can connect to containerd can
drive it. So the controls are the wedge's own: its closed, typed API (no exec, no command line, interface name, selector or container
id from the caller; every parameter a number or a name it resolves itself), the compiled-in bounds, its own auto-revert (journalled
before applying, reverted at the end by its own timer whether the controller is alive or not, and by the next wedge after a crash), and
its own target checks (the pod resolved on its own node, its namespace's own labels naming the wedge's fleet, not excluded). The wedge
serves one fleet and trusts only that fleet's kernel: server TLS plus a kernel-signed token, never mTLS, the generated authorizer admitting
chaos-controller only.

**The CRI calls the wedge makes**, all of them ([wedge/src/runtime.rs](wedge/src/runtime.rs)), none with an id from the caller:

| Call | For |
|---|---|
| `ListPodSandbox` (filter: the pod's namespace and name labels, state `SANDBOX_READY`) | is the target pod on this node |
| `PodSandboxStatus` (verbose) | the sandbox's pid: `/proc/<pid>/ns/net` is the pod's network namespace |
| `ListContainers` (filter: that sandbox, state `CONTAINER_RUNNING`) | the target container |
| `ContainerStatus` (verbose) | its pid (for its cgroup and limits) and its mounts (does the pod hold a Secret) |
| `StopContainer` (timeout 0) | `CreateContainerKill`: the only call that changes anything |

## Build and test

The gates, as CI runs them ([ci/gates.sh](ci/gates.sh)): generate both services with tacplus at the floor
([ci/tacplus.sh](ci/tacplus.sh); nothing under `target/` is tracked), then `cargo build --locked` and `cargo test --locked` in
`policy/`, `wedge/`, `controller/` and `upstream-watch/`, then the wedge's real-kernel tests ([ci/netns-test.sh](ci/netns-test.sh)):
netem through `tc` in a network namespace, its revert at the end and after a crash, in a container with `NET_ADMIN` and `SYS_ADMIN`
only. A run in a real cluster belongs to fleet-seatrials#26.

```bash
ci/gates.sh
```
