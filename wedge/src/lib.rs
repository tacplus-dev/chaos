// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! chaos-wedge: the only privileged part of tacplus's chaos system (tacplus-dev/chaos#1). See
//! proto/services/wedge.proto for what it does and README.md for its privileges.

pub mod journal;
pub mod kernel;
pub mod kube;
pub mod runtime;
pub mod service;
pub mod stress;
#[cfg(test)]
mod service_tests;

/// The CRI client, generated from cri/api.proto by build.rs.
#[allow(clippy::all)]
pub mod cri {
    tonic::include_proto!("runtime.v1");
}
