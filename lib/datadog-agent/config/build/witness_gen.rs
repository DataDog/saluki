use std::collections::HashSet;
use std::path::Path;

use datadog_agent_config_overlay_model::{KnownEntry, SchemaOverlay};

pub fn generate(overlay: &SchemaOverlay, manifest_dir: &Path) {
    let mut seen = HashSet::new();
    let keys = overlay
        .inventory
        .iter()
        .filter(|(_, entry)| matches!(entry, KnownEntry::Full(_) | KnownEntry::Partial(_)))
        .map(|(key, _)| {
            let method = method_name(key);
            if !seen.insert(method.clone()) {
                panic!("duplicate witness method name `{method}` generated for `{key}`");
            }
            (key.as_str(), method)
        })
        .collect::<Vec<_>>();

    let mut out = String::new();
    out.push_str("// @generated from schema_overlay.yaml — DO NOT EDIT BY HAND.\n");
    out.push_str("use std::fmt;\n\nuse serde_json::Value;\n\n");
    out.push_str("use crate::DatadogConfiguration;\n\n");
    out.push_str("/// Error returned while driving Datadog configuration into a native consumer.\n");
    out.push_str("#[derive(Debug)]\n");
    out.push_str("pub struct TranslateError {\n    message: String,\n}\n\n");
    out.push_str("impl TranslateError {\n");
    out.push_str("    /// Creates a translation error from a message.\n");
    out.push_str(
        "    pub fn new(message: impl Into<String>) -> Self {\n        Self { message: message.into() }\n    }\n}\n\n",
    );
    out.push_str("impl fmt::Display for TranslateError {\n");
    out.push_str("    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {\n        f.write_str(&self.message)\n    }\n}\n\n");
    out.push_str("impl std::error::Error for TranslateError {}\n\n");
    out.push_str("impl From<serde_json::Error> for TranslateError {\n");
    out.push_str("    fn from(error: serde_json::Error) -> Self {\n        Self::new(error.to_string())\n    }\n}\n\n");
    out.push_str("/// Result type for Datadog configuration translation.\n");
    out.push_str("pub type TranslateResult = Result<(), TranslateError>;\n\n");
    out.push_str("/// Witness consumer for every Datadog config key with full or partial support.\n");
    out.push_str("pub trait DatadogConfigConsumer {\n");
    for (key, method) in &keys {
        out.push_str(&format!("    /// Consumes `{key}`.\n"));
        out.push_str(&format!(
            "    fn consume_{method}(&mut self, value: Option<Value>) -> TranslateResult;\n\n"
        ));
    }
    out.push_str("}\n\n");
    out.push_str("/// Drives every supported Datadog key into `consumer`.\n");
    out.push_str(
        "pub fn drive(config: &DatadogConfiguration, consumer: &mut impl DatadogConfigConsumer) -> TranslateResult {\n",
    );
    out.push_str("    let value = serde_json::to_value(config)?;\n");
    for (key, method) in &keys {
        out.push_str(&format!(
            "    consumer.consume_{method}(lookup(&value, &[{}]))?;\n",
            rust_string_slice(key)
        ));
    }
    out.push_str("    Ok(())\n}\n\n");
    out.push_str("fn lookup(value: &Value, path: &[&str]) -> Option<Value> {\n");
    out.push_str("    let mut current = value;\n");
    out.push_str("    for segment in path {\n        current = current.get(*segment)?;\n    }\n");
    out.push_str("    Some(current.clone())\n}\n");

    let path = manifest_dir.join("src/witness.rs");
    std::fs::write(&path, out).unwrap_or_else(|e| panic!("cannot write {}: {e}", path.display()));
    let _ = std::process::Command::new("rustfmt")
        .arg("--edition")
        .arg("2021")
        .arg(&path)
        .status();
}

fn method_name(key: &str) -> String {
    let mut out = String::new();
    let mut last_was_sep = true;
    for ch in key.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_sep = false;
        } else if !last_was_sep {
            out.push('_');
            last_was_sep = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
        out.insert_str(0, "key_");
    }
    out
}

fn rust_string_slice(key: &str) -> String {
    key.split('.')
        .map(|segment| format!("\"{}\"", segment.escape_default()))
        .collect::<Vec<_>>()
        .join(", ")
}
