// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! chaos-wedge's entry point. With `stress ...` it is the stress process the wedge starts itself
//! (src/stress.rs); otherwise the service, under the generated ServiceManager.

use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

use chaos_wedge::kernel::HostKernel;
use chaos_wedge::kube::InCluster;
use chaos_wedge::runtime::Cri;
use chaos_wedge::service::{EventSink, Wedge};
use chaoswedge_model::s2sm::handler::Handler;
use chaoswedge_model::s2sm::internal::grpc::servers::services::chaoswedge::wedge::WedgeHandler;
use chaoswedge_model::s2sm::ServiceManager;
use chaoswedge_model::types::services::chaoswedge::chaoswedge::Config;

/// The containerd socket, mounted from the node.
const CRI_SOCKET: &str = "/run/containerd/containerd.sock";
/// The node's journal (a hostPath); each fleet has its own directory in it.
const JOURNAL_ROOT: &str = "/var/lib/tacplus-chaos";

struct Service {
    wedge: Wedge,
}

#[async_trait::async_trait]
impl Handler for Service {
    fn wedge_grpc_impl(&self) -> Arc<dyn WedgeHandler> {
        Arc::new(self.wedge.clone())
    }

    async fn wedge_config(&self, cfg: &Config) -> Result<(), Box<dyn Error>> {
        Ok(self.wedge.configure(cfg).await?)
    }

    async fn shutdown(&self) {
        // A wedge going away reverts its faults now rather than leave them to the next one.
        self.wedge.revert_all("the wedge's shutdown").await;
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("stress") {
        std::process::exit(chaos_wedge::stress::run(&args[2..]));
    }
    if let Err(e) = logger::install_bootstrap() {
        eprintln!("FATAL: could not install the redacting logger: {e}");
        std::process::exit(1);
    }
    if let Err(e) = run().await {
        tracing::error!(error = %e, "chaos-wedge failed");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    // The node, from the downward API (spec.nodeName): the wedge acts on this node's pods only, and the
    // runtime it asks knows no other.
    let node = std::env::var("NODE_NAME").map_err(|_| "NODE_NAME is not set (the downward API's spec.nodeName)")?;
    let sm = Arc::new(ServiceManager::new().await?);
    let publish = sm.create_chaosv1_fault_event_bus_stream_publisher().await;
    let events: EventSink = Arc::new(move |event| {
        if let Err(e) = publish(event) {
            tracing::error!(error = %e, "fault record queue full: the record is lost");
        }
    });
    let wedge = Wedge::new(
        node,
        PathBuf::from(JOURNAL_ROOT),
        Arc::new(Cri::connect(CRI_SOCKET.as_ref()).await?),
        Arc::new(InCluster::from_env()?),
        Arc::new(HostKernel::default()),
        events,
    );
    sm.run(Arc::new(Service { wedge })).await
}
