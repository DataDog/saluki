use std::fmt::Write as _;
use std::path::PathBuf;

use serde_yaml::Value;

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let schema_path = manifest_dir.join("vendor/core_schema.yaml");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let out_path = out_dir.join("schema.rs");

    // Only re-run when the schema source or this build script changes.
    println!("cargo:rerun-if-changed=vendor/core_schema.yaml");
    println!("cargo:rerun-if-changed=build.rs");

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
    std::fs::write(&out_path, output).unwrap_or_else(|e| panic!("failed to write {}: {}", out_path.display(), e));
}

// ── Schema entry (one per setting leaf) ─────────────────────────────────────

struct SchemaEntry {
    yaml_path: String,
    const_name: String,
    env_vars: Vec<String>,
    value_type: SchemaValueType,
    default: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SchemaValueType {
    String,
    Bool,
    Integer,
    Float,
    StringList,
    Unknown,
}

impl SchemaValueType {
    fn as_rust(self) -> &'static str {
        match self {
            SchemaValueType::String => "ValueType::String",
            SchemaValueType::Bool => "ValueType::Bool",
            SchemaValueType::Integer => "ValueType::Integer",
            SchemaValueType::Float => "ValueType::Float",
            SchemaValueType::StringList => "ValueType::StringList",
            SchemaValueType::Unknown => "ValueType::String", // safe fallback; annotate to correct
        }
    }

    fn is_unknown(self) -> bool {
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
    match value.get("type").and_then(|v| v.as_str()) {
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
        _ => SchemaValueType::Unknown,
    }
}

/// Convert a YAML scalar default to a JSON string for embedding as a Rust `&'static str`.
fn yaml_value_to_json_str(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::Null => None,
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::String(s) => {
            let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
            Some(format!("\"{}\"", escaped))
        }
        serde_yaml::Value::Sequence(seq) if seq.is_empty() => Some("[]".to_string()),
        serde_yaml::Value::Mapping(map) if map.is_empty() => Some("{}".to_string()),
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

    writeln!(
        out,
        "// @generated by build.rs from vendor/core_schema.yaml — DO NOT EDIT"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "use crate::config_registry::{{SchemaEntry, ValueType}};").unwrap();
    writeln!(out).unwrap();

    for entry in entries {
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
