// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! chaos-controller: schedules tacplus's chaos for one fleet (tacplus-dev/chaos#1). See
//! proto/services/controller.proto.

pub mod kube;
pub mod service;
pub mod wedges;
#[cfg(test)]
mod service_tests;
