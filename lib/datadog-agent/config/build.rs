use std::path::PathBuf;

use datadog_agent_config_overlay_model::{schema_gen, Files, SchemaOverlay};

#[path = "build/classifier_gen.rs"]
mod classifier_gen;
#[path = "build/datadog_config_gen.rs"]
mod datadog_config_gen;

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let files = Files::default();

    let schema_dir = files.schema.parent().expect("schema file must have a parent directory");
    println!("cargo:rerun-if-changed={}", schema_dir.display());
    println!("cargo:rerun-if-changed={}", files.overlay.display());
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build/classifier_gen.rs");
    println!("cargo:rerun-if-changed=build/datadog_config_gen.rs");

    let schema_path = files.schema.clone();
    let overlay = SchemaOverlay::load(files).unwrap_or_else(|e| panic!("{e}"));
    let schema_map = schema_gen::load_schema(&schema_path);

    classifier_gen::generate(&overlay, &schema_map, &manifest_dir);
    datadog_config_gen::generate(&overlay, &schema_path, &manifest_dir);
}
