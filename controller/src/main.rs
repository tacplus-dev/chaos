// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! chaos-controller's entry point, under the generated ServiceManager.

use std::error::Error;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chaos_controller::kube::{Cluster, InCluster};
use chaos_controller::service::{Controller, EventSink};
use chaos_controller::wedges::{Dial, Wedges};
use chaoscontroller_model::s2sm::handler::Handler;
use chaoscontroller_model::s2sm::internal::grpc::servers::services::chaoscontroller::controller::ControllerHandler;
use chaoscontroller_model::s2sm::ServiceManager;
use chaoscontroller_model::types::services::chaoscontroller::chaoscontroller::Config;
use tonic::transport::Channel;

/// The wedge on a node: the fleet's chaos-wedge pod on it, dialled with the ServiceManager's TLS and a
/// kernel-signed token for audience chaos-wedge.
struct WedgeOnNode {
    cluster: Arc<dyn Cluster>,
    sm: Arc<ServiceManager>,
    /// The fleet's namespaces and the wedge's port, from the Config.
    where_: Arc<Mutex<(Vec<String>, u32)>>,
}

#[async_trait]
impl Dial for WedgeOnNode {
    async fn dial(&self, node: &str) -> Result<(Channel, Option<String>), String> {
        let (namespaces, port) = self.where_.lock().unwrap().clone();
        let mut addr = None;
        for ns in &namespaces {
            let pods = self.cluster.list_pods(ns).await?;
            if let Some(p) = pods.iter().find(|p| p.service == chaos_policy::WEDGE && p.node == node && p.running && !p.ip.is_empty()) {
                addr = Some(format!("{}:{port}", p.ip));
                break;
            }
        }
        let addr = addr.ok_or_else(|| format!("no chaos-wedge of this fleet runs on node {node}"))?;
        let channel = self.sm.create_channel(&addr).await.map_err(|e| format!("dial the wedge at {addr}: {e}"))?;
        let token = self.sm.token_for_service(chaos_policy::WEDGE).await.map_err(|e| format!("token for chaos-wedge: {e}"))?;
        Ok((channel, Some(token)))
    }
}

struct Service {
    controller: Controller,
    where_: Arc<Mutex<(Vec<String>, u32)>>,
}

#[async_trait]
impl Handler for Service {
    fn controller_grpc_impl(&self) -> Arc<dyn ControllerHandler> {
        Arc::new(self.controller.clone())
    }

    async fn controller_config(&self, cfg: &Config) -> Result<(), Box<dyn Error>> {
        self.controller.configure(cfg)?;
        *self.where_.lock().unwrap() = (cfg.namespaces.clone(), cfg.wedge_port);
        Ok(())
    }

    async fn shutdown(&self) {
        // Live wedge faults end on their own at their end (the wedge's timer); nothing to drain here.
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = logger::install_bootstrap() {
        eprintln!("FATAL: could not install the redacting logger: {e}");
        std::process::exit(1);
    }
    if let Err(e) = run().await {
        tracing::error!(error = %e, "chaos-controller failed");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let sm = Arc::new(ServiceManager::new().await?);
    let publish = sm.create_chaosv1_fault_event_bus_stream_publisher().await;
    let events: EventSink = Arc::new(move |event| {
        if let Err(e) = publish(event) {
            tracing::error!(error = %e, "fault record queue full: the record is lost");
        }
    });
    let cluster: Arc<dyn Cluster> = Arc::new(InCluster::from_env()?);
    let where_ = Arc::new(Mutex::new((Vec::new(), 8443)));
    let wedges = Wedges::new(Arc::new(WedgeOnNode { cluster: cluster.clone(), sm: sm.clone(), where_: where_.clone() }));
    let controller = Controller::new(cluster, wedges, events, Duration::from_secs(2));
    tokio::spawn(controller.clone().schedule());
    sm.run(Arc::new(Service { controller, where_ })).await
}
