// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Generates the Rust types of `proto/carlink.proto` with a protobuf compiler written in Rust, so the build needs no
//! `protoc`.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto = "proto/carlink.proto";
    println!("cargo:rerun-if-changed={proto}");
    let descriptors = protox::compile([proto], ["proto"])?;
    prost_build::Config::new().compile_fds(descriptors)?;
    Ok(())
}
