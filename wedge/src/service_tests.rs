// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! The wedge's acceptance tests (#1): every call goes in through the GENERATED `ValidationWrapper`, with
//! a signed bearer and a trace_id, as a real call arrives, with the generated trace publisher wired to a
//! list. The node is fake (runtime, namespace labels, kernel), so these run anywhere; the real kernel's
//! netem is tests/netns.rs.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chaos_policy::exclusion::{DEPLOYMENT_LABEL, TENANT_DEPLOYMENT_LABEL};
use chaoswedge_model::s2sm::internal::grpc::servers::services::chaoswedge::wedge::{Authorizer, ValidationWrapper};
use chaoswedge_model::types::services::chaosv1::{FaultEvent, FaultKind, FaultReply, FaultTag, Outcome, Target};
use chaoswedge_model::types::services::chaoswedge::chaoswedge::wedge_server::Wedge as WedgeRpc;
use chaoswedge_model::types::services::chaoswedge::chaoswedge::{
    Config, CreateContainerKillRequest, CreateNetworkDelayRequest, CreatePacketLossRequest, CreateStressRequest, DeleteFaultRequest,
    ListFaultsRequest,
};
use chaoswedge_model::types::sys::trace::{TraceEvent, TraceEventKind};

use crate::kernel::{Kernel, Netem, StressProcess, StressSpec};
use crate::kube::Namespaces;
use crate::runtime::{Resolved, Runtime};
use crate::service::Wedge;

const FLEET: &str = "prod";
const NODE: &str = "node-a";

// --- the fake node -----------------------------------------------------------------------------------

/// The pods on this node, by namespace/pod.
struct FakeRuntime {
    pods: HashMap<(String, String), Resolved>,
    stopped: Mutex<Vec<String>>,
}

#[async_trait]
impl Runtime for FakeRuntime {
    async fn resolve(&self, namespace: &str, pod: &str, _container: &str) -> Result<Resolved, String> {
        self.pods.get(&(namespace.into(), pod.into())).cloned().ok_or_else(|| format!("pod {namespace}/{pod} is not running on this node"))
    }

    async fn stop_container(&self, container_id: &str) -> Result<(), String> {
        self.stopped.lock().unwrap().push(container_id.into());
        Ok(())
    }
}

struct FakeNamespaces(BTreeMap<String, BTreeMap<String, String>>);

#[async_trait]
impl Namespaces for FakeNamespaces {
    async fn labels(&self, namespace: &str) -> Result<BTreeMap<String, String>, String> {
        Ok(self.0.get(namespace).cloned().unwrap_or_default())
    }
}

/// Records what the wedge did to the kernel; the netem "in place" is a set, so a revert is visible.
#[derive(Default)]
struct FakeKernel {
    calls: Mutex<Vec<String>>,
    netem: Mutex<HashMap<u32, Netem>>,
    stress: Mutex<HashMap<u32, StressSpec>>,
    /// The netns inode a pid reports; a pod restarted elsewhere reports another.
    inodes: Mutex<HashMap<u32, u64>>,
    /// How many netem removals fail before they work again.
    failing_removes: Mutex<u32>,
}

#[async_trait]
impl Kernel for FakeKernel {
    fn netns_inode(&self, pid: u32) -> Result<u64, String> {
        self.inodes.lock().unwrap().get(&pid).copied().ok_or_else(|| format!("no pid {pid}"))
    }

    async fn add_netem(&self, pid: u32, netem: Netem) -> Result<(), String> {
        self.calls.lock().unwrap().push(format!("add_netem {pid} {netem:?}"));
        self.netem.lock().unwrap().insert(pid, netem);
        Ok(())
    }

    async fn remove_netem(&self, pid: u32, netns_inode: u64) -> Result<bool, String> {
        self.calls.lock().unwrap().push(format!("remove_netem {pid}"));
        {
            let mut failing = self.failing_removes.lock().unwrap();
            if *failing > 0 {
                *failing -= 1;
                return Err("tc: RTNETLINK answers: Device or resource busy".into());
            }
        }
        if self.inodes.lock().unwrap().get(&pid) != Some(&netns_inode) {
            return Ok(false);
        }
        Ok(self.netem.lock().unwrap().remove(&pid).is_some())
    }

