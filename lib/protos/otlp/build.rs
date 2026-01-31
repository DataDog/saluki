fn main() {
    // Always rerun if the build script itself changes.
    println!("cargo:rerun-if-changed=build.rs");

    // Handle code generation for gRPC service definitions.
    tonic_prost_build::configure()
        .field_attribute(".", "#[allow(clippy::all)]") // Clippy doesn't like the double spaces in the OTLP proto
        .build_server(true)
        .build_client(true)
        .include_file("otlp.mod.rs")
        .compile_protos(
            &[
                "proto/opentelemetry/proto/collector/metrics/v1/metrics_service.proto",
                "proto/opentelemetry/proto/collector/logs/v1/logs_service.proto",
                "proto/opentelemetry/proto/collector/trace/v1/trace_service.proto",
            ],
            &["proto"],
        )
        .expect("failed to build gRPC service definitions for OTLP");
}
