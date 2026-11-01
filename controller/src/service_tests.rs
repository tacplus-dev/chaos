// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! The controller's acceptance tests (#1): rounds go in through the GENERATED `ValidationWrapper` with a
//! trace_id and the generated trace publisher wired to a list; the cluster is fake, and the wedge is a
//! fake served over real gRPC with the generated server and client, so the controller's wedge calls
//! cross the wire as they do in a fleet.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chaoscontroller_model::s2sm::internal::grpc::servers::services::chaoscontroller::controller::{Authorizer, ValidationWrapper};
use chaoscontroller_model::types::services::chaoscontroller::chaoscontroller::controller_server::Controller as ControllerRpc;
use chaoscontroller_model::types::services::chaoscontroller::chaoscontroller::{Config, CreateRoundRequest, ListRoundsRequest, Round, UpdateScheduleRequest};
use chaoscontroller_model::types::services::chaosv1::{FaultEvent, FaultKind, FaultReply, Outcome};
use chaoscontroller_model::types::services::chaoswedge::chaoswedge::wedge_server::{Wedge as WedgeRpc, WedgeServer};
use chaoscontroller_model::types::services::chaoswedge::chaoswedge::{
    CreateContainerKillRequest, CreateNetworkDelayRequest, CreatePacketLossRequest, CreateStressRequest, DeleteFaultRequest, ListFaultsRequest,
    ListFaultsResponse,
};
use chaoscontroller_model::types::sys::trace::{TraceEvent, TraceEventKind};
use tonic::transport::Channel;

use crate::kube::{Cluster, PodInfo};
use crate::service::{now_ms, Controller};
use crate::wedges::{Dial, Wedges};

const FLEET: &str = "prod";
const NS: &str = "tac-prod";

// --- the fake cluster ----------------------------------------------------------------------------------

#[derive(Default)]
struct FakeCluster {
    pods: Mutex<BTreeMap<String, Vec<PodInfo>>>,
    deleted: Mutex<Vec<String>>,
    events: Mutex<Vec<(String, String)>>,
    /// Whether a deleted pod's ReplicaSet brings a new Ready one.
    replaces: Mutex<bool>,
}

fn p(name: &str, service: &str, node: &str) -> PodInfo {
    PodInfo { namespace: NS.into(), name: name.into(), service: service.into(), node: node.into(), ip: "10.0.0.1".into(), running: true, ready: true, mounts_secret: false }
}

impl FakeCluster {
    /// stratum (2 Ready pods) is the only eligible service: solo has one pod, worker one Ready of two,
    /// kmaker mounts a Secret, letsencrypt and the chaos services are excluded.
    fn fleet() -> FakeCluster {
        let mut kmaker = [p("kmaker-0", "kmaker", "n1"), p("kmaker-1", "kmaker", "n2")];
        for k in &mut kmaker {
            k.mounts_secret = true;
        }
        let mut worker1 = p("worker-1", "worker", "n2");
        worker1.ready = false;
        let mut pods = vec![
            p("stratum-0", "stratum", "n1"),
            p("stratum-1", "stratum", "n2"),
            p("solo-0", "solo", "n1"),
            p("worker-0", "worker", "n1"),
            worker1,
            p("letsencrypt-0", "letsencrypt", "n1"),
            p("letsencrypt-1", "letsencrypt", "n2"),
            p("chaos-wedge-a", "chaos-wedge", "n1"),
            p("chaos-wedge-b", "chaos-wedge", "n2"),
            p("chaos-controller-0", "chaos-controller", "n1"),
            p("chaos-controller-1", "chaos-controller", "n2"),
        ];
        pods.extend(kmaker);
        FakeCluster { pods: Mutex::new(BTreeMap::from([(NS.to_string(), pods)])), replaces: Mutex::new(true), ..Default::default() }
    }
}

#[async_trait]
impl Cluster for FakeCluster {
    async fn list_pods(&self, namespace: &str) -> Result<Vec<PodInfo>, String> {
        Ok(self.pods.lock().unwrap().get(namespace).cloned().unwrap_or_default())
    }