    async fn start_stress(&self, cgroup_dir: &Path, spec: StressSpec) -> Result<StressProcess, String> {
        self.calls.lock().unwrap().push(format!("start_stress {} {spec:?}", cgroup_dir.display()));
        self.stress.lock().unwrap().insert(9000, spec);
        Ok(StressProcess { pid: 9000, start_ticks: 77 })
    }

    async fn stop_stress(&self, process: StressProcess) -> Result<bool, String> {
        self.calls.lock().unwrap().push(format!("stop_stress {}", process.pid));
        Ok(self.stress.lock().unwrap().remove(&process.pid).is_some())
    }
}

fn pod(namespace: &str, pod: &str, service: &str, sandbox_pid: u32) -> Resolved {
    Resolved {
        namespace: namespace.into(),
        pod: pod.into(),
        container: service.into(),
        container_id: format!("cid-{pod}"),
        service: service.into(),
        sandbox_pid,
        cgroup_dir: PathBuf::from(format!("/sys/fs/cgroup/kubepods/{pod}")),
        cpu_limit_millis: Some(2000),
        memory_limit_bytes: Some(1 << 30),
        mounts_secret: false,
    }
}

/// The node: stratum and people pods of fleet prod (deployment and tenant namespaces), a pod of fleet dev,
/// letsencrypt, a pod that mounts a Secret, and an unlabelled namespace named like prod's.
fn node() -> (FakeRuntime, FakeNamespaces, FakeKernel) {
    let mut kmaker = pod("tac-prod", "kmaker-0", "kmaker", 106);
    kmaker.mounts_secret = true;
    let pods = [
        pod("tac-prod", "stratum-0", "stratum", 101),
        pod("tac-prod", "stratum-1", "stratum", 102),
        pod("tac-prod-a1b2c3d4", "people-0", "people", 103),
        pod("tac-dev", "stratum-0", "stratum", 104),
        pod("tac-prod", "letsencrypt-0", "letsencrypt", 105),
        kmaker,
        pod("tac-prodx", "stratum-0", "stratum", 107),
        pod("tac-prod", "worker-0", "worker", 108),
        pod("tac-prod", "worker-1", "worker", 109),
        pod("tac-prod", "worker-2", "worker", 110),
        pod("tac-prod", "worker-3", "worker", 111),
    ];
    let runtime = FakeRuntime { pods: pods.into_iter().map(|p| ((p.namespace.clone(), p.pod.clone()), p)).collect(), stopped: Mutex::default() };
    let label = |k: &str, v: &str| BTreeMap::from([(k.to_string(), v.to_string())]);
    let namespaces = FakeNamespaces(BTreeMap::from([
        ("tac-prod".to_string(), label(DEPLOYMENT_LABEL, FLEET)),
        ("tac-prod-a1b2c3d4".to_string(), label(TENANT_DEPLOYMENT_LABEL, FLEET)),
        ("tac-dev".to_string(), label(DEPLOYMENT_LABEL, "dev")),
    ]));
    let kernel = FakeKernel::default();
    *kernel.inodes.lock().unwrap() = (101..=111).map(|pid| (pid, 4026530000 + pid as u64)).collect();
    (runtime, namespaces, kernel)
}

// --- auth: a signed bearer, and an authorizer that only parses it (the generated one enforces
// auth_services: ["chaos-controller"]; the wedge checks the caller again itself, which these test) ------

struct ParseOnly;

#[tonic::async_trait]
impl Authorizer for ParseOnly {
    async fn authorize(&self, _m: &str, authorization: Option<&str>) -> Result<HashMap<String, String>, tonic::Status> {
        authorization.map(|_| HashMap::new()).ok_or_else(|| tonic::Status::unauthenticated("no token"))
    }
    async fn check_at_or_below(&self, m: &str, a: Option<&str>, _t: &str) -> Result<HashMap<String, String>, tonic::Status> {
        self.authorize(m, a).await
    }
    async fn authenticate(&self, m: &str, a: Option<&str>) -> Result<HashMap<String, String>, tonic::Status> {
        self.authorize(m, a).await
    }
}

