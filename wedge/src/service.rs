// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! The wedge's handler (proto/services/wedge.proto). Every fault call goes through the same steps, and a
//! refusal at any of them applies nothing:
//!
//! 1. the caller is chaos-controller (the generated authorizer admits no one else; checked again here);
//! 2. the tag's fleet is this wedge's fleet;
//! 3. every parameter is within its compiled-in bound (policy/src/bounds.rs);
//! 4. the pod has no live fault, and this node has room for one more;
//! 5. the target is resolved on this node by the runtime, and its namespace's own labels put it in the
//!    fleet, and it is not excluded (policy/src/exclusion.rs);
//! 6. the fault is journalled, then applied, then its revert is scheduled at its end.
//!
//! Each outcome (injected, refused, reverted) is a `chaosv1.FaultEvent` on the record, carrying the
//! trace_id of the call that started the fault.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use chaos_policy::bounds::Bounds;
use chaos_policy::exclusion::{Exclusions, PodFacts};
use chaoswedge_model::s2sm::internal::grpc::servers::services::chaoswedge::wedge::WedgeHandler;
use chaoswedge_model::types::services::chaosv1::{FaultEvent, FaultKind, FaultParameters, FaultReply, FaultTag, Outcome, Target};
use chaoswedge_model::types::services::chaoswedge::chaoswedge::{
    Config, CreateContainerKillRequest, CreateNetworkDelayRequest, CreatePacketLossRequest, CreateStressRequest, DeleteFaultRequest,
    ListFaultsRequest, ListFaultsResponse,
};
use servicemanager::auth::Caller;

use crate::journal::{valid_fault_id, Effect, Entry, Journal};
use crate::kernel::{Kernel, Netem, StressProcess, StressSpec};
use crate::kube::Namespaces;
use crate::runtime::{Resolved, Runtime};

/// Where the fault records go: the generated FaultEvent publisher in production, a list in tests.
pub type EventSink = Arc<dyn Fn(FaultEvent) + Send + Sync>;

/// The settings in force, from Config.
#[derive(Clone, Debug, Default)]
struct Settings {
    fleet: String,
    bounds: Bounds,
    exclusions: Exclusions,
}

struct Live {
    entry: Entry,
    timer: Option<tokio::task::JoinHandle<()>>,
}

struct Inner {
    node: String,
    journal_root: PathBuf,
    runtime: Arc<dyn Runtime>,
    namespaces: Arc<dyn Namespaces>,
    kernel: Arc<dyn Kernel>,
    events: EventSink,
    settings: Mutex<Settings>,
    journal: Mutex<Option<Journal>>,
    live: Mutex<BTreeMap<String, Live>>,
    /// Serializes fault calls and reverts, so the checks and the apply of one are never interleaved with
    /// another's, nor with a revert (whose slot is free only once its effect is gone).
    apply: tokio::sync::Mutex<()>,
    /// Set by the shutdown: no fault is applied after it.
    closing: std::sync::atomic::AtomicBool,
}

/// The wedge's handler. Cheap to clone: the state is shared.
#[derive(Clone)]
pub struct Wedge {
    inner: Arc<Inner>,
}

/// How soon a failed revert is tried again.
const REVERT_RETRY: Duration = Duration::from_secs(5);

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}

/// What one fault call asks for, whatever its kind.
struct Ask {
    tag: FaultTag,
    target: Target,
    kind: FaultKind,
    params: FaultParameters,
    trace_id: String,
}

fn refusal(reason: impl Into<String>) -> String {
    reason.into()
}

