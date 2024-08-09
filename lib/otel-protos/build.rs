fn main() {
    // Always rerun if the build script itself changes.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=proto/opentelemetry/proto");

    // Handle code generation for gRPC service definitions.
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .include_file("mod.rs")
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