fn signed<T>(service: &str, req: T) -> tonic::Request<T> {
    let key = key::keys::generate_ed25519_key().unwrap();
    let mut t = token::token::Token::new();
    t.set_issuer("chaos-wedge-tests").unwrap();
    t.set_sub_type("service").unwrap();
    t.set_subject(&format!("spiffe://td.test/system/services/{service}")).unwrap();
    t.set_namespace("/system").unwrap();
    t.set_expiration(crate::service::now_ms() / 1000 + 3600).unwrap();
    t.sign(&key).unwrap();
    let mut request = tonic::Request::new(req);
    request.metadata_mut().insert("authorization", format!("Bearer {}", t.base64().unwrap()).parse().unwrap());
    request
}

// --- the harness ---------------------------------------------------------------------------------------

struct Harness {
    wedge: Wedge,
    wrapper: ValidationWrapper,
    runtime: Arc<FakeRuntime>,
    kernel: Arc<FakeKernel>,
    events: Arc<Mutex<Vec<FaultEvent>>>,
    traces: Arc<Mutex<Vec<TraceEvent>>>,
    _journal: Arc<tempfile::TempDir>,
}

impl Harness {
    async fn start() -> Harness {
        let (runtime, namespaces, kernel) = node();
        Harness::start_on(Arc::new(runtime), Arc::new(namespaces), Arc::new(kernel), Arc::new(tempfile::tempdir().unwrap())).await
    }

    async fn start_on(runtime: Arc<FakeRuntime>, namespaces: Arc<FakeNamespaces>, kernel: Arc<FakeKernel>, journal: Arc<tempfile::TempDir>) -> Harness {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let wedge = Wedge::new(
            NODE.into(),
            journal.path().to_path_buf(),
            runtime.clone(),
            namespaces,
            kernel.clone(),
            Arc::new(move |e| sink.lock().unwrap().push(e)),
        );
        wedge.configure(&Config { fleet: FLEET.into(), ..Default::default() }).await.unwrap();
        let wrapper = ValidationWrapper::new(Arc::new(wedge.clone()));
        wrapper.set_authorize_func(Some(Arc::new(ParseOnly))).await;
        let traces = Arc::new(Mutex::new(Vec::new()));
        let t = traces.clone();
        wrapper.set_trace_publisher(Arc::new(move |e| t.lock().unwrap().push(e))).await;
        Harness { wedge, wrapper, runtime, kernel, events, traces, _journal: journal }
    }

    fn calls(&self) -> Vec<String> {
        self.kernel.calls.lock().unwrap().clone()
    }

    fn outcomes(&self) -> Vec<(String, Outcome)> {
        self.events.lock().unwrap().iter().map(|e| (e.tag.as_ref().unwrap().fault_id.clone(), Outcome::try_from(e.outcome).unwrap())).collect()
    }

    /// The trace events of one call, by its trace_id: the request and its response.
    fn traced(&self, trace_id: &str) -> Vec<TraceEventKind> {
        self.traces.lock().unwrap().iter().filter(|e| e.trace_id == trace_id).map(|e| TraceEventKind::try_from(e.kind).unwrap()).collect()
    }

    async fn delay(&self, caller: &str, fault_id: &str, target: Target, latency_ms: u32, duration_s: u32) -> FaultReply {
        let req = CreateNetworkDelayRequest {
            tag: Some(tag(fault_id)),
            target: Some(target),
            latency_ms,
            jitter_ms: 10,
            correlation_pct: 25,
            duration_s,
            trace_id: format!("trace-{fault_id}"),
            ..Default::default()
        };
        self.wrapper.create_network_delay(signed(caller, req)).await.unwrap().into_inner()
    }
}

fn tag(fault_id: &str) -> FaultTag {
    FaultTag { fleet: FLEET.into(), round: "round-1".into(), fault_id: fault_id.into(), ..Default::default() }
}

fn target(namespace: &str, pod: &str) -> Target {
    Target { namespace: namespace.into(), pod: pod.into(), ..Default::default() }
}

async fn wait_for(mut done: impl FnMut() -> bool) {
    for _ in 0..100 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out");
}

// --- the tests -----------------------------------------------------------------------------------------

