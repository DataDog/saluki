use std::{fs, io, path::PathBuf};

const CONTAINERD_PROTO_FILES: &[&str] = &[
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

    if let Err(e) = fixup_bad_doc_comments() {
        eprintln!("Failed to fixup bad doc comments: {}", e);
        std::process::exit(1);
    }
}

fn fixup_bad_doc_comments() -> Result<(), io::Error> {
    // Some of the comments ported from the Protocol Buffers definitions end up getting parsed as Rust code
    // in the doc comments, so we need to replace those usages with something that disables that.
    let fixup_files = [
        "containerd.services.containers.v1.rs",
        "containerd.services.content.v1.rs",
        "containerd.services.images.v1.rs",
        "containerd.services.introspection.v1.rs",
        "containerd.services.snapshots.v1.rs",
    ];

    let out_dir = std::env::var("OUT_DIR").map(PathBuf::from).unwrap();
    for fixup_file in fixup_files {
        let generated_file = out_dir.join(fixup_file);

        let file_contents = fs::read_to_string(&generated_file)?.replace(
            "/// 	filters\\[0\\] or filters\\[1\\] or ... or filters\\[n-1\\] or filters\\[n\\]",
            r#"
            /// ```text
            /// 	filters[0] or filters[1] or ... or filters[n-1] or filters[n]
            /// ```"#,
        );
        fs::write(&generated_file, file_contents)?;
    }

    Ok(())
}
