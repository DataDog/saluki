use std::path::{Path, PathBuf};

use datadog_agent_config_overlay_model::{schema_gen, Files, SchemaOverlay};

#[path = "build/registry_gen.rs"]
mod registry_gen;

#[path = "build/doc_gen.rs"]
mod doc_gen;

fn workspace_dir() -> PathBuf {
    let output = std::process::Command::new(env!("CARGO"))
        .arg("locate-project")
        .arg("--workspace")
        .arg("--message-format=plain")
        .output()
        .unwrap()
        .stdout;
    #[allow(clippy::disallowed_methods)] // not production code
    let cargo_path = Path::new(std::str::from_utf8(&output).unwrap().trim());
    cargo_path.parent().unwrap().to_path_buf()
}

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let doc_dir = workspace_dir()
        .join("docs")
        .join("agent-data-plane")
        .join("configuration");
    let template_path = doc_dir.join("configuration.md.tmpl");
    let doc_target = doc_dir.join("configuration.md");
    let generated_dir = manifest_dir.join("src/config_registry/generated");

    let files = Files::default();

    let core_schema_dir = files
        .datadog_schema
        .parent()
        .expect("core schema file must have a parent directory");
    println!("cargo:rerun-if-changed={}", core_schema_dir.display());
    println!("cargo:rerun-if-changed={}", files.otel_schema_dir.display());
    println!("cargo:rerun-if-changed={}", files.overlay.display());
    println!("cargo:rerun-if-changed={}", template_path.display());
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build/registry_gen.rs");
    println!("cargo:rerun-if-changed=build/doc_gen.rs");
    // The output is untracked, so watch it too: a `git clean` or a stray deletion has to bring the
    // generator back rather than fail the crate on a missing `include!`.
    println!("cargo:rerun-if-changed=src/config_registry/generated");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    let schema_map = schema_gen::load_schema(&files.datadog_schema, &files.otel_schema_dir);
    let overlay = SchemaOverlay::load(files).unwrap_or_else(|e| panic!("{e}"));

    // The annotation tables restate the overlay, so they are generated beside their hand-written
    // module and left untracked: readable and greppable in a built checkout, absent from diffs.
    std::fs::create_dir_all(&generated_dir).unwrap();
    schema_gen::generate_schema_rs(&schema_map, &generated_dir);
    registry_gen::generate(&overlay, &schema_map, &generated_dir);

    // Generate documentation markdown.
    doc_gen::generate(&overlay, &template_path, &out_dir);
    write_generated_doc(&out_dir, &doc_target);
}

fn write_generated_doc(out_dir: &Path, dst: &Path) {
    let src = out_dir.join("docs/configuration.md");
    let content = std::fs::read(&src).unwrap_or_else(|e| panic!("cannot read {}: {}", src.display(), e));
    std::fs::write(dst, content).unwrap_or_else(|e| panic!("cannot write {}: {}", dst.display(), e));
}
