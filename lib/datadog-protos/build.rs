fn main() {
    // Always rerun if the build script itself changes.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=proto");

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
        .inputs(["proto/agent-payload/agent_payload.proto"])
        .cargo_out_dir("protos")
        .customize(codegen_customize)
        .run_from_script();

    // Handle code generation for gRPC service definitions.
    tonic_build::configure()
        .build_server(true)
        .include_file("api.mod.rs")
        .compile_protos(
            &["proto/datadog-agent/datadog/api/v1/api.proto"],
            &["proto", "proto/datadog-agent"],
        )
        .expect("Failed to build gRPC service definitions for Datadog Agent.");
}