impl Wedge {
    pub fn new(
        node: String,
        journal_root: PathBuf,
        runtime: Arc<dyn Runtime>,
        namespaces: Arc<dyn Namespaces>,
        kernel: Arc<dyn Kernel>,
        events: EventSink,
    ) -> Wedge {
        Wedge {
            inner: Arc::new(Inner {
                node,
                journal_root,
                runtime,
                namespaces,
                kernel,
                events,
                settings: Mutex::new(Settings::default()),
                journal: Mutex::new(None),
                live: Mutex::new(BTreeMap::new()),
                apply: tokio::sync::Mutex::new(()),
                closing: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    /// Applies a Config. The first one names the fleet and opens its journal, and reverts what the journal
    /// holds (a previous wedge's faults) before the wedge serves. The fleet never changes after that: a
    /// Config naming another one is refused.
    pub async fn configure(&self, cfg: &Config) -> Result<(), String> {
        let settings = Settings {
            fleet: cfg.fleet.clone(),
            bounds: Bounds::tightened(cfg.max_duration_s, cfg.max_latency_ms, cfg.max_loss_pct, cfg.max_memory_pct_of_limit, cfg.max_live_faults),
            exclusions: Exclusions::with(&cfg.exclude_namespaces, &cfg.exclude_services),
        };
        // It names the journal's directory: a DNS label, as a deployment name is.
        if settings.fleet.is_empty() || settings.fleet.len() > 63 || !settings.fleet.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
            return Err(format!("Config.fleet {:?} is not a deployment name", settings.fleet));
        }
        let first = {
            let mut cur = self.inner.settings.lock().unwrap();
            if !cur.fleet.is_empty() && cur.fleet != settings.fleet {
                return Err(format!("Config.fleet changed from {} to {}; the wedge serves one fleet", cur.fleet, settings.fleet));
            }
            let first = cur.fleet.is_empty();
            *cur = settings.clone();
            first
        };
        if first {
            let journal = Journal::open(&self.inner.journal_root, &settings.fleet).map_err(|e| format!("open journal: {e}"))?;
            *self.inner.journal.lock().unwrap() = Some(journal);
            self.recover().await;
        }
        Ok(())
    }

    /// Reverts every fault the journal holds: a wedge starting after another reverts its faults first.
    pub async fn recover(&self) {
        let entries = match self.journal() {
            Some(j) => j.entries().unwrap_or_else(|e| {
                tracing::error!(error = %e, "cannot read the journal");
                Vec::new()
            }),
            None => return,
        };
        for entry in entries {
            tracing::info!(fault_id = %entry.fault_id, "reverting a fault left in the journal");
            self.revert_entry(&entry, "the wedge restarted", true).await;
        }
    }

    /// Reverts every live fault now and applies none after (the wedge's shutdown).
    pub async fn revert_all(&self, why: &str) {
        self.inner.closing.store(true, std::sync::atomic::Ordering::SeqCst);
        // Wait for a fault call in flight: it is live once it returns.
        drop(self.inner.apply.lock().await);
        let entries: Vec<Entry> = self.inner.live.lock().unwrap().values().map(|l| l.entry.clone()).collect();
        for entry in entries {
            self.revert_entry(&entry, why, false).await;
        }
    }

    fn journal(&self) -> Option<Journal> {
        self.inner.journal.lock().unwrap().clone()
    }

    fn settings(&self) -> Settings {
        self.inner.settings.lock().unwrap().clone()
    }

    fn emit(&self, ask_or_entry: EventFields, outcome: Outcome, refusal: &str) {
        let f = ask_or_entry;
        let event = FaultEvent {
            // Within event_id's 64 bytes: a fault id is at most 40 (valid_fault_id).
            event_id: format!("{}-{}-{}", f.tag.fault_id, outcome.as_str_name().trim_start_matches("OUTCOME_").to_lowercase(), now_ms()),
            fleet: f.tag.fleet.clone(),
            namespace: f.target.namespace.clone(),
            tag: Some(f.tag),
            kind: f.kind as i32,
            target: Some(f.target),
            service: f.service,
            parameters: Some(f.params),
            outcome: outcome as i32,
            refusal: bounded(refusal),
            start_unix_ms: f.start,
            end_unix_ms: f.end,
            recorded_by: chaos_policy::WEDGE.into(),
            node: self.inner.node.clone(),
            recorded_unix_ms: now_ms(),
            trace_id: f.trace_id,
            ..Default::default()
        };
        (self.inner.events)(event);
    }

    fn refuse(&self, ask: Ask, why: String) -> FaultReply {
        tracing::warn!(fault_id = %ask.tag.fault_id, kind = ?ask.kind, refusal = %why, "refused");
        let now = now_ms();
        let reply = FaultReply {
            tag: Some(ask.tag.clone()),
            kind: ask.kind as i32,
            target: Some(ask.target.clone()),
            start_unix_ms: now,
            end_unix_ms: now,
            outcome: Outcome::Refused as i32,
            refusal: bounded(&why),
            ..Default::default()
        };
        self.emit(EventFields::from_ask(&ask, String::new(), now, now), Outcome::Refused, &why);
        reply
    }

    /// Steps 1–5: Ok(the resolved target) or the refusal.
    async fn check(&self, caller: Option<&Caller>, ask: &Ask) -> Result<Resolved, String> {
        let settings = self.settings();
        if !caller.is_some_and(is_controller) {
            return Err(refusal("the caller is not chaos-controller"));
        }
        if settings.fleet.is_empty() {
            return Err(refusal("the wedge has no Config yet"));
        }
        if self.inner.closing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(refusal("the wedge is shutting down"));
        }
        if ask.tag.fleet != settings.fleet {
            return Err(format!("fleet {:?} is not this wedge's fleet {}", ask.tag.fleet, settings.fleet));
        }
        if !valid_fault_id(&ask.tag.fault_id) {
            return Err(format!("fault_id {:?} is not a plain token", ask.tag.fault_id));
        }
        let p = &ask.params;
        let b = settings.bounds;
        match ask.kind {
            FaultKind::NetworkDelay => b.check_network_delay(p.latency_ms, p.jitter_ms, p.correlation_pct, p.duration_s)?,
            FaultKind::PacketLoss => b.check_packet_loss(p.loss_pct, p.correlation_pct, p.duration_s)?,
            // Stress is checked again below, once the container's limits are known.
            FaultKind::Stress => b.check_duration(p.duration_s)?,
            _ => {}
        }
        {
            let live = self.inner.live.lock().unwrap();
            if live.contains_key(&ask.tag.fault_id) {
                return Err(format!("fault {} is already live", ask.tag.fault_id));
            }
            if live.values().any(|l| l.entry.namespace == ask.target.namespace && l.entry.pod == ask.target.pod) {
                return Err(format!("pod {}/{} already has a live fault", ask.target.namespace, ask.target.pod));
            }
            if live.len() as u32 >= b.max_live_faults {
                return Err(format!("this node has {} live faults, the most it takes", live.len()));
            }
        }
        settings.exclusions.check_namespace(&ask.target.namespace)?;
        let resolved = self.inner.runtime.resolve(&ask.target.namespace, &ask.target.pod, &ask.target.container).await?;
        let labels = self.inner.namespaces.labels(&resolved.namespace).await?;
        let facts = PodFacts {
            namespace: resolved.namespace.clone(),
            namespace_labels: labels,
            pod: resolved.pod.clone(),
            service: resolved.service.clone(),
            mounts_secret: resolved.mounts_secret,
        };
        settings.exclusions.check(&settings.fleet, &facts)?;
        if ask.kind == FaultKind::Stress {
            b.check_stress(p.cpu_workers, p.cpu_load_pct, p.memory_pct_of_limit, p.duration_s, resolved.cpu_limit_millis, resolved.memory_limit_bytes)?;
        }
        Ok(resolved)
    }

    async fn inject(&self, caller: Option<&Caller>, ask: Ask) -> FaultReply {
        let _one_at_a_time = self.inner.apply.lock().await;
        let resolved = match self.check(caller, &ask).await {
            Ok(r) => r,
            Err(why) => return self.refuse(ask, why),
        };
        let mut target = ask.target.clone();
        target.container = resolved.container.clone();
        let start = now_ms();
        let p = ask.params.clone();
        if ask.kind == FaultKind::ContainerKill {
            if let Err(e) = self.inner.runtime.stop_container(&resolved.container_id).await {
                return self.refuse(ask, format!("StopContainer failed (the container may have been stopped all the same): {e}"));
            }
            self.emit(EventFields::from_ask(&Ask { target: target.clone(), ..ask_clone(&ask) }, resolved.service.clone(), start, start), Outcome::Injected, "");
            return reply(&ask.tag, ask.kind, &target, start, 0, Outcome::Injected);
        }
        let end = start + p.duration_s as i64 * 1000;
        let effect = match ask.kind {
            FaultKind::NetworkDelay | FaultKind::PacketLoss => match self.inner.kernel.netns_inode(resolved.sandbox_pid) {
                Ok(netns_inode) => Effect::Netem { pid: resolved.sandbox_pid, netns_inode },
                Err(e) => return self.refuse(ask, e),
            },
            // The stress process does not exist yet; its pid is recorded once it does (below).
            _ => Effect::Stress { pid: 0, start_ticks: 0 },
        };
        let mut entry = Entry {
            fleet: ask.tag.fleet.clone(),
            round: ask.tag.round.clone(),
            fault_id: ask.tag.fault_id.clone(),
            kind: ask.kind as i32,
            namespace: target.namespace.clone(),
            pod: target.pod.clone(),
            container: target.container.clone(),
            service: resolved.service.clone(),
            start_unix_ms: start,
            end_unix_ms: end,
            trace_id: ask.trace_id.clone(),
            effect,
        };
        let Some(journal) = self.journal() else {
            return self.refuse(ask, "the wedge has no journal".into());
        };
        let applied = match ask.kind {
            FaultKind::NetworkDelay | FaultKind::PacketLoss => {
                // Journalled before it is applied: a crash between the two leaves an entry whose revert
                // finds nothing to remove, never a fault with no entry.
                if let Err(e) = journal.write(&entry) {
                    return self.refuse(ask, format!("journal: {e}"));
                }
                let netem = if ask.kind == FaultKind::NetworkDelay {
                    Netem::Delay { latency_ms: p.latency_ms, jitter_ms: p.jitter_ms, correlation_pct: p.correlation_pct }
                } else {
                    Netem::Loss { loss_pct: p.loss_pct, correlation_pct: p.correlation_pct }
                };
                self.inner.kernel.add_netem(resolved.sandbox_pid, netem).await
            }
            _ => {
                let memory_bytes = resolved.memory_limit_bytes.unwrap_or(0) / 100 * p.memory_pct_of_limit as u64;
                let spec = StressSpec { cpu_workers: p.cpu_workers, cpu_load_pct: p.cpu_load_pct, memory_bytes, duration_s: p.duration_s };
                // Journalled before it starts (with no pid yet: a crash before the pid is recorded leaves an
                // entry whose revert finds nothing, and the stress process ends itself at its end).
                if let Err(e) = journal.write(&entry) {
                    return self.refuse(ask, format!("journal: {e}"));
                }
                match self.inner.kernel.start_stress(&resolved.cgroup_dir, spec).await {
                    Ok(StressProcess { pid, start_ticks }) => {
                        entry.effect = Effect::Stress { pid, start_ticks };
                        journal.write(&entry).map_err(|e| format!("journal: {e}"))
                    }
                    Err(e) => Err(e),
                }
            }
        };
        if let Err(e) = applied {
            // Undo whatever part of it took, and drop the entry.
            let _ = self.undo(&entry).await;
            let _ = journal.remove(&entry.fault_id);
            return self.refuse(ask, format!("the fault was not applied: {e}"));
        }
        let timer = self.schedule_revert(entry.fault_id.clone(), Duration::from_millis((end - now_ms()).max(0) as u64));
        self.inner.live.lock().unwrap().insert(entry.fault_id.clone(), Live { entry: entry.clone(), timer: Some(timer) });
        self.emit(EventFields::from_entry(&entry, p.clone()), Outcome::Injected, "");
        reply(&ask.tag, ask.kind, &target, start, end, Outcome::Injected)
    }

    fn schedule_revert(&self, fault_id: String, after: Duration) -> tokio::task::JoinHandle<()> {
        let weak: Weak<Inner> = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            if let Some(inner) = weak.upgrade() {
                let wedge = Wedge { inner };
                let entry = wedge.inner.live.lock().unwrap().get(&fault_id).map(|l| l.entry.clone());
                if let Some(entry) = entry {
                    wedge.revert_entry_from_timer(&entry).await;
                }
            }
        })
    }

