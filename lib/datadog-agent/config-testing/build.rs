use std::path::{Path, PathBuf};

use datadog_agent_config_overlay_model::{schema_gen, Files, SchemaOverlay};

#[path = "build/registry_gen.rs"]
mod registry_gen;

#[path = "build/doc_gen.rs"]
mod doc_gen;

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let doc_dir = manifest_dir
        .join("..")
        .join("..")
        .join("..")
        .join("docs")
        .join("agent-data-plane")
        .join("configuration");
    let template_path = doc_dir.join("dogstatsd.md.tmpl");
    let doc_target = doc_dir.join("dogstatsd.md");
    let config_registry_dir = manifest_dir.join("src/config_registry");

    let files = Files::default();

    println!("cargo:rerun-if-changed={}", files.schema.display());
    println!("cargo:rerun-if-changed={}", files.overlay.display());
    println!("cargo:rerun-if-changed={}", template_path.display());
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build/registry_gen.rs");
    println!("cargo:rerun-if-changed=build/doc_gen.rs");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    let schema_path = files.schema.clone();
    let overlay = SchemaOverlay::load(files).unwrap_or_else(|e| panic!("{e}"));
    let schema_map = schema_gen::load_schema(&schema_path);

    // schema.rs is ~480KB — stays in OUT_DIR, never committed.
    let registry_out_dir = out_dir.join("config_registry");
    std::fs::create_dir_all(&registry_out_dir).unwrap();
    schema_gen::generate_schema_rs(&schema_map, &registry_out_dir);

    // Generate annotation subsystem files + annotations_index.rs in-tree for PR diff visibility.
    registry_gen::generate_in_tree(&overlay, &schema_map, &config_registry_dir);

    // Generate documentation markdown.
    doc_gen::generate(&overlay, &template_path, &out_dir);
    write_generated_doc(&out_dir, &doc_target);
}

fn write_generated_doc(out_dir: &Path, dst: &Path) {
    let src = out_dir.join("docs/dogstatsd.md");
    let content = std::fs::read(&src).unwrap_or_else(|e| panic!("cannot read {}: {}", src.display(), e));
    std::fs::write(dst, content).unwrap_or_else(|e| panic!("cannot write {}: {}", dst.display(), e));
}