    async fn delete_pod(&self, namespace: &str, name: &str, _grace_period_s: u32) -> Result<(), String> {
        self.deleted.lock().unwrap().push(name.into());
        let mut pods = self.pods.lock().unwrap();
        let list = pods.get_mut(namespace).ok_or("no namespace")?;
        let i = list.iter().position(|x| x.name == name).ok_or("no pod")?;
        let mut old = list.remove(i);
        if *self.replaces.lock().unwrap() {
            old.name = format!("{name}-new");
            list.push(old);
        }
        Ok(())
    }

    async fn create_event(&self, _namespace: &str, pod: &str, message: &str) -> Result<(), String> {
        self.events.lock().unwrap().push((pod.into(), message.into()));
        Ok(())
    }
}

// --- the fake wedge, over gRPC -------------------------------------------------------------------------

#[derive(Default)]
struct FakeWedge {
    calls: Mutex<Vec<(String, String, String)>>, // (rpc, fault_id, trace_id)
    delays: Mutex<Vec<CreateNetworkDelayRequest>>,
    /// How long the faults it injects last.
    lasts_ms: Mutex<i64>,
    refuse: Mutex<bool>,
}

impl FakeWedge {
    fn reply(&self, rpc: &str, tag: Option<chaoscontroller_model::types::services::chaosv1::FaultTag>, trace_id: String, kind: FaultKind) -> FaultReply {
        let tag = tag.unwrap_or_default();
        self.calls.lock().unwrap().push((rpc.into(), tag.fault_id.clone(), trace_id));
        let now = now_ms();
        let refused = *self.refuse.lock().unwrap();
        FaultReply {
            tag: Some(tag),
            kind: kind as i32,
            start_unix_ms: now,
            end_unix_ms: now + *self.lasts_ms.lock().unwrap(),
            outcome: if refused { Outcome::Refused } else { Outcome::Injected } as i32,
            refusal: if refused { "pod tac-prod/x mounts secret material".into() } else { String::new() },
            ..Default::default()
        }
    }
}

/// The fake wedge as a gRPC service.
struct WedgeSvc(Arc<FakeWedge>);

#[tonic::async_trait]
impl WedgeRpc for WedgeSvc {
    async fn create_network_delay(&self, r: tonic::Request<CreateNetworkDelayRequest>) -> Result<tonic::Response<FaultReply>, tonic::Status> {
        let r = r.into_inner();
        self.0.delays.lock().unwrap().push(r.clone());
        Ok(tonic::Response::new(self.0.reply("CreateNetworkDelay", r.tag, r.trace_id, FaultKind::NetworkDelay)))
    }
    async fn create_packet_loss(&self, r: tonic::Request<CreatePacketLossRequest>) -> Result<tonic::Response<FaultReply>, tonic::Status> {
        let r = r.into_inner();
        Ok(tonic::Response::new(self.0.reply("CreatePacketLoss", r.tag, r.trace_id, FaultKind::PacketLoss)))
    }
    async fn create_container_kill(&self, r: tonic::Request<CreateContainerKillRequest>) -> Result<tonic::Response<FaultReply>, tonic::Status> {
        let r = r.into_inner();
        let mut reply = self.0.reply("CreateContainerKill", r.tag, r.trace_id, FaultKind::ContainerKill);
        reply.end_unix_ms = 0;
        Ok(tonic::Response::new(reply))
    }
    async fn create_stress(&self, r: tonic::Request<CreateStressRequest>) -> Result<tonic::Response<FaultReply>, tonic::Status> {
        let r = r.into_inner();
        Ok(tonic::Response::new(self.0.reply("CreateStress", r.tag, r.trace_id, FaultKind::Stress)))
    }
    async fn delete_fault(&self, r: tonic::Request<DeleteFaultRequest>) -> Result<tonic::Response<FaultReply>, tonic::Status> {
        let r = r.into_inner();
        let mut reply = self.0.reply("DeleteFault", r.tag, r.trace_id, FaultKind::Unspecified);
        reply.outcome = Outcome::Reverted as i32;
        Ok(tonic::Response::new(reply))
    }
    async fn list_faults(&self, _r: tonic::Request<ListFaultsRequest>) -> Result<tonic::Response<ListFaultsResponse>, tonic::Status> {
        Ok(tonic::Response::new(ListFaultsResponse::default()))
    }
}

