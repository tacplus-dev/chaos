// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! The controller's rounds (proto/services/controller.proto). A round is one fault on one target:
//!
//! 1. nothing while paused, or while the last round's target has not recovered (one fault at a time per
//!    fleet, so triage maps a failure to exactly one fault);
//! 2. a kind among the levels the Config switches on (pods by default; network and stress opt-in);
//! 3. a target at random among the fleet's Running and Ready pods, never an excluded one, one that
//!    mounts a Secret, or one of a service with fewer than 2 Ready pods;
//! 4. a Kubernetes Event on the target with the fault's tag, then the fault: a pod kill through the
//!    Kubernetes API, anything else through the wedge on the target's node;
//! 5. the recovery check: once the fault is over, the service must be back to as many Ready pods as it
//!    had within the recovery bound: RECOVERED, else NOT_RECOVERED (an incident).
//!
//! Every step's outcome is a `chaosv1.FaultEvent` on the record, with the round's trace_id.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chaos_policy::bounds::Bounds;
use chaos_policy::exclusion::{Exclusions, PodFacts, DEPLOYMENT_LABEL};
use chaoscontroller_model::s2sm::internal::grpc::servers::services::chaoscontroller::controller::ControllerHandler;
use chaoscontroller_model::types::services::chaoscontroller::chaoscontroller::{
    Config, CreateRoundRequest, ListRoundsRequest, ListRoundsResponse, Round, ScheduleResponse, UpdateScheduleRequest,
};
use chaoscontroller_model::types::services::chaosv1::{FaultEvent, FaultKind, FaultParameters, FaultReply, FaultTag, Outcome, Target};
use chaoscontroller_model::types::services::chaoswedge::chaoswedge::{
    CreateContainerKillRequest, CreateNetworkDelayRequest, CreatePacketLossRequest, CreateStressRequest, DeleteFaultRequest,
};
use rand::seq::SliceRandom;
use servicemanager::auth::Caller;

use crate::kube::{Cluster, PodInfo};
use crate::wedges::{WedgeCall, Wedges};

pub type EventSink = Arc<dyn Fn(FaultEvent) + Send + Sync>;

/// The rounds kept for ListRounds.
const KEPT_ROUNDS: usize = 64;

