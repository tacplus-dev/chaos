// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! Calling the wedge on a target's node. Each fleet runs one wedge per node (#1 Q1); the call must reach
//! the one on the target's node, so the controller finds that wedge's pod (the chaos-wedge pod of its
//! fleet's namespace on that node) and dials it directly, with server TLS and its own kernel-signed token
//! for audience chaos-wedge, as every S2S call. The generated S2S clients pick any healthy instance; a
//! node-routed call is a tacplus feature that does not exist yet (PROPOSALS.md).

use std::sync::Arc;

use async_trait::async_trait;
use chaoscontroller_model::types::services::chaosv1::FaultReply;
use chaoscontroller_model::types::services::chaoswedge::chaoswedge::wedge_client::WedgeClient;
use chaoscontroller_model::types::services::chaoswedge::chaoswedge::{
    CreateContainerKillRequest, CreateNetworkDelayRequest, CreatePacketLossRequest, CreateStressRequest, DeleteFaultRequest,
};
use tonic::transport::Channel;

/// One call to a wedge.
#[derive(Clone, Debug)]
pub enum WedgeCall {
    Delay(CreateNetworkDelayRequest),
    Loss(CreatePacketLossRequest),
    Kill(CreateContainerKillRequest),
    Stress(CreateStressRequest),
    Delete(DeleteFaultRequest),
}

/// How to reach the wedge on a node: a channel, and the bearer token to send (None in tests over plain
/// connections).
#[async_trait]
pub trait Dial: Send + Sync + 'static {
    async fn dial(&self, node: &str) -> Result<(Channel, Option<String>), String>;
}

/// The wedges of the fleet, over gRPC.
pub struct Wedges {
    dial: Arc<dyn Dial>,
}

impl Wedges {
    pub fn new(dial: Arc<dyn Dial>) -> Wedges {
        Wedges { dial }
    }

    /// Calls the wedge on `node`.
    pub async fn call(&self, node: &str, call: WedgeCall) -> Result<FaultReply, String> {
        let (channel, token) = self.dial.dial(node).await?;
        let mut client = WedgeClient::new(channel);
        fn signed<T>(req: T, token: &Option<String>) -> Result<tonic::Request<T>, String> {
            let mut r = tonic::Request::new(req);
            if let Some(t) = token {
                r.metadata_mut().insert("authorization", format!("Bearer {t}").parse().map_err(|e| format!("token: {e}"))?);
            }
            Ok(r)
        }
        let resp = match call {
            WedgeCall::Delay(r) => client.create_network_delay(signed(r, &token)?).await,
            WedgeCall::Loss(r) => client.create_packet_loss(signed(r, &token)?).await,
            WedgeCall::Kill(r) => client.create_container_kill(signed(r, &token)?).await,
            WedgeCall::Stress(r) => client.create_stress(signed(r, &token)?).await,
            WedgeCall::Delete(r) => client.delete_fault(signed(r, &token)?).await,
        };
        resp.map(|r| r.into_inner()).map_err(|s| format!("the wedge on {node}: {}", s.message()))
    }
}
