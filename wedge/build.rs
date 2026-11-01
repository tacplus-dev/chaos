// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

// The CRI client the wedge resolves targets and stops containers with (src/runtime.rs), generated from
// the vendored cri/api.proto at build time. Client only: the wedge serves no CRI.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=cri/api.proto");
    tonic_prost_build::configure().build_server(false).compile_protos(&["cri/api.proto"], &["cri"])?;
    Ok(())
}
