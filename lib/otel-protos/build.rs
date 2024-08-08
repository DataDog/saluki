fn main() {
    // Always rerun if the build script itself changes.
    println!("cargo:rerun-if-changed=build.rs");

    println!("cargo:rerun-if-changed=proto/opentelemetry/proto");

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
            "proto/opentelemetry/proto/common/v1/common.proto",
            "proto/opentelemetry/proto/resource/v1/resource.proto",
            "proto/opentelemetry/proto/logs/v1/logs.proto",
            "proto/opentelemetry/proto/metrics/v1/metrics.proto",
            "proto/opentelemetry/proto/trace/v1/trace.proto",
        ])
        .cargo_out_dir("protos")
        .customize(codegen_customize)
        .run_from_script();

    // Handle code generation for gRPC service definitions.
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .include_file("api.mod.rs")
        .compile(
            &[
                "proto/opentelemetry/proto/collector/logs/v1/logs_service.proto",
                "proto/opentelemetry/proto/collector/metrics/v1/metrics_service.proto",
                "proto/opentelemetry/proto/collector/trace/v1/trace_service.proto",
            ],
            &["proto"],
        )
        .expect("failed to build gRPC service definitions for DCA")
}
