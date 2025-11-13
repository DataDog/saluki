const CONTAINERD_PROTO_FILES: &[&str] = &[
    // Types
    "proto/github.com/containerd/containerd/api/types/descriptor.proto",
    "proto/github.com/containerd/containerd/api/types/metrics.proto",
    "proto/github.com/containerd/containerd/api/types/mount.proto",
    "proto/github.com/containerd/containerd/api/types/platform.proto",
    "proto/github.com/containerd/containerd/api/types/sandbox.proto",
    "proto/github.com/containerd/containerd/api/types/task/task.proto",
    "proto/github.com/containerd/containerd/api/types/transfer/imagestore.proto",
    "proto/github.com/containerd/containerd/api/types/transfer/importexport.proto",
    "proto/github.com/containerd/containerd/api/types/transfer/progress.proto",
    "proto/github.com/containerd/containerd/api/types/transfer/registry.proto",
    "proto/github.com/containerd/containerd/api/types/transfer/streaming.proto",
    // Services
    "proto/github.com/containerd/containerd/api/services/containers/v1/containers.proto",
    "proto/github.com/containerd/containerd/api/services/content/v1/content.proto",
    "proto/github.com/containerd/containerd/api/services/diff/v1/diff.proto",
    "proto/github.com/containerd/containerd/api/services/events/v1/events.proto",
    "proto/github.com/containerd/containerd/api/services/images/v1/images.proto",
    "proto/github.com/containerd/containerd/api/services/introspection/v1/introspection.proto",
    "proto/github.com/containerd/containerd/api/services/leases/v1/leases.proto",
    "proto/github.com/containerd/containerd/api/services/namespaces/v1/namespace.proto",
    "proto/github.com/containerd/containerd/api/services/sandbox/v1/sandbox.proto",
    "proto/github.com/containerd/containerd/api/services/snapshots/v1/snapshots.proto",
    "proto/github.com/containerd/containerd/api/services/streaming/v1/streaming.proto",
    "proto/github.com/containerd/containerd/api/services/tasks/v1/tasks.proto",
    "proto/github.com/containerd/containerd/api/services/transfer/v1/transfer.proto",
    "proto/github.com/containerd/containerd/api/services/version/v1/version.proto",
    // Events
    "proto/github.com/containerd/containerd/api/events/container.proto",
    "proto/github.com/containerd/containerd/api/events/content.proto",
    "proto/github.com/containerd/containerd/api/events/image.proto",
    "proto/github.com/containerd/containerd/api/events/namespace.proto",
    "proto/github.com/containerd/containerd/api/events/snapshot.proto",
    "proto/github.com/containerd/containerd/api/events/task.proto",
];

fn main() {
    // Always rerun if the build script itself changes.
    println!("cargo:rerun-if-changed=build.rs");

    // Handle code generation for gRPC service definitions.
    let mut config = tonic_build::Config::new();
    config.enable_type_names();

    tonic_build::configure()
        .build_server(false)
        .include_file("containerd.mod.rs")
        .compile_protos_with_config(config, CONTAINERD_PROTO_FILES, &["proto/"])
        .expect("failed to build gRPC service definitions for containerd");
}
