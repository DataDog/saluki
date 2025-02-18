fn main() {
    // Always rerun if the build script itself changes.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../proto/datadog/adp/v1/telemetry.proto");

    // Handle code generation for gRPC service definitions.
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .include_file("telemetry.mod.rs")
        .compile_protos(&["../../proto/datadog/adp/v1/telemetry.proto"], &["../../proto"])
        .expect("failed to build gRPC service definitions for ADP telemetry")
}