/// Dials the fake wedge, recording which node each call was for.
struct TestDial {
    url: String,
    nodes: Mutex<Vec<String>>,
}

#[async_trait]
impl Dial for TestDial {
    async fn dial(&self, node: &str) -> Result<(Channel, Option<String>), String> {
        self.nodes.lock().unwrap().push(node.into());
        let channel = Channel::from_shared(self.url.clone()).unwrap().connect().await.map_err(|e| e.to_string())?;
        Ok((channel, None))
    }
}

async fn serve(wedge: Arc<FakeWedge>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(WedgeServer::new(WedgeSvc(wedge)))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    url
}

// --- auth and the harness ------------------------------------------------------------------------------

struct ParseOnly;

#[tonic::async_trait]
impl Authorizer for ParseOnly {
    async fn authorize(&self, _m: &str, a: Option<&str>) -> Result<HashMap<String, String>, tonic::Status> {
        a.map(|_| HashMap::new()).ok_or_else(|| tonic::Status::unauthenticated("no token"))
    }
    async fn check_at_or_below(&self, m: &str, a: Option<&str>, _t: &str) -> Result<HashMap<String, String>, tonic::Status> {
        self.authorize(m, a).await
    }
    async fn authenticate(&self, m: &str, a: Option<&str>) -> Result<HashMap<String, String>, tonic::Status> {
        self.authorize(m, a).await
    }
}

fn signed<T>(req: T) -> tonic::Request<T> {
    let key = key::keys::generate_ed25519_key().unwrap();
    let mut t = token::token::Token::new();
    t.set_issuer("chaos-controller-tests").unwrap();
    t.set_sub_type("service").unwrap();
    t.set_subject("spiffe://td.test/system/services/seatrials").unwrap();
    t.set_namespace("/system").unwrap();
    t.set_expiration(now_ms() / 1000 + 3600).unwrap();
    t.sign(&key).unwrap();
    let mut request = tonic::Request::new(req);
    request.metadata_mut().insert("authorization", format!("Bearer {}", t.base64().unwrap()).parse().unwrap());
    request
}

struct Harness {
    controller: Controller,
    wrapper: ValidationWrapper,
    cluster: Arc<FakeCluster>,
    wedge: Arc<FakeWedge>,
    dial: Arc<TestDial>,
    events: Arc<Mutex<Vec<FaultEvent>>>,
    traces: Arc<Mutex<Vec<TraceEvent>>>,
}

impl Harness {
    async fn start(network: bool) -> Harness {
        let cluster = Arc::new(FakeCluster::fleet());
        let wedge = Arc::new(FakeWedge::default());
        *wedge.lasts_ms.lock().unwrap() = 100;
        let dial = Arc::new(TestDial { url: serve(wedge.clone()).await, nodes: Mutex::default() });
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let controller = Controller::new(cluster.clone(), Wedges::new(dial.clone()), Arc::new(move |e| sink.lock().unwrap().push(e)), Duration::from_millis(20));
        controller
            .configure(&Config {
                fleet: FLEET.into(),
                namespaces: vec![NS.into()],
                interval_s: 900,
                level_pods: true,
                level_network: network,
                recovery_timeout_s: 1,
                exclude_services: vec![],
                wedge_port: 8443,
                ..Default::default()
            })
            .unwrap();
        let wrapper = ValidationWrapper::new(Arc::new(controller.clone()));
        wrapper.set_authorize_func(Some(Arc::new(ParseOnly))).await;
        let traces = Arc::new(Mutex::new(Vec::new()));
        let t = traces.clone();
        wrapper.set_trace_publisher(Arc::new(move |e| t.lock().unwrap().push(e))).await;
        Harness { controller, wrapper, cluster, wedge, dial, events, traces }
    }