    async fn revert_entry_from_timer(&self, entry: &Entry) {
        // The timer is this task: forget it before reverting so the revert does not abort itself.
        if let Some(l) = self.inner.live.lock().unwrap().get_mut(&entry.fault_id) {
            l.timer = None;
        }
        self.revert_entry(entry, "its end", false).await;
    }

    /// Removes the effect. Ok(true) when something was removed, Ok(false) when it was already gone.
    async fn undo(&self, entry: &Entry) -> Result<bool, String> {
        match entry.effect {
            Effect::Netem { pid, netns_inode } => self.inner.kernel.remove_netem(pid, netns_inode).await,
            Effect::Stress { pid: 0, .. } => Ok(false),
            Effect::Stress { pid, start_ticks } => self.inner.kernel.stop_stress(StressProcess { pid, start_ticks }).await,
        }
    }

    /// Reverts one fault: idempotent, and recorded once. A fault that is no longer live was reverted by
    /// another path already (its timer, a DeleteFault, the shutdown), unless it comes from the journal of a
    /// previous wedge. A revert that fails stays journalled and live, and is tried again shortly.
    async fn revert_entry(&self, entry: &Entry, why: &str, from_journal: bool) -> Option<FaultReply> {
        let _one_at_a_time = self.inner.apply.lock().await;
        let live = self.inner.live.lock().unwrap().remove(&entry.fault_id);
        let kind = FaultKind::try_from(entry.kind).unwrap_or_default();
        match live {
            Some(Live { timer: Some(t), .. }) => t.abort(),
            Some(_) => {}
            None if from_journal => {}
            None => return Some(reply(&entry_tag(entry), kind, &entry_target(entry), entry.start_unix_ms, now_ms(), Outcome::Reverted)),
        }
        let params = FaultParameters { duration_s: ((entry.end_unix_ms - entry.start_unix_ms) / 1000) as u32, ..Default::default() };
        match self.undo(entry).await {
            Ok(removed) => {
                if let Some(j) = self.journal() {
                    if let Err(e) = j.remove(&entry.fault_id) {
                        tracing::error!(fault_id = %entry.fault_id, error = %e, "cannot remove the journal entry");
                    }
                }
                let note = if removed { format!("reverted at {why}") } else { format!("reverted at {why} (target gone)") };
                tracing::info!(fault_id = %entry.fault_id, %note, "reverted");
                let now = now_ms();
                let mut fields = EventFields::from_entry(entry, params);
                fields.end = now;
                self.emit(fields, Outcome::Reverted, "");
                Some(reply(&entry_tag(entry), kind, &entry_target(entry), entry.start_unix_ms, now, Outcome::Reverted))
            }
            Err(e) => {
                tracing::error!(fault_id = %entry.fault_id, error = %e, "revert failed: it stays journalled, and is tried again");
                let timer = self.schedule_revert(entry.fault_id.clone(), REVERT_RETRY);
                self.inner.live.lock().unwrap().insert(entry.fault_id.clone(), Live { entry: entry.clone(), timer: Some(timer) });
                None
            }
        }
    }
}