/// One network fault through the wedge: applied in the pod's netns, reverted by the wedge's own timer at
/// its end with no further call from the controller (the controller may be gone), and on the record
/// and in the trace throughout.
#[tokio::test]
async fn network_delay_is_applied_then_reverted_at_its_end_without_the_controller() {
    let h = Harness::start().await;
    let reply = h.delay("chaos-controller", "f1", target("tac-prod", "stratum-0"), 200, 1).await;
    assert_eq!(Outcome::try_from(reply.outcome).unwrap(), Outcome::Injected, "{}", reply.refusal);
    assert_eq!(reply.end_unix_ms - reply.start_unix_ms, 1000);
    assert_eq!(reply.target.as_ref().unwrap().container, "stratum", "the wedge names the container it resolved");
    assert_eq!(h.calls(), [format!("add_netem 101 {:?}", Netem::Delay { latency_ms: 200, jitter_ms: 10, correlation_pct: 25 })]);
    assert_eq!(h.wrapper.list_faults(signed("chaos-controller", ListFaultsRequest::default())).await.unwrap().into_inner().faults.len(), 1);

    // Nothing more is called: the controller died mid-fault. The wedge reverts it at its end.
    wait_for(|| h.kernel.netem.lock().unwrap().is_empty()).await;
    assert_eq!(h.calls()[1], "remove_netem 101");
    wait_for(|| h.outcomes().len() == 2).await;
    assert_eq!(h.outcomes(), [("f1".to_string(), Outcome::Injected), ("f1".to_string(), Outcome::Reverted)]);
    let events = h.events.lock().unwrap().clone();
    for e in &events {
        assert_eq!(e.fleet, FLEET);
        assert_eq!(e.namespace, "tac-prod");
        assert_eq!(e.tag.as_ref().unwrap().round, "round-1");
        assert_eq!(FaultKind::try_from(e.kind).unwrap(), FaultKind::NetworkDelay);
        assert_eq!(e.target.as_ref().unwrap().pod, "stratum-0");
        assert_eq!(e.service, "stratum");
        assert_eq!(e.node, NODE);
        assert_eq!(e.recorded_by, "chaos-wedge");
        assert_eq!(e.trace_id, "trace-f1", "the record carries the trace_id of the call that started it");
        assert!(e.start_unix_ms > 0 && e.end_unix_ms >= e.start_unix_ms);
    }
    assert_eq!(events[0].parameters.as_ref().unwrap().latency_ms, 200);
    assert_eq!(h.traced("trace-f1"), [TraceEventKind::InternalRequest, TraceEventKind::InternalResponse]);
    assert!(h.wrapper.list_faults(signed("chaos-controller", ListFaultsRequest::default())).await.unwrap().into_inner().faults.is_empty());
}

/// A wedge that crashed mid-fault leaves the fault in the node's journal; the next one reverts it
/// before it serves.
#[tokio::test]
async fn a_restarted_wedge_reverts_the_journal_before_it_serves() {
    let (runtime, namespaces, kernel) = node();
    let (runtime, namespaces, kernel) = (Arc::new(runtime), Arc::new(namespaces), Arc::new(kernel));
    let journal = Arc::new(tempfile::tempdir().unwrap());
    let first = Harness::start_on(runtime.clone(), namespaces.clone(), kernel.clone(), journal.clone()).await;
    let reply = first.delay("chaos-controller", "f2", target("tac-prod", "stratum-0"), 300, 300).await;
    assert_eq!(Outcome::try_from(reply.outcome).unwrap(), Outcome::Injected, "{}", reply.refusal);
    // The wedge dies: its timer goes with it, the netem and the journal entry stay.
    drop(first);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(kernel.netem.lock().unwrap().len(), 1);

    let second = Harness::start_on(runtime, namespaces, kernel.clone(), journal).await;
    assert!(kernel.netem.lock().unwrap().is_empty(), "reverted during configure, before serving");
    assert_eq!(second.outcomes(), [("f2".to_string(), Outcome::Reverted)]);
    assert_eq!(second.events.lock().unwrap()[0].trace_id, "trace-f2");
    // The journal is empty now: a third start reverts nothing.
    let third = Harness::start_on(second.runtime.clone(), Arc::new(node().1), kernel, second._journal.clone()).await;
    assert!(third.outcomes().is_empty());
}

