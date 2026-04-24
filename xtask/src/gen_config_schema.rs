use std::fmt::Write as _;
use std::path::PathBuf;

use serde::Deserialize;
use serde_yaml::Value;

// Locate the workspace root by walking up from the xtask binary location.
fn workspace_root() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest).parent().unwrap().to_path_buf()
}

pub fn run() {
    let root = workspace_root();
    let schema_path = root.join("lib/saluki-components/vendor/core_schema.yaml");
    let out_path = root.join("lib/saluki-components/src/config_registry/generated/schema.rs");

    let src = std::fs::read_to_string(&schema_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", schema_path.display(), e));

    let doc: Value = serde_yaml::from_str(&src).unwrap_or_else(|e| panic!("failed to parse schema YAML: {}", e));

    let properties = doc
        .get("properties")
        .and_then(|v| v.as_mapping())
        .expect("schema root must have a 'properties' mapping");

    let mut entries: Vec<SchemaEntry> = Vec::new();
    collect_entries(properties, &[], &mut entries);
    entries.sort_by(|a, b| a.yaml_path.cmp(&b.yaml_path));

    let output = render(&entries);
    std::fs::create_dir_all(out_path.parent().unwrap()).unwrap();
    std::fs::write(&out_path, output).unwrap_or_else(|e| panic!("failed to write {}: {}", out_path.display(), e));

    // Format the output so it passes the pre-commit fmt check without a separate step.
    let fmt_status = std::process::Command::new("rustup")
        .args(["run", "nightly", "rustfmt", "--edition=2021"])
        .arg(&out_path)
        .status()
        .unwrap_or_else(|e| panic!("failed to run rustfmt: {}", e));
    if !fmt_status.success() {
        panic!("rustfmt failed on {}", out_path.display());
    }

    println!("Generated {} entries -> {}", entries.len(), out_path.display());
}

// ── Schema entry (one per setting leaf) ─────────────────────────────────────

#[derive(Debug)]
struct SchemaEntry {
    yaml_path: String,
    const_name: String,
    env_vars: Vec<String>,
    value_type: SchemaValueType,
    /// JSON-encoded default value from the schema, if present (e.g. `"true"`, `"0"`, `"\"datadoghq.com\""`).
    default: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchemaValueType {
    String,
    Bool,
    Integer,
    Float,
    StringList,
    Unknown,
}

impl SchemaValueType {
    fn as_rust(&self) -> &'static str {
        match self {
            SchemaValueType::String => "ValueType::String",
            SchemaValueType::Bool => "ValueType::Bool",
            SchemaValueType::Integer => "ValueType::Integer",
            SchemaValueType::Float => "ValueType::Float",
            SchemaValueType::StringList => "ValueType::StringList",
            SchemaValueType::Unknown => "ValueType::String", // safe fallback; annotate to correct
        }
    }

    fn is_unknown(&self) -> bool {
        matches!(self, SchemaValueType::Unknown)
    }
}

// ── YAML traversal ───────────────────────────────────────────────────────────

fn collect_entries(mapping: &serde_yaml::Mapping, path_parts: &[&str], out: &mut Vec<SchemaEntry>) {
    for (key, value) in mapping {
        let key_str = match key.as_str() {
            Some(s) => s,
            None => continue,
        };

        let mut parts = path_parts.to_vec();
        parts.push(key_str);

        let node_type = value.get("node_type").and_then(|v| v.as_str()).unwrap_or("");

        match node_type {
            "setting" => {
                if let Some(entry) = parse_setting(&parts, value) {
                    out.push(entry);
                }
            }
            "section" => {
                if let Some(props) = value.get("properties").and_then(|v| v.as_mapping()) {
                    collect_entries(props, &parts, out);
                }
            }
            _ => {
                // Unknown node_type — try to recurse if there are properties, otherwise skip.
                if let Some(props) = value.get("properties").and_then(|v| v.as_mapping()) {
                    collect_entries(props, &parts, out);
                }
            }
        }
    }
}

fn parse_setting(path_parts: &[&str], value: &Value) -> Option<SchemaEntry> {
    let yaml_path = path_parts.join(".");
    let const_name = yaml_path_to_const(&yaml_path);

    // Env vars: use explicit list if present. If the key has `no-env` tag OR no env_vars
    // field, the list is empty. Annotations can override.
    let has_no_env_tag = value
        .get("tags")
        .and_then(|v| v.as_sequence())
        .map(|tags| tags.iter().any(|t| t.as_str() == Some("no-env")))
        .unwrap_or(false);

    let env_vars: Vec<String> = if has_no_env_tag {
        Vec::new()
    } else {
        value
            .get("env_vars")
            .and_then(|v| v.as_sequence())
            .map(|seq| seq.iter().filter_map(|v| v.as_str()).map(|s| s.to_string()).collect())
            .unwrap_or_default()
    };

    let value_type = parse_value_type(value);
    let default = value.get("default").and_then(yaml_value_to_json_str);

    Some(SchemaEntry {
        yaml_path,
        const_name,
        env_vars,
        value_type,
        default,
    })
}

fn parse_value_type(value: &Value) -> SchemaValueType {
    let type_str = value.get("type").and_then(|v| v.as_str());

    match type_str {
        Some("string") => SchemaValueType::String,
        Some("boolean") => SchemaValueType::Bool,
        Some("integer") => SchemaValueType::Integer,
        Some("number") => SchemaValueType::Float,
        Some("array") => {
            // Only map to StringList if items are strings. Anything else is Unknown.
            let item_type = value.get("items").and_then(|v| v.get("type")).and_then(|v| v.as_str());
            if item_type == Some("string") {
                SchemaValueType::StringList
            } else {
                SchemaValueType::Unknown
            }
        }
        Some("object") => SchemaValueType::Unknown,
        _ => SchemaValueType::Unknown,
    }
}

/// Convert a YAML scalar default value to a JSON string representation suitable
/// for embedding as a Rust `&'static str` literal (e.g. `true` → `"true"`,
/// `"datadoghq.com"` → `"\"datadoghq.com\""`). Complex nested structures are
/// skipped since the smoke tests only need primitive defaults.
fn yaml_value_to_json_str(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::Null => None,
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::String(s) => {
            // Produce a JSON string literal (with surrounding quotes, inner quotes escaped).
            let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
            Some(format!("\"{}\"", escaped))
        }
        serde_yaml::Value::Sequence(seq) if seq.is_empty() => Some("[]".to_string()),
        serde_yaml::Value::Mapping(map) if map.is_empty() => Some("{}".to_string()),
        // Non-empty arrays/objects: skip — too complex to embed reliably.
        _ => None,
    }
}