/// A refusal within the record's 512 bytes (a runtime's or tc's error can be longer).
fn bounded(why: &str) -> String {
    let mut end = why.len().min(500);
    while !why.is_char_boundary(end) {
        end -= 1;
    }
    why[..end].to_string()
}

fn ask_clone(a: &Ask) -> Ask {
    Ask { tag: a.tag.clone(), target: a.target.clone(), kind: a.kind, params: a.params.clone(), trace_id: a.trace_id.clone() }
}

fn entry_tag(e: &Entry) -> FaultTag {
    FaultTag { fleet: e.fleet.clone(), round: e.round.clone(), fault_id: e.fault_id.clone(), ..Default::default() }
}

fn entry_target(e: &Entry) -> Target {
    Target { namespace: e.namespace.clone(), pod: e.pod.clone(), container: e.container.clone(), ..Default::default() }
}

fn reply(tag: &FaultTag, kind: FaultKind, target: &Target, start: i64, end: i64, outcome: Outcome) -> FaultReply {
    FaultReply { tag: Some(tag.clone()), kind: kind as i32, target: Some(target.clone()), start_unix_ms: start, end_unix_ms: end, outcome: outcome as i32, ..Default::default() }
}

/// The fields of a FaultEvent that come from the fault.
struct EventFields {
    tag: FaultTag,
    target: Target,
    kind: FaultKind,
    service: String,
    params: FaultParameters,
    start: i64,
    end: i64,
    trace_id: String,
}