/// The wedge refuses targets outside its fleet, excluded ones and ones not on its node, out-of-bound
/// parameters and callers other than the controller: nothing is applied, and each refusal is on the
/// record and in the trace with its reason.
#[tokio::test]
async fn the_wedge_refuses_out_of_fleet_excluded_and_out_of_bound() {
    let h = Harness::start().await;
    let cases: Vec<(&str, &str, Target, u32, u32, &str)> = vec![
        ("r-other-fleet", "chaos-controller", target("tac-dev", "stratum-0"), 100, 10, "not in fleet prod"),
        ("r-named-like", "chaos-controller", target("tac-prodx", "stratum-0"), 100, 10, "not in fleet prod"),
        ("r-letsencrypt", "chaos-controller", target("tac-prod", "letsencrypt-0"), 100, 10, "letsencrypt is excluded"),
        ("r-secret", "chaos-controller", target("tac-prod", "kmaker-0"), 100, 10, "mounts secret material"),
        ("r-elsewhere", "chaos-controller", target("tac-prod", "stratum-9"), 100, 10, "not running on this node"),
        ("r-system", "chaos-controller", target("kube-system", "coredns-0"), 100, 10, "kube-system is excluded"),
        ("r-latency", "chaos-controller", target("tac-prod", "stratum-0"), 2001, 10, "latency_ms 2001 is out of bound"),
        ("r-zero", "chaos-controller", target("tac-prod", "stratum-0"), 0, 10, "latency_ms 0 is out of bound"),
        ("r-duration", "chaos-controller", target("tac-prod", "stratum-0"), 100, 301, "duration_s 301 is out of bound"),
        ("r-caller", "stratum", target("tac-prod", "stratum-0"), 100, 10, "not chaos-controller"),
    ];
    for (id, caller, t, latency, duration, why) in &cases {
        let reply = h.delay(caller, id, t.clone(), *latency, *duration).await;
        assert_eq!(Outcome::try_from(reply.outcome).unwrap(), Outcome::Refused, "{id}");
        assert!(reply.refusal.contains(why), "{id}: {}", reply.refusal);
        assert_eq!(h.traced(&format!("trace-{id}")), [TraceEventKind::InternalRequest, TraceEventKind::InternalResponse], "{id}");
    }

    // Another fleet's tag, loss and stress out of bound.
    let mut other = tag("r-tag");
    other.fleet = "dev".into();
    let req = CreatePacketLossRequest { tag: Some(other), target: Some(target("tac-prod", "stratum-0")), loss_pct: 10, duration_s: 10, ..Default::default() };
    let reply = h.wrapper.create_packet_loss(signed("chaos-controller", req)).await.unwrap().into_inner();
    assert!(reply.refusal.contains("not this wedge's fleet"), "{}", reply.refusal);
    let req = CreatePacketLossRequest { tag: Some(tag("r-loss")), target: Some(target("tac-prod", "stratum-0")), loss_pct: 51, duration_s: 10, ..Default::default() };
    let reply = h.wrapper.create_packet_loss(signed("chaos-controller", req)).await.unwrap().into_inner();
    assert!(reply.refusal.contains("loss_pct 51"), "{}", reply.refusal);
    for (id, workers, memory, why) in [("r-cpu", 3, 0, "cpu_workers 3"), ("r-mem", 0, 51, "memory_pct_of_limit 51")] {
        let req = CreateStressRequest {
            tag: Some(tag(id)),
            target: Some(target("tac-prod", "stratum-0")),
            cpu_workers: workers,
            cpu_load_pct: if workers > 0 { 50 } else { 0 },
            memory_pct_of_limit: memory,
            duration_s: 10,
            ..Default::default()
        };
        let reply = h.wrapper.create_stress(signed("chaos-controller", req)).await.unwrap().into_inner();
        assert!(reply.refusal.contains(why), "{id}: {}", reply.refusal);
    }
    let req = CreateNetworkDelayRequest { tag: Some(tag("../../etc/x")), target: Some(target("tac-prod", "stratum-0")), latency_ms: 10, duration_s: 10, ..Default::default() };
    assert!(h.wrapper.create_network_delay(signed("chaos-controller", req)).await.unwrap().into_inner().refusal.contains("not a plain token"));

    assert!(h.calls().is_empty(), "nothing was applied: {:?}", h.calls());
    assert!(h.runtime.stopped.lock().unwrap().is_empty());
    let events = h.events.lock().unwrap().clone();
    assert_eq!(events.len(), cases.len() + 5);
    assert!(events.iter().all(|e| Outcome::try_from(e.outcome).unwrap() == Outcome::Refused && !e.refusal.is_empty()));
}

