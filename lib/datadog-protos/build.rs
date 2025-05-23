fn main() {
    // Always rerun if the build script itself changes.
    println!("cargo:rerun-if-changed=build.rs");

    println!("cargo:rerun-if-changed=proto/agent_payload.proto");
    println!("cargo:rerun-if-changed=proto/dd_trace.proto");
    println!("cargo:rerun-if-changed=proto/ddsketch_full.proto");
    println!("cargo:rerun-if-changed=proto/datadog/api/v1/api.proto");
    println!("cargo:rerun-if-changed=proto/datadog/workloadmeta/workloadmeta.proto");
    println!("cargo:rerun-if-changed=proto/datadog/remoteagent/remoteagent.proto");
    println!("cargo:rerun-if-changed=proto/datadog/autodiscovery/autodiscovery.proto");

    // Handle code generation for pure Protocol Buffers message types.
    let codegen_customize = protobuf_codegen::Customize::default()
        .tokio_bytes(true)
        .tokio_bytes_for_string(true)
        .generate_accessors(true)
        .gen_mod_rs(true)
        .lite_runtime(true);

    protobuf_codegen::Codegen::new()
        .protoc()
        .includes(["proto"])
        .inputs([
            "proto/agent_payload.proto",
            "proto/ddsketch_full.proto",
            "proto/dd_trace.proto",
        ])
        .cargo_out_dir("protos")
        .customize(codegen_customize)
        .run_from_script();

    // Handle code generation for gRPC service definitions.
    tonic_build::configure()
        .build_server(true)
        .include_file("api.mod.rs")
        .compile_protos(
            &[
                "proto/datadog/api/v1/api.proto",
                "proto/datadog/workloadmeta/workloadmeta.proto",
                "proto/datadog/remoteagent/remoteagent.proto",
                "proto/datadog/autodiscovery/autodiscovery.proto",
            ],
            &["proto"],
        )
        .expect("failed to build gRPC service definitions for DCA");
}