impl EventFields {
    fn from_ask(a: &Ask, service: String, start: i64, end: i64) -> EventFields {
        EventFields { tag: a.tag.clone(), target: a.target.clone(), kind: a.kind, service, params: a.params.clone(), start, end, trace_id: a.trace_id.clone() }
    }

    fn from_entry(e: &Entry, params: FaultParameters) -> EventFields {
        EventFields {
            tag: entry_tag(e),
            target: entry_target(e),
            kind: FaultKind::try_from(e.kind).unwrap_or_default(),
            service: e.service.clone(),
            params,
            start: e.start_unix_ms,
            end: e.end_unix_ms,
            trace_id: e.trace_id.clone(),
        }
    }
}

/// Whether the authenticated caller is chaos-controller: its token's subject is the service's name or
/// its SPIFFE id.
fn is_controller(caller: &Caller) -> bool {
    let sub = caller.token().subject().unwrap_or_default();
    sub == chaos_policy::CONTROLLER || sub.ends_with(&format!("/services/{}", chaos_policy::CONTROLLER))
}

#[tonic::async_trait]
impl WedgeHandler for Wedge {
    async fn create_network_delay(&self, caller: Option<&Caller>, req: CreateNetworkDelayRequest) -> Result<FaultReply, tonic::Status> {
        let params = FaultParameters { latency_ms: req.latency_ms, jitter_ms: req.jitter_ms, correlation_pct: req.correlation_pct, duration_s: req.duration_s, ..Default::default() };
        Ok(self.inject(caller, Ask { tag: req.tag.unwrap_or_default(), target: req.target.unwrap_or_default(), kind: FaultKind::NetworkDelay, params, trace_id: req.trace_id }).await)
    }