/// A tenant namespace of the fleet is the fleet's; a container kill is one-shot, through the runtime.
#[tokio::test]
async fn container_kill_in_a_tenant_namespace() {
    let h = Harness::start().await;
    let req = CreateContainerKillRequest { tag: Some(tag("k1")), target: Some(target("tac-prod-a1b2c3d4", "people-0")), trace_id: "trace-k1".into(), ..Default::default() };
    let reply = h.wrapper.create_container_kill(signed("chaos-controller", req)).await.unwrap().into_inner();
    assert_eq!(Outcome::try_from(reply.outcome).unwrap(), Outcome::Injected, "{}", reply.refusal);
    assert_eq!(reply.end_unix_ms, 0, "one-shot");
    assert_eq!(*h.runtime.stopped.lock().unwrap(), ["cid-people-0"]);
    assert_eq!(h.outcomes(), [("k1".to_string(), Outcome::Injected)]);
    assert_eq!(h.traced("trace-k1").len(), 2);
}

/// Stress runs in the container's cgroup, sized from its limits, and ends at its end.
#[tokio::test]
async fn stress_runs_in_the_containers_cgroup_and_ends() {
    let h = Harness::start().await;
    let req = CreateStressRequest {
        tag: Some(tag("s1")),
        target: Some(target("tac-prod", "stratum-1")),
        cpu_workers: 2,
        cpu_load_pct: 50,
        memory_pct_of_limit: 10,
        duration_s: 1,
        ..Default::default()
    };
    let reply = h.wrapper.create_stress(signed("chaos-controller", req)).await.unwrap().into_inner();
    assert_eq!(Outcome::try_from(reply.outcome).unwrap(), Outcome::Injected, "{}", reply.refusal);
    let spec = StressSpec { cpu_workers: 2, cpu_load_pct: 50, memory_bytes: (1u64 << 30) / 100 * 10, duration_s: 1 };
    assert_eq!(h.calls(), [format!("start_stress /sys/fs/cgroup/kubepods/stratum-1 {spec:?}")]);
    wait_for(|| h.kernel.stress.lock().unwrap().is_empty()).await;
    wait_for(|| h.outcomes().len() == 2).await;
    assert_eq!(h.outcomes()[1], ("s1".to_string(), Outcome::Reverted));
}

/// At most one live fault per pod and four per node; DeleteFault reverts at once, and twice is success.
#[tokio::test]
async fn one_fault_per_pod_four_per_node_and_early_revert() {
    let h = Harness::start().await;
    let ok = |r: &FaultReply| Outcome::try_from(r.outcome).unwrap() == Outcome::Injected;
    assert!(ok(&h.delay("chaos-controller", "a", target("tac-prod", "worker-0"), 10, 300).await));
    let again = h.delay("chaos-controller", "b", target("tac-prod", "worker-0"), 10, 300).await;
    assert!(again.refusal.contains("already has a live fault"), "{}", again.refusal);
    for (id, p) in [("c", "worker-1"), ("d", "worker-2"), ("e", "worker-3")] {
        assert!(ok(&h.delay("chaos-controller", id, target("tac-prod", p), 10, 300).await));
    }
    let fifth = h.delay("chaos-controller", "f", target("tac-prod", "stratum-0"), 10, 300).await;
    assert!(fifth.refusal.contains("4 live faults"), "{}", fifth.refusal);

    let del = DeleteFaultRequest { tag: Some(tag("a")), ..Default::default() };
    let reverted = h.wrapper.delete_fault(signed("chaos-controller", del.clone())).await.unwrap().into_inner();
    assert_eq!(Outcome::try_from(reverted.outcome).unwrap(), Outcome::Reverted);
    assert!(!h.kernel.netem.lock().unwrap().contains_key(&108));
    let twice = h.wrapper.delete_fault(signed("chaos-controller", del.clone())).await.unwrap().into_inner();
    assert_eq!(Outcome::try_from(twice.outcome).unwrap(), Outcome::Reverted);
    assert_eq!(h.wrapper.delete_fault(signed("stratum", del)).await.unwrap_err().code(), tonic::Code::PermissionDenied);

    h.wedge.revert_all("the test's end").await;
    assert!(h.kernel.netem.lock().unwrap().is_empty());
}