    async fn round(&self, kind: FaultKind, trace_id: &str) -> Result<Round, tonic::Status> {
        let req = CreateRoundRequest { kind: kind as i32, trace_id: trace_id.into(), ..Default::default() };
        self.wrapper.create_round(signed(req)).await.map(|r| r.into_inner())
    }

    fn outcomes(&self, round: &str) -> Vec<Outcome> {
        self.events.lock().unwrap().iter().filter(|e| e.tag.as_ref().unwrap().round == round).map(|e| Outcome::try_from(e.outcome).unwrap()).collect()
    }

    fn traced(&self, trace_id: &str) -> Vec<TraceEventKind> {
        self.traces.lock().unwrap().iter().filter(|e| e.trace_id == trace_id).map(|e| TraceEventKind::try_from(e.kind).unwrap()).collect()
    }

    async fn wait_outcomes(&self, round: &str, n: usize) {
        for _ in 0..200 {
            if self.outcomes(round).len() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("round {round}: {:?}", self.outcomes(round));
    }
}

// --- the tests -----------------------------------------------------------------------------------------

/// Pod kills: each round deletes exactly one pod of the fleet, never of an excluded, secret-holding,
/// single-replica or not fully Ready service; it marks the target with an Event, and the round is on the
/// record (injected, then recovered) with its tag and in the trace.
#[tokio::test]
async fn pod_kills_delete_one_eligible_pod_and_recover() {
    let h = Harness::start(false).await;
    for i in 0..6 {
        let trace_id = format!("trace-{i}");
        let round = h.round(FaultKind::Unspecified, &trace_id).await.unwrap();
        assert!(round.skipped.is_empty(), "{}", round.skipped);
        assert_eq!(round.service, "stratum");
        let fault = round.fault.as_ref().unwrap();
        assert_eq!(FaultKind::try_from(fault.kind).unwrap(), FaultKind::PodKill);
        assert_eq!(h.cluster.deleted.lock().unwrap().len(), i + 1, "exactly one pod per round");
        h.wait_outcomes(&round.round, 2).await;
        assert_eq!(h.outcomes(&round.round), [Outcome::Injected, Outcome::Recovered]);
        assert_eq!(h.traced(&trace_id), [TraceEventKind::InternalRequest, TraceEventKind::InternalResponse]);
        let events: Vec<FaultEvent> = h.events.lock().unwrap().iter().filter(|e| e.tag.as_ref().unwrap().round == round.round).cloned().collect();
        for e in &events {
            assert_eq!(e.fleet, FLEET);
            assert_eq!(e.namespace, NS);
            assert_eq!(e.service, "stratum");
            assert_eq!(e.recorded_by, "chaos-controller");
            assert_eq!(e.trace_id, trace_id);
            assert_eq!(e.tag.as_ref().unwrap().fault_id, format!("{}-1", round.round));
            assert!(e.target.as_ref().unwrap().pod.starts_with("stratum-"));
        }
        let (pod, marker) = h.cluster.events.lock().unwrap().last().cloned().unwrap();
        assert_eq!(pod, h.cluster.deleted.lock().unwrap().last().cloned().unwrap());
        assert!(marker.contains(&format!("round={}", round.round)) && marker.contains("fleet=prod"), "{marker}");
    }
    for d in h.cluster.deleted.lock().unwrap().iter() {
        assert!(d.starts_with("stratum-"), "deleted {d}");
    }
    assert!(h.wedge.calls.lock().unwrap().is_empty(), "a pod kill does not go through the wedge");
    let rounds = h.wrapper.list_rounds(signed(ListRoundsRequest::default())).await.unwrap().into_inner().rounds;
    assert_eq!(rounds.len(), 6);
    assert!(rounds.iter().all(|r| Outcome::try_from(r.recovery).unwrap() == Outcome::Recovered));
}

/// A target that does not recover is an incident on the record, and no new round starts until it has.
#[tokio::test]
async fn not_recovered_stops_the_next_round() {
    let h = Harness::start(false).await;
    *h.cluster.replaces.lock().unwrap() = false;
    let first = h.round(FaultKind::PodKill, "t1").await.unwrap();
    h.wait_outcomes(&first.round, 2).await;
    assert_eq!(h.outcomes(&first.round), [Outcome::Injected, Outcome::NotRecovered]);
    let second = h.round(FaultKind::PodKill, "t2").await.unwrap();
    assert!(second.skipped.contains("has not recovered"), "{}", second.skipped);
    assert_eq!(h.cluster.deleted.lock().unwrap().len(), 1);
    // The service comes back: rounds start again.
    h.cluster.pods.lock().unwrap().get_mut(NS).unwrap().push(p("stratum-9", "stratum", "n1"));
    let third = h.round(FaultKind::PodKill, "t3").await.unwrap();
    assert!(third.skipped.is_empty(), "{}", third.skipped);
}

/// A network fault goes through the wedge on the target's node, tagged and with the round's trace_id,
/// with the first release's parameters; the round's recovery is checked once the fault is over.
#[tokio::test]
async fn network_fault_goes_through_the_wedge_on_the_targets_node() {
    let h = Harness::start(true).await;
    let round = h.round(FaultKind::NetworkDelay, "trace-net").await.unwrap();
    assert!(round.skipped.is_empty(), "{}", round.skipped);
    let delays = h.wedge.delays.lock().unwrap().clone();
    assert_eq!(delays.len(), 1);
    let req = &delays[0];
    let tag = req.tag.as_ref().unwrap();
    assert_eq!((tag.fleet.as_str(), tag.round.as_str()), (FLEET, round.round.as_str()));
    assert_eq!(req.trace_id, "trace-net", "the wedge call carries the round's trace_id");
    assert_eq!((req.latency_ms, req.jitter_ms, req.duration_s), (200, 50, 60));
    let pod = req.target.as_ref().unwrap().pod.clone();
    let node = if pod == "stratum-0" { "n1" } else { "n2" };
    assert_eq!(*h.dial.nodes.lock().unwrap(), [node]);
    assert!(h.cluster.deleted.lock().unwrap().is_empty());
    h.wait_outcomes(&round.round, 1).await;
    assert_eq!(h.outcomes(&round.round), [Outcome::Recovered]);
    assert_eq!(h.traced("trace-net").len(), 2);
}

/// The pause reverts every live wedge fault through the wedge, and no round runs while paused.
#[tokio::test]
async fn pause_reverts_live_faults_and_stops_rounds() {
    let h = Harness::start(true).await;
    *h.wedge.lasts_ms.lock().unwrap() = 300_000;
    let round = h.round(FaultKind::PacketLoss, "t-loss").await.unwrap();
    assert!(round.skipped.is_empty(), "{}", round.skipped);
    let paused = h.wrapper.update_schedule(signed(UpdateScheduleRequest { paused: true, ..Default::default() })).await.unwrap().into_inner();
    assert!(paused.paused);
    assert_eq!(paused.reverted.len(), 1);
    let calls = h.wedge.calls.lock().unwrap().clone();
    assert_eq!(calls.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), ["CreatePacketLoss", "DeleteFault"]);
    assert_eq!(calls[1].1, format!("{}-1", round.round));
    let skipped = h.round(FaultKind::PodKill, "t-paused").await.unwrap();
    assert!(skipped.skipped.contains("paused"), "{}", skipped.skipped);
    assert!(!h.controller.update_schedule(false).await.paused);
}

/// A kind outside the levels switched on is refused; a refusal by the wedge applies nothing and leaves
/// nothing to recover from.
#[tokio::test]
async fn levels_and_wedge_refusals() {
    let h = Harness::start(false).await;
    let err = h.round(FaultKind::NetworkDelay, "t").await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    *h.wedge.refuse.lock().unwrap() = true;
    let round = h.round(FaultKind::ContainerKill, "t-kill").await.unwrap();
    assert!(round.skipped.starts_with("refused:"), "{}", round.skipped);
    // Nothing pending: the next round runs.
    *h.wedge.refuse.lock().unwrap() = false;
    assert!(h.round(FaultKind::PodKill, "t-next").await.unwrap().skipped.is_empty());
}