    async fn create_packet_loss(&self, caller: Option<&Caller>, req: CreatePacketLossRequest) -> Result<FaultReply, tonic::Status> {
        let params = FaultParameters { loss_pct: req.loss_pct, correlation_pct: req.correlation_pct, duration_s: req.duration_s, ..Default::default() };
        Ok(self.inject(caller, Ask { tag: req.tag.unwrap_or_default(), target: req.target.unwrap_or_default(), kind: FaultKind::PacketLoss, params, trace_id: req.trace_id }).await)
    }

    async fn create_container_kill(&self, caller: Option<&Caller>, req: CreateContainerKillRequest) -> Result<FaultReply, tonic::Status> {
        Ok(self.inject(caller, Ask { tag: req.tag.unwrap_or_default(), target: req.target.unwrap_or_default(), kind: FaultKind::ContainerKill, params: Default::default(), trace_id: req.trace_id }).await)
    }

    async fn create_stress(&self, caller: Option<&Caller>, req: CreateStressRequest) -> Result<FaultReply, tonic::Status> {
        let params = FaultParameters {
            cpu_workers: req.cpu_workers,
            cpu_load_pct: req.cpu_load_pct,
            memory_pct_of_limit: req.memory_pct_of_limit,
            duration_s: req.duration_s,
            ..Default::default()
        };
        Ok(self.inject(caller, Ask { tag: req.tag.unwrap_or_default(), target: req.target.unwrap_or_default(), kind: FaultKind::Stress, params, trace_id: req.trace_id }).await)
    }

    async fn delete_fault(&self, caller: Option<&Caller>, req: DeleteFaultRequest) -> Result<FaultReply, tonic::Status> {
        let tag = req.tag.unwrap_or_default();
        if !caller.is_some_and(is_controller) {
            return Err(tonic::Status::permission_denied("the caller is not chaos-controller"));
        }
        if tag.fleet != self.settings().fleet {
            return Err(tonic::Status::permission_denied(format!("fleet {:?} is not this wedge's fleet", tag.fleet)));
        }
        // A fault call in flight is live once it returns: wait for it.
        drop(self.inner.apply.lock().await);
        let entry = self.inner.live.lock().unwrap().get(&tag.fault_id).map(|l| l.entry.clone());
        let Some(entry) = entry else {
            // Gone already: reverting it is success.
            let now = now_ms();
            return Ok(FaultReply { tag: Some(tag), start_unix_ms: now, end_unix_ms: now, outcome: Outcome::Reverted as i32, ..Default::default() });
        };
        self.revert_entry(&entry, "the controller's request", false)
            .await
            .ok_or_else(|| tonic::Status::internal(format!("fault {} could not be reverted; it stays journalled", tag.fault_id)))
    }

    async fn list_faults(&self, _caller: Option<&Caller>, _req: ListFaultsRequest) -> Result<ListFaultsResponse, tonic::Status> {
        let faults = self
            .inner
            .live
            .lock()
            .unwrap()
            .values()
            .map(|l| {
                let kind = FaultKind::try_from(l.entry.kind).unwrap_or_default();
                reply(&entry_tag(&l.entry), kind, &entry_target(&l.entry), l.entry.start_unix_ms, l.entry.end_unix_ms, Outcome::Injected)
            })
            .collect();
        Ok(ListFaultsResponse { faults, ..Default::default() })
    }
}