/// A fault whose pod is gone by its end (its netns with it) is recorded reverted, target gone.
#[tokio::test]
async fn a_pod_gone_before_the_end_is_reverted_target_gone() {
    let h = Harness::start().await;
    assert_eq!(Outcome::try_from(h.delay("chaos-controller", "g", target("tac-prod", "stratum-0"), 10, 1).await.outcome).unwrap(), Outcome::Injected);
    h.kernel.inodes.lock().unwrap().remove(&101);
    wait_for(|| h.outcomes().len() == 2).await;
    assert_eq!(h.outcomes()[1], ("g".to_string(), Outcome::Reverted));
}

/// The fleet is the first Config's and never changes.
#[tokio::test]
async fn the_fleet_never_changes() {
    let h = Harness::start().await;
    let err = h.wedge.configure(&Config { fleet: "dev".into(), ..Default::default() }).await.unwrap_err();
    assert!(err.contains("serves one fleet"), "{err}");
}

/// A revert that fails is not dropped: the fault stays live and journalled, and the wedge tries again
/// until it is gone; it is recorded once, when it is reverted.
#[tokio::test]
async fn a_failed_revert_is_tried_again() {
    let h = Harness::start().await;
    *h.kernel.failing_removes.lock().unwrap() = 1;
    assert_eq!(Outcome::try_from(h.delay("chaos-controller", "x", target("tac-prod", "stratum-0"), 10, 1).await.outcome).unwrap(), Outcome::Injected);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(h.calls().len(), 2, "the first revert ran and failed: {:?}", h.calls());
    assert_eq!(h.kernel.netem.lock().unwrap().len(), 1);
    assert_eq!(h.wrapper.list_faults(signed("chaos-controller", ListFaultsRequest::default())).await.unwrap().into_inner().faults.len(), 1);
    for _ in 0..200 {
        if h.kernel.netem.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(h.kernel.netem.lock().unwrap().is_empty(), "tried again and reverted");
    assert_eq!(h.outcomes(), [("x".to_string(), Outcome::Injected), ("x".to_string(), Outcome::Reverted)]);
}

/// The shutdown reverts what is live and applies nothing after it.
#[tokio::test]
async fn nothing_is_applied_after_the_shutdown() {
    let h = Harness::start().await;
    assert_eq!(Outcome::try_from(h.delay("chaos-controller", "s1", target("tac-prod", "stratum-0"), 10, 300).await.outcome).unwrap(), Outcome::Injected);
    h.wedge.revert_all("the wedge's shutdown").await;
    assert!(h.kernel.netem.lock().unwrap().is_empty());
    let late = h.delay("chaos-controller", "s2", target("tac-prod", "stratum-1"), 10, 300).await;
    assert!(late.refusal.contains("shutting down"), "{}", late.refusal);
    assert!(h.kernel.netem.lock().unwrap().is_empty());
}

/// The fleet names the journal's directory, so it must be a deployment name.
#[tokio::test]
async fn the_fleet_must_be_a_deployment_name() {
    let (runtime, namespaces, kernel) = node();
    let dir = tempfile::tempdir().unwrap();
    let wedge = Wedge::new(NODE.into(), dir.path().to_path_buf(), Arc::new(runtime), Arc::new(namespaces), Arc::new(kernel), Arc::new(|_| {}));
    for bad in ["", "../x", "/abs", "Prod", "a/b"] {
        assert!(wedge.configure(&Config { fleet: bad.into(), ..Default::default() }).await.is_err(), "{bad:?}");
    }
}