/// The first release's fault parameters, well inside the compiled-in bounds (#1 Q4).
pub fn default_parameters(kind: FaultKind) -> FaultParameters {
    match kind {
        FaultKind::NetworkDelay => FaultParameters { latency_ms: 200, jitter_ms: 50, correlation_pct: 25, duration_s: 60, ..Default::default() },
        FaultKind::PacketLoss => FaultParameters { loss_pct: 10, duration_s: 60, ..Default::default() },
        // One worker: within any container's CPU limit (the wedge checks it against the real one).
        FaultKind::Stress => FaultParameters { cpu_workers: 1, cpu_load_pct: 80, duration_s: 60, ..Default::default() },
        _ => FaultParameters::default(),
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}

#[derive(Clone, Debug, Default)]
struct Settings {
    fleet: String,
    namespaces: Vec<String>,
    interval_s: u32,
    level_pods: bool,
    level_network: bool,
    level_stress: bool,
    config_paused: bool,
    recovery_timeout_s: u32,
    exclusions: Exclusions,
}

/// A round whose recovery is not RECOVERED yet.
#[derive(Clone, Debug)]
struct Pending {
    round: String,
    namespace: String,
    service: String,
    ready_before: usize,
    /// Set once the check ran out its bound: the next round checks again before it starts.
    not_recovered: bool,
}

struct Inner {
    cluster: Arc<dyn Cluster>,
    wedges: Wedges,
    events: EventSink,
    settings: Mutex<Settings>,
    /// Paused through UpdateSchedule (the Config can pause too).
    api_paused: Mutex<bool>,
    rounds: Mutex<VecDeque<Round>>,
    /// Live wedge faults, by fault id: their node and tag, for the pause's revert.
    live: Mutex<BTreeMap<String, (String, FaultTag)>>,
    pending: Mutex<Option<Pending>>,
    one_round: tokio::sync::Mutex<()>,
    counter: AtomicU64,
    /// How often the recovery check looks.
    poll: Duration,
}

#[derive(Clone)]
pub struct Controller {
    inner: Arc<Inner>,
}

impl Controller {
    pub fn new(cluster: Arc<dyn Cluster>, wedges: Wedges, events: EventSink, poll: Duration) -> Controller {
        Controller {
            inner: Arc::new(Inner {
                cluster,
                wedges,
                events,
                settings: Mutex::new(Settings::default()),
                api_paused: Mutex::new(false),
                rounds: Mutex::new(VecDeque::new()),
                live: Mutex::new(BTreeMap::new()),
                pending: Mutex::new(None),
                one_round: tokio::sync::Mutex::new(()),
                counter: AtomicU64::new(0),
                poll,
            }),
        }
    }

    pub fn configure(&self, cfg: &Config) -> Result<(), String> {
        if cfg.fleet.is_empty() || cfg.namespaces.is_empty() {
            return Err("Config.fleet and Config.namespaces are required".into());
        }
        *self.inner.settings.lock().unwrap() = Settings {
            fleet: cfg.fleet.clone(),
            namespaces: cfg.namespaces.clone(),
            interval_s: cfg.interval_s,
            level_pods: cfg.level_pods,
            level_network: cfg.level_network,
            level_stress: cfg.level_stress,
            config_paused: cfg.paused,
            recovery_timeout_s: cfg.recovery_timeout_s,
            exclusions: Exclusions::with(&[], &cfg.exclude_services),
        };
        Ok(())
    }

    fn settings(&self) -> Settings {
        self.inner.settings.lock().unwrap().clone()
    }

    pub fn paused(&self) -> bool {
        self.settings().config_paused || *self.inner.api_paused.lock().unwrap()
    }

    /// The time between scheduled rounds.
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.settings().interval_s.max(60) as u64)
    }

    /// The schedule: a round every interval, until the process ends.
    pub async fn schedule(self) {
        loop {
            tokio::time::sleep(self.interval()).await;
            if self.settings().fleet.is_empty() || self.paused() {
                continue;
            }
            match self.run_round(FaultKind::Unspecified, String::new()).await {
                Ok(r) if !r.skipped.is_empty() => tracing::info!(round = %r.round, skipped = %r.skipped, "round skipped"),
                Ok(r) => tracing::info!(round = %r.round, service = %r.service, "round"),
                Err(e) => tracing::warn!(error = %e.message(), "round failed"),
            }
        }
    }

    fn emit(&self, tag: &FaultTag, kind: FaultKind, pod: &PodInfo, params: &FaultParameters, outcome: Outcome, start: i64, end: i64, trace_id: &str) {
        let event = FaultEvent {
            event_id: format!("{}-{}-{}", tag.fault_id, outcome.as_str_name().trim_start_matches("OUTCOME_").to_lowercase(), now_ms()),
            fleet: tag.fleet.clone(),
            namespace: pod.namespace.clone(),
            tag: Some(tag.clone()),
            kind: kind as i32,
            target: Some(Target { namespace: pod.namespace.clone(), pod: pod.name.clone(), ..Default::default() }),
            service: pod.service.clone(),
            parameters: Some(params.clone()),
            outcome: outcome as i32,
            start_unix_ms: start,
            end_unix_ms: end,
            recorded_by: chaos_policy::CONTROLLER.into(),
            node: pod.node.clone(),
            recorded_unix_ms: now_ms(),
            trace_id: trace_id.into(),
            ..Default::default()
        };
        (self.inner.events)(event);
    }

    fn remember(&self, round: Round) -> Round {
        let mut rounds = self.inner.rounds.lock().unwrap();
        rounds.retain(|r| r.round != round.round);
        rounds.push_front(round.clone());
        rounds.truncate(KEPT_ROUNDS);
        round
    }

    fn skipped(&self, round: &str, why: String) -> Round {
        self.remember(Round { round: round.into(), skipped: why, ..Default::default() })
    }

    async fn ready_count(&self, namespace: &str, service: &str) -> Result<usize, String> {
        Ok(self.inner.cluster.list_pods(namespace).await?.iter().filter(|p| p.service == service && p.running && p.ready).count())
    }

    /// Whether the last round's target has recovered (or there was none).
    async fn recovered_since(&self) -> Result<(), String> {
        let Some(p) = self.inner.pending.lock().unwrap().clone() else { return Ok(()) };
        if !p.not_recovered {
            return Err(format!("round {} is not over: {} has not been checked yet", p.round, p.service));
        }
        if self.ready_count(&p.namespace, &p.service).await? >= p.ready_before {
            *self.inner.pending.lock().unwrap() = None;
            return Ok(());
        }
        Err(format!("service {} has not recovered from round {}", p.service, p.round))
    }

    /// The kinds the levels switch on, and those a round chooses from when none is asked for.
    fn kinds(s: &Settings) -> (Vec<FaultKind>, Vec<FaultKind>) {
        let (mut allowed, mut chosen) = (Vec::new(), Vec::new());
        if s.level_pods {
            allowed.extend([FaultKind::PodKill, FaultKind::ContainerKill]);
            chosen.push(FaultKind::PodKill);
        }
        if s.level_network {
            allowed.extend([FaultKind::NetworkDelay, FaultKind::PacketLoss]);
            chosen.extend([FaultKind::NetworkDelay, FaultKind::PacketLoss]);
        }
        if s.level_stress {
            allowed.push(FaultKind::Stress);
            chosen.push(FaultKind::Stress);
        }
        (allowed, chosen)
    }

    /// The fleet's pods a fault may target, with their service's Ready count.
    async fn candidates(&self, s: &Settings) -> Result<(Vec<PodInfo>, BTreeMap<(String, String), usize>), String> {
        let mut pods = Vec::new();
        for ns in &s.namespaces {
            pods.extend(self.inner.cluster.list_pods(ns).await?);
        }
        let mut ready: BTreeMap<(String, String), usize> = BTreeMap::new();
        for p in pods.iter().filter(|p| p.running && p.ready) {
            *ready.entry((p.namespace.clone(), p.service.clone())).or_default() += 1;
        }
        // The controller's namespaces are the fleet's by its Config (its Role covers no other); the wedge
        // checks the namespace's own labels again.
        let labels = BTreeMap::from([(DEPLOYMENT_LABEL.to_string(), s.fleet.clone())]);
        let eligible = pods
            .into_iter()
            .filter(|p| p.running && p.ready)
            .filter(|p| ready.get(&(p.namespace.clone(), p.service.clone())).copied().unwrap_or(0) >= 2)
            .filter(|p| {
                let facts = PodFacts { namespace: p.namespace.clone(), namespace_labels: labels.clone(), pod: p.name.clone(), service: p.service.clone(), mounts_secret: p.mounts_secret };
                s.exclusions.check(&s.fleet, &facts).is_ok()
            })
            .collect();
        Ok((eligible, ready))
    }

    /// One round now. `kind` unspecified chooses one among the levels switched on.
    pub async fn run_round(&self, kind: FaultKind, trace_id: String) -> Result<Round, tonic::Status> {
        let _one = self.inner.one_round.lock().await;
        let s = self.settings();
        if s.fleet.is_empty() {
            return Err(tonic::Status::failed_precondition("the controller has no Config yet"));
        }
        let round = format!("r{}-{}", now_ms(), self.inner.counter.fetch_add(1, Ordering::Relaxed));
        let trace_id = if trace_id.is_empty() { format!("chaos-{round}") } else { trace_id };
        if self.paused() {
            return Ok(self.skipped(&round, "the schedule is paused".into()));
        }
        let (allowed, chosen) = Self::kinds(&s);
        let kind = match kind {
            FaultKind::Unspecified => match chosen.choose(&mut rand::thread_rng()) {
                Some(k) => *k,
                None => return Ok(self.skipped(&round, "no level is switched on".into())),
            },
            k if allowed.contains(&k) => k,
            k => return Err(tonic::Status::failed_precondition(format!("{} is not in a level the Config switches on", k.as_str_name()))),
        };
        if let Err(why) = self.recovered_since().await {
            return Ok(self.skipped(&round, why));
        }
        let (eligible, ready) = self.candidates(&s).await.map_err(tonic::Status::unavailable)?;
        let Some(pod) = eligible.choose(&mut rand::thread_rng()).cloned() else {
            return Ok(self.skipped(&round, "no eligible target".into()));
        };
        let ready_before = ready.get(&(pod.namespace.clone(), pod.service.clone())).copied().unwrap_or(0);
        let tag = FaultTag { fleet: s.fleet.clone(), round: round.clone(), fault_id: format!("{round}-1"), ..Default::default() };
        let params = default_parameters(kind);
        // The controller checks the bounds too, to refuse early; the wedge's check is the one that counts.
        let b = Bounds::default();
        let bounded = match kind {
            FaultKind::NetworkDelay => b.check_network_delay(params.latency_ms, params.jitter_ms, params.correlation_pct, params.duration_s),
            FaultKind::PacketLoss => b.check_packet_loss(params.loss_pct, params.correlation_pct, params.duration_s),
            FaultKind::Stress => b.check_stress(params.cpu_workers, params.cpu_load_pct, params.memory_pct_of_limit, params.duration_s, Some(1000), None),
            _ => b.check_grace_period(params.grace_period_s),
        };
        bounded.map_err(tonic::Status::internal)?;

        let marker = format!(
            "tacplus chaos: {} fleet={} round={} fault_id={} parameters={:?}",
            kind.as_str_name(),
            tag.fleet,
            tag.round,
            tag.fault_id,
            params
        );
        if let Err(e) = self.inner.cluster.create_event(&pod.namespace, &pod.name, &marker).await {
            tracing::warn!(error = %e, "no Event on the target");
        }
        let target = Target { namespace: pod.namespace.clone(), pod: pod.name.clone(), ..Default::default() };
        let start = now_ms();
        let reply = if kind == FaultKind::PodKill {
            if let Err(e) = self.inner.cluster.delete_pod(&pod.namespace, &pod.name, params.grace_period_s).await {
                return Ok(self.skipped(&round, format!("the pod kill failed: {e}")));
            }
            self.emit(&tag, kind, &pod, &params, Outcome::Injected, start, start, &trace_id);
            FaultReply { tag: Some(tag.clone()), kind: kind as i32, target: Some(target), start_unix_ms: start, outcome: Outcome::Injected as i32, ..Default::default() }
        } else {
            let (t, tg, tr) = (Some(target), Some(tag.clone()), trace_id.clone());
            let call = match kind {
                FaultKind::NetworkDelay => WedgeCall::Delay(CreateNetworkDelayRequest {
                    tag: tg,
                    target: t,
                    latency_ms: params.latency_ms,
                    jitter_ms: params.jitter_ms,
                    correlation_pct: params.correlation_pct,
                    duration_s: params.duration_s,
                    trace_id: tr,
                    ..Default::default()
                }),
                FaultKind::PacketLoss => WedgeCall::Loss(CreatePacketLossRequest {
                    tag: tg,
                    target: t,
                    loss_pct: params.loss_pct,
                    correlation_pct: params.correlation_pct,
                    duration_s: params.duration_s,
                    trace_id: tr,
                    ..Default::default()
                }),
                FaultKind::Stress => WedgeCall::Stress(CreateStressRequest {
                    tag: tg,
                    target: t,
                    cpu_workers: params.cpu_workers,
                    cpu_load_pct: params.cpu_load_pct,
                    memory_pct_of_limit: params.memory_pct_of_limit,
                    duration_s: params.duration_s,
                    trace_id: tr,
                    ..Default::default()
                }),
                _ => WedgeCall::Kill(CreateContainerKillRequest { tag: tg, target: t, trace_id: tr, ..Default::default() }),
            };
            match self.inner.wedges.call(&pod.node, call).await {
                Ok(r) => r,
                Err(e) => return Ok(self.skipped(&round, e)),
            }
        };
        let mut record = Round { round: round.clone(), fault: Some(reply.clone()), service: pod.service.clone(), ..Default::default() };
        if Outcome::try_from(reply.outcome).unwrap_or_default() != Outcome::Injected {
            // Refused by the wedge (it recorded why): nothing was applied, nothing to recover from.
            record.skipped = format!("refused: {}", reply.refusal);
            return Ok(self.remember(record));
        }
        if reply.end_unix_ms > 0 {
            self.inner.live.lock().unwrap().insert(tag.fault_id.clone(), (pod.node.clone(), tag.clone()));
        }
        *self.inner.pending.lock().unwrap() =
            Some(Pending { round: round.clone(), namespace: pod.namespace.clone(), service: pod.service.clone(), ready_before, not_recovered: false });
        let this = self.clone();
        let fault_end = reply.end_unix_ms;
        let recovery = Duration::from_secs(s.recovery_timeout_s as u64);
        tokio::spawn(async move { this.check_recovery(tag, kind, pod, params, ready_before, fault_end, recovery, trace_id).await });
        Ok(self.remember(record))
    }

    /// Waits for the fault to be over, then for the service to have its Ready pods back.
    #[allow(clippy::too_many_arguments)]
    async fn check_recovery(&self, tag: FaultTag, kind: FaultKind, pod: PodInfo, params: FaultParameters, ready_before: usize, fault_end: i64, bound: Duration, trace_id: String) {
        let wait = (fault_end - now_ms()).max(0) as u64;
        tokio::time::sleep(Duration::from_millis(wait)).await;
        self.inner.live.lock().unwrap().remove(&tag.fault_id);
        let deadline = tokio::time::Instant::now() + bound;
        let start = now_ms();
        let recovered = loop {
            // A pod kill takes a moment to show: look after the first poll.
            tokio::time::sleep(self.inner.poll).await;
            match self.ready_count(&pod.namespace, &pod.service).await {
                Ok(n) if n >= ready_before => break true,
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "recovery check"),
            }
            if tokio::time::Instant::now() >= deadline {
                break false;
            }
        };
        let outcome = if recovered { Outcome::Recovered } else { Outcome::NotRecovered };
        self.emit(&tag, kind, &pod, &params, outcome, start, now_ms(), &trace_id);
        {
            let mut pending = self.inner.pending.lock().unwrap();
            if pending.as_ref().is_some_and(|p| p.round == tag.round) {
                if recovered {
                    *pending = None;
                } else if let Some(p) = pending.as_mut() {
                    p.not_recovered = true;
                }
            }
        }
        if let Some(r) = self.inner.rounds.lock().unwrap().iter_mut().find(|r| r.round == tag.round) {
            r.recovery = outcome as i32;
        }
        if !recovered {
            tracing::error!(round = %tag.round, service = %pod.service, "NOT RECOVERED: an incident");
        }
    }

    /// Pauses (reverting every live wedge fault) or resumes.
    pub async fn update_schedule(&self, paused: bool) -> ScheduleResponse {
        *self.inner.api_paused.lock().unwrap() = paused;
        let mut reverted = Vec::new();
        if paused {
            let live: Vec<(String, FaultTag)> = self.inner.live.lock().unwrap().values().cloned().collect();
            for (node, tag) in live {
                match self.inner.wedges.call(&node, WedgeCall::Delete(DeleteFaultRequest { tag: Some(tag.clone()), ..Default::default() })).await {
                    Ok(r) => {
                        self.inner.live.lock().unwrap().remove(&tag.fault_id);
                        reverted.push(r);
                    }
                    Err(e) => tracing::error!(fault_id = %tag.fault_id, error = %e, "pause: revert failed (the wedge still reverts it at its end)"),
                }
            }
        }
        ScheduleResponse { paused: self.paused(), reverted, ..Default::default() }
    }

    pub fn rounds(&self) -> Vec<Round> {
        self.inner.rounds.lock().unwrap().iter().cloned().collect()
    }
}

#[tonic::async_trait]
impl ControllerHandler for Controller {
    async fn create_round(&self, _caller: Option<&Caller>, req: CreateRoundRequest) -> Result<Round, tonic::Status> {
        let kind = FaultKind::try_from(req.kind).map_err(|_| tonic::Status::invalid_argument("unknown fault kind"))?;
        self.run_round(kind, req.trace_id).await
    }

    async fn update_schedule(&self, _caller: Option<&Caller>, req: UpdateScheduleRequest) -> Result<ScheduleResponse, tonic::Status> {
        Ok(Controller::update_schedule(self, req.paused).await)
    }

    async fn list_rounds(&self, _caller: Option<&Caller>, _req: ListRoundsRequest) -> Result<ListRoundsResponse, tonic::Status> {
        Ok(ListRoundsResponse { rounds: self.rounds(), ..Default::default() })
    }
}
