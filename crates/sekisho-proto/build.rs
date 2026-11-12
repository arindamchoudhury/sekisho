//! Generates the gRPC types from `proto/`, which holds files vendored
//! unmodified from agentgateway v1.7.0 (git 26f5aeb), `crates/protos/proto/`,
//! and, in `proto/dex/`, Dex v2.46.1's `api/v2/api.proto` (git 75eb192).
//!
//! `protox` compiles the protos in Rust, so no `protoc` is needed.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto");
    let descriptors = protox::compile(["proto/rls.proto", "proto/ext_proc.proto"], ["proto"])?;
    tonic_prost_build::configure()
        .build_server(true)
        // The tests drive the services through generated clients.
        .build_client(true)
        .compile_fds(descriptors)?;
    // Sekisho only calls Dex's API.
    let dex = protox::compile(["proto/dex/api.proto"], ["proto"])?;
    tonic_prost_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_fds(dex)?;
    Ok(())
}
