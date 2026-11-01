# Chaos Mesh survey

What Chaos Mesh does, so that tacplus's own chaos system (#1) can take what fits and leave the rest. tacplus is **informed by
Chaos Mesh, not built on it** (Jody, 2026-11-01): nothing here is a dependency.

- **Version surveyed:** Chaos Mesh **v2.8.7** (released 2026-10-28; https://github.com/chaos-mesh/chaos-mesh/releases/tag/v2.8.7).
  The nightly upstream watch (#1, step 4) compares newer releases with this pin and updates it.
- **Sources:** `api/v1alpha1/*_types.go`, `helm/chaos-mesh/values.yaml` and `CHANGELOG.md` at tag v2.8.7, and the documentation at
  https://chaos-mesh.org/docs/.
- **Decisions:** each feature is marked **adopt now** (in the first release), **adopt later** (fits the design; a follow-up in
  PROPOSALS.md, after Jody's approval) or **not doing** (with the reason). "Controller" and "wedge" are tacplus's two services (the
  design approved on #1): the namespace-scoped controller does pod faults through the Kubernetes API and calls the per-node privileged
  wedge, over its closed, typed gRPC API, for everything else.

The rules that drive the decisions, from #1, the README and fleet-seatrials#26:

1. Faults act on **the caller's own fleet only**, never on `letsencrypt`, secret material, other deployments or system namespaces.
2. The privileged part does only what it was built for: a closed, typed API, hard bounds on every parameter, auto-revert even if the
   caller is gone, and it verifies the target itself.
3. Every action is a tacplus gRPC call, so it is in the event trace, and every fault is tagged so triage can match a failure to it.
4. It runs in production too, so a fault must be one the fleet is expected to survive and recover from on its own.

## 1. Fault kinds

Every Chaos Mesh fault is a CRD (`chaos-mesh.org/v1alpha1`) whose spec has a selector (section 3), a `duration` and, since 2.x, a
`remoteCluster`. The controller reconciles it: it selects the targets, calls the daemon to *apply*, and calls it to *recover* at the
end of the duration or when the object is deleted.

### 1.1 PodChaos (`podchaos_types.go`)

| Action | Parameters | How | Decision |
|---|---|---|---|
| `pod-kill` (default) | `gracePeriod` (s, ≥0; 0 = immediate) | Kubernetes API: delete the pod | **Adopt now**, controller, through the Kubernetes API. The default fault of every fleet (README). `gracePeriod` bounded (0–30 s). |
| `pod-failure` | `duration` (required) | Patches the pod's containers to a pause image until recovery, so it is not Ready while it stays scheduled | **Adopt later**, controller. It mutates the pod spec (image), which the controller's RBAC would need (`patch pods`), and it must be reverted by someone: tacplus's auto-revert lives in the wedge, so this needs a design for a controller-side revert that survives the controller dying (a lease on the pod, reverted by the wedge or by the pod's own restart). |
| `container-kill` | `containerNames` | Daemon kills the container through the CRI | **Adopt now**, wedge `KillContainer`: one container of a pod (a sidecar, say) without deleting the pod. |

### 1.2 NetworkChaos (`networkchaos_types.go`)

Common parameters: `direction` (`to` / `from` / `both`), `target` (a second selector: the peer pods), `externalTargets` (IPs or
hostnames outside the cluster), `device` and `targetDevice` (interface names). The daemon enters the pod's network namespace and runs
`tc` (netem, tbf), `iptables` and `ipset`.

| Action | Parameters | Decision |
|---|---|---|
| `delay` (default) | `latency`, `jitter`, `correlation` (%), `reorder` {`reorder` %, `correlation`, `gap`} | **Adopt now**, wedge `DelayNetwork`: latency ≤ 2 s, jitter ≤ latency, correlation 0–100. `reorder` adopt later. |
| `loss` | `loss` (%), `correlation` | **Adopt now**, wedge `DropPackets`: loss ≤ 50 %. |
| `duplicate` | `duplicate` (%), `correlation` | **Adopt later**: same netem qdisc as delay; low value until the first two are proven. |
| `corrupt` | `corrupt` (%), `correlation` | **Adopt later**: as duplicate. |
| `netem` | delay + loss + duplicate + corrupt in one qdisc | **Adopt later**, as one typed call with each part bounded, once each part exists alone. |
| `bandwidth` | `rate`, `limit`, `buffer`, `peakrate`, `minburst` (tbf) | **Adopt later**, wedge `LimitBandwidth`, rate ≥ 1 mbit. |
| `partition` | `direction`, `target` / `externalTargets` | **Adopt later**, wedge `PartitionNetwork`, peers restricted to pods of the same fleet. The "member" level (README): splitting a stratum member from its peers is the fault that tests quorum. It has a larger blast radius than delay, so it waits for the first release to run clean. |
| `externalTargets` (any action) | IPs / hostnames outside the cluster | **Not doing** for arbitrary hosts: the peer must be a pod of the fleet the wedge has resolved itself. A typed "egress to the model vendor via llmproxy" fault could come later as its own call. |
| `device` / `targetDevice` | interface names | **Not doing** as a free string: the wedge applies the fault on the pod's own interface (`eth0` in its netns), which it finds itself. |

### 1.3 DNSChaos (`dnschaos_type.go`)

`error` (default) or `random` (a random IP) for `patterns` (domain names with `?` and a trailing `*`). It needs Chaos Mesh's own
CoreDNS deployment (`dnsServer` in the chart) and rewrites the selected pods' resolution to it.

**Adopt later**, as a wedge call scoped to the pod's netns (answering NXDOMAIN for a bounded list of fleet service names) rather than a
second cluster DNS. Patterns limited to the fleet's own service names.

### 1.4 HTTPChaos (`httpchaos_types.go`, `podhttpchaos_types.go`)

A transparent proxy (`chaos-tproxy`) injected into the pod's netns intercepts HTTP on `port`, filtered by `path`, `method`, `code`,
`request_headers`, `response_headers`, `target` (`Request` / `Response`), with actions `abort`, `delay`, `replace` (path, method,
code, body, queries, headers) and `patch` (body, queries, headers). `tls` names a Secret (cert, key, CA) so it can terminate TLS.

- **Not doing** HTTP rewriting (`replace` / `patch`) and TLS termination: the second needs the service's own key material, which
  rule 1 forbids, and every tacplus S2S call is TLS.
- **Adopt later**, an equivalent at the right layer: a gRPC fault (delay or a status code on chosen methods) in the tacplus chassis,
  switched on by the controller through a typed call. It needs a chassis feature in tacplus-dev/tacplus, and it is cleaner than a
  proxy: it sees the method and status, and the event trace records it.

### 1.5 IOChaos (`iochaos_types.go`, `podiochaos_types.go`)

The daemon mounts a FUSE filesystem (`toda`) over `volumePath` in the container's mount namespace and filters by `path`, `methods`
(31 FUSE operations: `read`, `write`, `fsync`, `open`, …) and `percent`. Actions:

| Action | Parameters | Decision |
|---|---|---|
| `latency` | `delay` | **Adopt later**, wedge `DelayIo` on a volume of the pod, delay ≤ 1 s, percent ≤ 50. The stratum and blobstore durability tests want it. |
| `fault` | `errno` | **Adopt later**, wedge `FailIo`, errno from a fixed list (`EIO`, `ENOSPC`). |
| `attrOverride` | `attr` (inode, size, perm, times, …) | **Not doing**: corrupts what the program sees about its files, with little resiliency value over `fault`. |
| `mistake` | `mistake` {`filling` zero/random, `maxOccurrences`, `maxLength`} | **Not doing** in production: silent data corruption is a lost-acknowledged-write incident by definition (fleet-seatrials#26), not a fault the fleet is expected to recover from. |

The FUSE overlay needs `SYS_ADMIN` and `MKNOD` in the target's mount namespace, and it has to be put in place while the process has
the volume open. Its recovery has had bugs upstream (v2.8.7 upgrades toda to remove leftover directories, #5138). Kept for later for
that reason.

### 1.6 BlockChaos (`blockchaos_types.go`)

`delay` (`latency`, `jitter`, `correlation`) on a block device behind `volumeName`, through a kernel module (`chaos-driver`).

**Not doing**: it loads a kernel module on the node, a privilege beyond the wedge's, and it affects every pod using that device, not
only the target.

### 1.7 TimeChaos (`timechaos_types.go`)

`timeOffset` (signed duration) on `clockIds` (default `CLOCK_REALTIME`; ten clocks are allowed). The daemon attaches to the process
with `ptrace` (`watchmaker`) and patches the vDSO so the process sees a shifted clock; the node's clock is untouched.

**Adopt later**, wedge `SkewClock`, `CLOCK_REALTIME` only, offset ≤ ±5 min (token expiry, certificate validity and lease timing are
what it tests). Needs `SYS_PTRACE` on the target's process and a revert that restores the vDSO; Chaos Mesh's revert needs its daemon
alive, which the wedge's auto-revert must not.

### 1.8 StressChaos (`stresschaos_types.go`)

`stressors.cpu` {`workers`, `load` %, `options`} and `stressors.memory` {`workers`, `size`, `oomScoreAdj`, `options`}, or a raw
`stressngStressors` string. The daemon runs `stress-ng` / `memStress` in the target container's cgroup, so the load counts against its
limits.

- **Adopt now**, wedge `Stress`: CPU workers ≤ the container's CPU limit, load ≤ 100 %; memory ≤ 50 % of the container's limit
  (an OOM kill is a pod kill, which the controller already does on purpose).
- **Not doing**: `options` and `stressngStressors`. They are free-form command-line arguments: the "no exec, no generic runtime
  access" rule.

### 1.9 KernelChaos (`kernelchaos_types.go`)

`failKernRequest` {`failtype` slab / alloc_page / bio, `callchain` frames with `predicate` C expressions, `probability`, `times`},
through BPF (`bpfki`).

**Not doing**: it compiles and loads caller-supplied BPF, which is generic kernel access, and its effect is node-wide.

### 1.10 JVMChaos (`jvmchaos_types.go`)

Attaches a Byteman agent to a Java process: `latency`, `return`, `exception`, `stress`, `gc`, `ruleData` (a raw Byteman rule) and
`mysql` (faults in the MySQL connector), by `class` and `method`.

**Not doing**: tacplus services are Rust by default (tacplus-dev/roadmap decisions/rust-default-language-2026-10-27.md), and
`ruleData` is code injection. A JVM-specific fault would come from the chassis-level gRPC fault (1.4) instead, which works in any
language.

### 1.11 Cloud and node faults: AWSChaos, GCPChaos, AzureChaos

AWS `ec2-stop`, `ec2-restart`, `detach-volume`; GCP `node-stop`, `node-reset`, `disk-loss`; Azure `vm-stop`, `vm-restart`,
`disk-detach`. Each takes a cloud credential from a Secret.

**Adopt later**, as the README's "node" level, but not through cloud credentials in the controller: a node fault in tacplus is a
**cordon and drain** of one node of the fleet (Kubernetes API), or the operator's own node lifecycle. A cloud credential with VM stop
rights is secret material the chaos system must never hold.

### 1.12 PhysicalMachineChaos (`physical_machine_chaos_types.go`)

Faults on hosts outside Kubernetes through `chaosd` (an agent on each host): stress, disk fill and payload, network (the same netem
set plus `flood`, `down`, `dns`), process kill, JVM, clock, Redis, Kafka, HTTP, file create/modify/delete/rename/append/replace, VM,
`user_defined` (a shell command).

**Not doing**: tacplus fleets run on Kubernetes, and `user_defined` and the file faults are exec by another name.

## 2. Scheduling and workflows

| Feature | What | Decision |
|---|---|---|
| **Schedule** (`schedule_types.go`) | A cron spec (`schedule`, seconds optional, `@every` descriptors) that creates a fault object each time; `concurrencyPolicy` `Forbid` / `Allow`; `startingDeadlineSeconds`; `historyLimit`. | **Adopt now**, in the controller: one schedule per fleet from the uni-config (interval, the levels switched on, the fault kinds, the intensity). Always `Forbid`: one fault at a time per fleet in the first release, so triage maps a failure to exactly one fault. A **round** is one scheduled firing; its id goes into the fault's tag. |
| **Workflow** (`workflow_types.go`) | A DAG of templates: `Serial`, `Parallel`, `Suspend` (wait), `Schedule`, `Task` (runs a **container** and branches on its output, `conditionalBranches`), `StatusCheck`, and every fault kind; a `deadline` per node. v2.8.7 adds `serviceAccountName`, `nodeSelector` and `tolerations` for Task pods. | **Adopt later**, without `Task`: a typed "scenario" (serial or parallel steps of typed faults, with waits) is useful for game days. `Task` runs an arbitrary container: **not doing**. |
| **StatusCheck** (`statuscheck_types.go`) | Probes an HTTP URL (`url`, `method`, `headers`, `body`, `criteria.statusCode`) in `Synchronous` or `Continuous` mode, with `intervalSeconds`, `timeoutSeconds`, `failureThreshold`, `successThreshold`; a workflow can abort on it (`abortWithStatusCheck`). | **Adopt now, in a different form**: the controller checks the target's **recovery** after each fault (its pods Ready again within the fault's recovery bound, through the Kubernetes API), and records `recovered` or `not-recovered` in the trace. That is what triage needs (fleet-seatrials#26). It also **stops the round** (no next fault) while the fleet is not healthy. HTTP probes to arbitrary URLs: **not doing**. |
| **Pause** (annotation `experiment.chaos-mesh.org/pause`) | Pauses a running fault (it is recovered) and resumes it later. | **Adopt now**, as a fleet-wide pause: a uni-config switch and a controller call that reverts every live fault and stops scheduling. Per-fault resume is not needed: a fault is short. |
| **Duration** | Every fault has an optional `duration`; without one it lasts until deleted. | **Adopt now**, and **required**: every wedge call carries a duration within its kind's bound. There is no "until deleted". |
| **One-shot** | Faults without a recovery (pod-kill, container-kill) are applied once. | **Adopt now**: kills are one-shot; their "recovery" is the restart, which the controller watches. |
| **RemoteCluster** (`remote_cluster_types.go`) | The controller installs and drives Chaos Mesh in another cluster from a kubeconfig Secret. | **Not doing**: the chaos system acts in its own fleet only, and a kubeconfig for another cluster is secret material. |

## 3. Safety

| Feature | What | Decision |
|---|---|---|
| **Selectors** (`selector.go`) | `namespaces`, `labelSelectors`, `expressionSelectors`, `annotationSelectors`, `fieldSelectors`, `nodes`, `nodeSelectors`, `pods` (namespace → names), `podPhaseSelectors`; `containerNames` for container faults. | **Adopt now, narrowed**: the controller lists only its fleet's namespaces, filters by service (the tacplus service label) and pod phase `Running`, and drops excluded services before choosing. The wedge re-checks on its own (below). Free label, annotation and field selectors: not needed, the services are the unit. |
| **Modes** | `one`, `all`, `fixed` (n), `fixed-percent`, `random-max-percent` (with `value`). | **Adopt now:** `one` only. **Adopt later:** `fixed` and `fixed-percent`, capped so a service always keeps a quorum (never more than ⌊(n−1)/2⌋ of a service's pods at once). **Not doing:** `all`: in production, all replicas of a service at once is an outage by design, not a resiliency test. |
| **Namespace filter** (`controllerManager.enableFilterNamespace`) | Only namespaces annotated `chaos-mesh.org/inject: enabled` can be targeted. | **Adopt now, stronger**: the scope is not opt-in per namespace but fixed: the controller's RBAC is Role/RoleBinding in its fleet's namespaces only (operator#18), and the wedge refuses any pod not in the caller's fleet. |
| **Namespace-scoped install** (`clusterScoped: false`, `targetNamespace`) | The controller watches one namespace with a Role instead of a ClusterRole. | **Adopt now**: namespace-scoped is the only mode. |
| **Exclusions** | None built in: beyond the namespace annotation filter and namespace-scoped mode (`pkg/selector/generic/namespace/selector.go`), any pod a selector matches can be targeted, `kube-system` and Chaos Mesh's own included. | **Adopt now**, as an exclusion list in the **wedge**, not only the controller: system namespaces (`kube-*`, the platform's own), other deployments, `letsencrypt`, any pod that mounts the fleet's secret material, and the chaos services themselves. Compiled in, plus a list from the wedge's own config that can only add. |
| **Admission webhook validation** (`*_webhook.go`) | Validates and defaults every CRD on create and update (durations, percentages, selectors). | **Adopt now, at the wedge**: bounds are enforced by the privileged side, the one that acts, not by an admission webhook in front of it. The controller checks the same bounds first, to refuse early, but the wedge's check is the one that counts. |
| **Recovery** | The controller calls the daemon's recover RPC at the end of `duration`, on delete (a finalizer holds the object until it succeeds) and on pause. If the controller is gone, the fault stays until it comes back; `chaosctl` can force-recover by hand. The daemon has no timer of its own. | **Adopt now, inverted**: the **wedge** owns the timer. Every fault is applied with its end time, and the wedge reverts it at that time on its own, whether or not the controller is alive. On wedge start it reverts anything a previous instance left (it records live faults on the node's filesystem before applying them). Acceptance: "auto-revert when the controller dies mid-fault". |
| **Dashboard security mode** (`dashboard.securityMode`) | The web dashboard uses the caller's own Kubernetes token, so its RBAC applies. | **Not doing** a dashboard. The event trace and the controller's typed read calls (live faults, last rounds) are the view. |
| **Audit** | Kubernetes Events on the chaos object and its status records (`Apply` / `Recover`, `Succeeded` / `Failed`, timestamps). | **Adopt now, as the event trace**: every schedule, inject, revert and refusal is a tacplus gRPC call, so it is in the trace, and each publishes the fault tag beside it as a typed `chaosv1.FaultEvent` record (#1 Q2: the trace itself carries no tags, and stays ephemeral) (fleet, target, kind, parameters, round, start, end). The controller also puts a Kubernetes Event on the target pod, so `kubectl describe` shows it (fleet-seatrials#26's marker). |

## 4. Architecture and privilege

| Component | What it is | Privilege | Decision |
|---|---|---|---|
| **chaos-controller-manager** | Deployment; controller-runtime reconcilers for every CRD, the admission webhooks, leader election. | ClusterRole (or Role when namespace-scoped) over pods, the Chaos Mesh CRDs, events, and more. | **Adopt now, as tacplus's controller**: a tacplus Rust service in the fleet's namespace, configured from the uni-config (operator#17), with a Role limited to its fleet's namespaces (pods: `get`, `list`, `watch`, `delete`; events: `create`). No CRDs: a CRD is cluster-scoped to install, and a deployment has no rights outside its namespace (decisions/deployments-per-cluster-2026-10-26.md). |
| **chaos-daemon** | DaemonSet, one per node; a gRPC server (port 31767) that enters the target's namespaces (`nsexec`) and runs `tc`, `iptables`, `ipset`, `stress-ng`, `toda`, `watchmaker`, and talks to the container runtime socket (docker / containerd / CRI-O). | **`privileged: true`** by default (or `SYS_PTRACE`, `NET_ADMIN`, `NET_RAW`, `MKNOD`, `SYS_CHROOT`, `SYS_ADMIN`, `KILL`, `IPC_LOCK`), `hostPID`, the runtime socket mounted. Its gRPC API (`pkg/chaosdaemon/pb/chaosdaemon.proto`: `SetTcs`, `SetIptablesChains`, `FlushIPSets`, `SetTimeOffset`, `ContainerKill`, `ExecStressors`, `ApplyIOChaos`, `ApplyHttpChaos`, `ApplyBlockChaos`, `SetDNSServer`, `InstallJVMRules`, …) takes raw tc and iptables rules, stress-ng argument strings and JSON action lists for whatever container id the caller names: the daemon trusts the controller for both the target and the bounds. | **Adopt now, as the wedge**, with what Chaos Mesh lacks: a closed, typed API (no command strings, no exec); hard bounds; its own end-of-fault timer; and target verification on the node (it resolves the pod through the runtime and the kubelet's view, and refuses out-of-fleet and excluded targets). Capabilities only those its calls need (`NET_ADMIN` for `tc`, `KILL`, `SYS_ADMIN` for `setns`, `SYS_PTRACE` once `SkewClock` comes), not `privileged`. The nsproxy pattern (tacplus-dev/nsproxy): the one privileged component, and it can only do what it was built for. |
| **controller ↔ daemon mTLS** (`chaosDaemon.mtls.enabled: true`) | The chart generates a CA and certificates at install; the daemon accepts any client with a certificate from that CA. | Possession of the client certificate means full control of every node's daemon. | **Not doing**: replaced by **kernel tokens plus server TLS** (#1): the wedge serves TLS, and each call carries the caller's kernel-signed S2S token, which names the calling service; the fleet is the kernel that signed it. That identity, not a shared CA, is what scopes the call: a wedge trusts only its own fleet's kernel, so a controller can only ever act on its own fleet (so there is one wedge per fleet per node, #1 Q1). |
| **chaos-dashboard** | Web UI and REST API for creating and watching experiments. | Its own service account, or the user's token in security mode. | **Not doing** (see Audit). |
| **chaos-dns-server** | A CoreDNS with a chaos plugin, for DNSChaos. | Cluster DNS changes. | **Not doing**; DNSChaos later as a wedge call (1.3). |
| **chaosctl** | CLI to debug and force-recover. | The user's kubeconfig. | **Adopt later**: a `chaos` subcommand of the controller (list live faults, pause, revert all), each a gRPC call, so it is in the trace. |
| **bpfki**, **chaos-driver**, **chaosd** | Helpers for KernelChaos, BlockChaos and PhysicalMachineChaos. | BPF, a kernel module, host root. | **Not doing** (1.6, 1.9, 1.12). |
| **Prometheus metrics**, **profiling** (`enableProfiling`) | Controller metrics; pprof. | — | **Adopt later**: tacplus's chassis metrics cover the services; fault counts by kind and outcome come from the event trace. |

## 5. What the first release does (summary)

| | Adopt now | Adopt later | Not doing |
|---|---|---|---|
| Faults | pod-kill (controller); `KillContainer`, `DelayNetwork`, `DropPackets`, `Stress` (wedge) | pod-failure, duplicate, corrupt, netem, bandwidth, partition, DNS, gRPC faults in the chassis, IO latency and errors, `SkewClock`, node cordon and drain | HTTP rewriting and TLS termination, IO attribute and data corruption, BlockChaos, KernelChaos, JVMChaos, cloud credentials, PhysicalMachineChaos, external targets, raw stress-ng options |
| Scheduling | one per-fleet schedule from the uni-config, `Forbid` concurrency, rounds, required durations, fleet-wide pause, recovery check | scenarios (typed workflows without `Task`), `fixed` and `fixed-percent` modes with a quorum cap | `Task` containers, `all` mode, remote clusters, HTTP status checks to arbitrary URLs |
| Safety | fleet-scoped Role, wedge-side target verification and exclusion list, wedge-side bounds, wedge-owned auto-revert, event trace with fault tags, Kubernetes Events on targets | a `chaos` CLI | dashboard |
| Architecture | controller (namespace-scoped) + wedge (per node, closed API), kernel tokens + server TLS | metrics by kind and outcome | CRDs, mTLS controller–daemon link, `privileged: true` |

## 6. Upstream changes reviewed

The nightly watch records here each upstream release it has compared, and the entries of its changelog's `Added` sections, so it only
suggests features it has not seen. Up to the pin, every feature is in sections 1–4.

| Release | `Added` entries | Where |
|---|---|---|
| v2.8.7 (2026-10-28) | `serviceAccountName` for Workflow Task pods (#5162); `nodeSelector` and `tolerations` for workflow task pods (#4790) | Section 2, Workflow (`Task`: not doing) |