// ── Identifier helpers ───────────────────────────────────────────────────────

fn yaml_path_to_const(yaml_path: &str) -> String {
    yaml_path.replace('.', "_").to_uppercase()
}

// ── Code rendering ───────────────────────────────────────────────────────────

fn render(entries: &[SchemaEntry]) -> String {
    let mut out = String::new();

    writeln!(out, "// @generated — DO NOT EDIT").unwrap();
    writeln!(out, "// Regenerate with: cargo xtask gen-config-schema").unwrap();
    writeln!(out, "// Source: lib/saluki-components/vendor/core_schema.yaml").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "#![allow(dead_code)]").unwrap();
    writeln!(out, "#![allow(missing_docs)]").unwrap();
    writeln!(out, "#![allow(non_upper_case_globals)]").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "use crate::config_registry::{{SchemaEntry, ValueType}};").unwrap();
    writeln!(out).unwrap();

    for entry in entries {
        // Emit a comment if the type had to fall back to Unknown.
        if entry.value_type.is_unknown() {
            writeln!(
                out,
                "// TODO: value_type unknown for '{}' — set an override in the annotation",
                entry.yaml_path
            )
            .unwrap();
        }

        let env_vars_literal = if entry.env_vars.is_empty() {
            "&[]".to_string()
        } else {
            let items: Vec<String> = entry.env_vars.iter().map(|e| format!("\"{}\"", e)).collect();
            format!("&[{}]", items.join(", "))
        };

        let default_literal = match &entry.default {
            Some(d) => format!("Some(\"{}\")", d.replace('\\', "\\\\").replace('"', "\\\"")),
            None => "None".to_string(),
        };

        writeln!(out, "pub const {}: SchemaEntry = SchemaEntry {{", entry.const_name).unwrap();
        writeln!(out, "    yaml_path: \"{}\",", entry.yaml_path).unwrap();
        writeln!(out, "    env_vars: {},", env_vars_literal).unwrap();
        writeln!(out, "    value_type: {},", entry.value_type.as_rust()).unwrap();
        writeln!(out, "    default: {},", default_literal).unwrap();
        writeln!(out, "}};").unwrap();
        writeln!(out).unwrap();
    }

    out
}

// ── Serde types (unused directly; here for future structured parsing) ────────

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct RawNode {
    node_type: Option<String>,
    #[serde(rename = "type")]
    type_: Option<String>,
    env_vars: Option<Vec<String>>,
    tags: Option<Vec<String>>,
    properties: Option<std::collections::HashMap<String, serde_yaml::Value>>,
}
