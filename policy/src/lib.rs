// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! The rules both chaos services enforce (tacplus-dev/chaos#1): the compiled-in bounds on every fault
//! parameter, the exclusion list, and the fleet check. The controller applies them to refuse early; the
//! wedge applies them again on what it resolved itself, and its check is the one that counts.
//!
//! Pure functions over plain values: no I/O, so each rule is tested on its own.

pub mod bounds;
pub mod exclusion;

/// The two services, by registry name.
pub const CONTROLLER: &str = "chaos-controller";
pub const WEDGE: &str = "chaos-wedge";
