// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! The wedge on a real kernel (#1 acceptance: one network fault through the wedge; auto-revert when the
//! controller dies mid-fault). It needs CAP_NET_ADMIN and CAP_SYS_ADMIN in a network namespace of its
//! own, so it is ignored by a plain `cargo test` and run by ci/netns-test.sh in a container with exactly
//! those (`--cap-add NET_ADMIN --cap-add SYS_ADMIN`). The "pod" is the test process itself: the wedge
//! enters its network namespace through /proc/<pid>/ns/net, as it enters a pod's, and puts netem on its
//! eth0 with `tc`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chaos_policy::exclusion::DEPLOYMENT_LABEL;
use chaos_wedge::kernel::{HostKernel, Kernel, Netem};
use chaos_wedge::kube::Namespaces;
use chaos_wedge::runtime::{Resolved, Runtime};
use chaos_wedge::service::Wedge;
use chaoswedge_model::s2sm::internal::grpc::servers::services::chaoswedge::wedge::WedgeHandler;
use chaoswedge_model::types::services::chaosv1::{FaultEvent, FaultTag, Outcome, Target};
use chaoswedge_model::types::services::chaoswedge::chaoswedge::{Config, CreateNetworkDelayRequest};

fn qdiscs() -> String {
    let out = Command::new("tc").args(["qdisc", "show", "dev", "eth0"]).output().expect("tc");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// This process, as a pod of fleet prod on this node.
struct ThisProcess;

#[async_trait]
impl Runtime for ThisProcess {
    async fn resolve(&self, namespace: &str, pod: &str, _container: &str) -> Result<Resolved, String> {
        Ok(Resolved {
            namespace: namespace.into(),
            pod: pod.into(),
            container: "test".into(),
            container_id: "cid".into(),
            service: "stratum".into(),
            sandbox_pid: std::process::id(),
            cgroup_dir: PathBuf::from("/sys/fs/cgroup"),
            cpu_limit_millis: None,
            memory_limit_bytes: None,
            mounts_secret: false,
        })
    }
    async fn stop_container(&self, _id: &str) -> Result<(), String> {
        Err("not in this test".into())
    }
}

struct ProdNamespace;

#[async_trait]
impl Namespaces for ProdNamespace {
    async fn labels(&self, _namespace: &str) -> Result<BTreeMap<String, String>, String> {
        Ok(BTreeMap::from([(DEPLOYMENT_LABEL.to_string(), "prod".to_string())]))
    }
}

fn controller() -> servicemanager::auth::Caller {
    let mut t = token::token::Token::new();
    t.set_sub_type("service").unwrap();
    t.set_subject("spiffe://td.test/system/services/chaos-controller").unwrap();
    servicemanager::auth::Caller::new(t, "netns-test".into())
}

#[test]
#[ignore = "needs CAP_NET_ADMIN and CAP_SYS_ADMIN: ci/netns-test.sh runs it in a container"]
fn host_kernel_adds_and_removes_netem() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let k = HostKernel::default();
        let pid = std::process::id();
        let inode = k.netns_inode(pid).unwrap();
        k.add_netem(pid, Netem::Delay { latency_ms: 200, jitter_ms: 20, correlation_pct: 25 }).await.unwrap();
        let shown = qdiscs();
        assert!(shown.contains("qdisc netem 7a3c: root") && shown.contains("delay 200ms  20ms 25%"), "{shown}");
        // A second fault on the same pod is refused by the kernel too (the wedge refuses it first).
        assert!(k.add_netem(pid, Netem::Loss { loss_pct: 10, correlation_pct: 0 }).await.is_err());
        assert!(k.remove_netem(pid, inode).await.unwrap());
        assert!(!qdiscs().contains("netem"), "{}", qdiscs());
        // Idempotent: removing what is gone is success, and touches nothing.
        assert!(!k.remove_netem(pid, inode).await.unwrap());
        // Another netns (a pid reused by another process) is never touched.
        k.add_netem(pid, Netem::Loss { loss_pct: 10, correlation_pct: 0 }).await.unwrap();
        assert!(!k.remove_netem(pid, inode + 1).await.unwrap());
        assert!(qdiscs().contains("loss 10%"), "{}", qdiscs());
        assert!(k.remove_netem(pid, inode).await.unwrap());
    });
}

#[test]
#[ignore = "needs CAP_NET_ADMIN and CAP_SYS_ADMIN: ci/netns-test.sh runs it in a container"]
fn a_delay_through_the_wedge_is_reverted_at_its_end_and_after_a_crash() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let journal = tempfile::tempdir().unwrap();
        let events: Arc<Mutex<Vec<FaultEvent>>> = Arc::default();
        let start = |events: Arc<Mutex<Vec<FaultEvent>>>| {
            Wedge::new(
                "node-a".into(),
                journal.path().to_path_buf(),
                Arc::new(ThisProcess),
                Arc::new(ProdNamespace),
                Arc::new(HostKernel::default()),
                Arc::new(move |e| events.lock().unwrap().push(e)),
            )
        };
        let cfg = Config { fleet: "prod".into(), ..Default::default() };
        let delay = |id: &str, duration_s| CreateNetworkDelayRequest {
            tag: Some(FaultTag { fleet: "prod".into(), round: "r1".into(), fault_id: id.into(), ..Default::default() }),
            target: Some(Target { namespace: "tac-prod".into(), pod: "stratum-0".into(), ..Default::default() }),
            latency_ms: 150,
            duration_s,
            trace_id: format!("trace-{id}"),
            ..Default::default()
        };

        // At its end, with no further call: the controller may have died the moment it returned.
        let wedge = start(events.clone());
        wedge.configure(&cfg).await.unwrap();
        let reply = wedge.create_network_delay(Some(&controller()), delay("f1", 1)).await.unwrap();
        assert_eq!(Outcome::try_from(reply.outcome).unwrap(), Outcome::Injected, "{}", reply.refusal);
        assert!(qdiscs().contains("delay 150ms"), "{}", qdiscs());
        tokio::time::sleep(Duration::from_millis(1600)).await;
        assert!(!qdiscs().contains("netem"), "reverted at its end: {}", qdiscs());

        // After a crash: a 300 s fault, the wedge gone; the next wedge reverts it before it serves.
        let reply = wedge.create_network_delay(Some(&controller()), delay("f2", 300)).await.unwrap();
        assert_eq!(Outcome::try_from(reply.outcome).unwrap(), Outcome::Injected, "{}", reply.refusal);
        drop(wedge);
        assert!(qdiscs().contains("delay 150ms"), "{}", qdiscs());
        let restarted = start(events.clone());
        restarted.configure(&cfg).await.unwrap();
        assert!(!qdiscs().contains("netem"), "reverted on start: {}", qdiscs());

        let outcomes: Vec<(String, Outcome)> =
            events.lock().unwrap().iter().map(|e| (e.tag.as_ref().unwrap().fault_id.clone(), Outcome::try_from(e.outcome).unwrap())).collect();
        assert_eq!(
            outcomes,
            [("f1".into(), Outcome::Injected), ("f1".into(), Outcome::Reverted), ("f2".into(), Outcome::Injected), ("f2".into(), Outcome::Reverted)]
        );
    });
}
