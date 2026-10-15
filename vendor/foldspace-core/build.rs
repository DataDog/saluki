fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let proto_root = manifest_dir.join("proto");
    let stateful_proto = proto_root.join("datadog/stateful/stateful_encoding.proto");

    println!("cargo:rerun-if-changed={}", stateful_proto.display());

    tonic_prost_build::configure().compile_protos(&[stateful_proto], &[proto_root])?;

    Ok(())
}
