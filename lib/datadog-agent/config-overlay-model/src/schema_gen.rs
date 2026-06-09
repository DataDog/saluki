//! Schema loading and Rust codegen for the Datadog agent config schema.
//!
//! Parses `core_schema.yaml` (with any overlay applied upstream) into a flat map of
//! `yaml_path → FieldInfo`, then emits `schema.rs` containing one `SchemaEntry` constant
//! per config key. Used exclusively at build time.
use std::path::Path;

use indexmap::IndexMap;
use serde_yaml::Value;

/// Value type of a config field, as declared in the schema YAML.
///
/// `Unknown` is assigned when the YAML `type` is absent or unrecognised; affected fields
/// are emitted with a `// TODO` comment in the generated output.
pub enum FieldType {
    String,
    Bool,
    Integer,
    Float,
    StringList,
    Unknown,
}

/// Parsed metadata for a single config field.
pub struct FieldInfo {
    /// Resolved value type (see [`FieldType`]).
    pub value_type: FieldType,
    /// Environment variable names that map to this field. Empty when the field carries a
    /// `no-env` tag.
    pub env_vars: Vec<String>,
    /// Default value serialised as a JSON literal, or `None` if the schema omits one.
    pub default: Option<String>,
}

/// Load and flatten the schema at `schema_path` into a `yaml_path → FieldInfo` map.
///
/// The map is sorted by key. Panics if the file cannot be read or parsed.
pub fn load_schema(schema_path: &Path) -> IndexMap<String, FieldInfo> {
    let src = std::fs::read_to_string(schema_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", schema_path.display(), e));
    let doc: Value = serde_yaml::from_str(&src).unwrap_or_else(|e| panic!("failed to parse schema YAML: {}", e));
    let properties = doc
        .get("properties")
        .and_then(|v| v.as_mapping())
        .expect("schema root must have a 'properties' mapping");

    let mut entries = Vec::new();
    collect_entries(properties, &[], &mut entries);
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut map = IndexMap::new();
    for (yaml_path, info) in entries {
        map.insert(yaml_path, info);
    }
    map
}

fn collect_entries(mapping: &serde_yaml::Mapping, path_parts: &[&str], out: &mut Vec<(String, FieldInfo)>) {
    for (key, value) in mapping {
        let key_str = match key.as_str() {
            Some(s) => s,
            None => continue,
        };

        let mut parts = path_parts.to_vec();
        parts.push(key_str);

        let node_type = value.get("node_type").and_then(|v| v.as_str()).unwrap_or("");

        match node_type {
            "setting" => out.push(parse_setting(&parts, value)),
            "section" => {
                if let Some(props) = value.get("properties").and_then(|v| v.as_mapping()) {
                    collect_entries(props, &parts, out);
                }
            }
            _ => {
                if let Some(props) = value.get("properties").and_then(|v| v.as_mapping()) {
                    collect_entries(props, &parts, out);
                }
            }
        }
    }
}

fn parse_setting(path_parts: &[&str], value: &Value) -> (String, FieldInfo) {
    let yaml_path = path_parts.join(".");

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

    (
        yaml_path,
        FieldInfo {
            value_type,
            env_vars,
            default,
        },
    )
}

fn parse_value_type(value: &Value) -> FieldType {
    match value.get("type").and_then(|v| v.as_str()) {
        Some("string") => FieldType::String,
        Some("boolean") => FieldType::Bool,
        Some("integer") => FieldType::Integer,
        Some("number") => FieldType::Float,
        Some("array") => {
            let item_type = value.get("items").and_then(|v| v.get("type")).and_then(|v| v.as_str());
            if item_type == Some("string") {
                FieldType::StringList
            } else {
                FieldType::Unknown
            }
        }
        _ => FieldType::Unknown,
    }
}

fn yaml_value_to_json_str(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::Null => None,
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::String(s) => {
            let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
            Some(format!("\"{}\"", escaped))
        }
        serde_yaml::Value::Sequence(seq) => {
            let items: Option<Vec<String>> = seq.iter().map(yaml_value_to_json_str).collect();
            items.map(|elems| format!("[{}]", elems.join(",")))
        }
        serde_yaml::Value::Mapping(map) if map.is_empty() => Some("{}".to_string()),
        _ => None,
    }
}

/// Return the `ValueType::*` token string for use in generated Rust source.
pub fn field_type_as_rust(ft: &FieldType) -> &'static str {
    match ft {
        FieldType::String | FieldType::Unknown => "ValueType::String",
        FieldType::Bool => "ValueType::Bool",
        FieldType::Integer => "ValueType::Integer",
        FieldType::Float => "ValueType::Float",
        FieldType::StringList => "ValueType::StringList",
    }
}

/// Return `true` if `ft` is [`FieldType::Unknown`].
pub fn is_unknown(ft: &FieldType) -> bool {
    matches!(ft, FieldType::Unknown)
}

/// Escape backslashes and double-quotes in `s` for use inside a Rust string literal.
pub fn escape_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Generate `schema.rs` in `dir` from `schema_map`.
///
/// The file contains one `pub const <NAME>: SchemaEntry = SchemaEntry { … };` block per
/// entry, sorted alphabetically. Panics if the file cannot be written.
pub fn generate_schema_rs(schema_map: &IndexMap<String, FieldInfo>, dir: &Path) {
    use std::fmt::Write as _;

    let mut out = String::new();
    writeln!(
        out,
        "// @generated by build.rs from core_schema.yaml + schema_overlay.yaml — DO NOT EDIT"
    )
    .unwrap();
    writeln!(out).unwrap();

    let mut keys: Vec<&str> = schema_map.keys().map(|s| s.as_str()).collect();
    keys.sort_unstable();

    for yaml_path in &keys {
        let info = &schema_map[*yaml_path];
        let const_name = yaml_path_to_const(yaml_path);
        let vt = field_type_as_rust(&info.value_type);

        if is_unknown(&info.value_type) {
            writeln!(
                out,
                "// TODO: unknown type for '{}' — set value_type_override in the annotation",
                yaml_path
            )
            .unwrap();
        }

        let env_vars_lit = if info.env_vars.is_empty() {
            "&[]".to_string()
        } else {
            let items: Vec<String> = info.env_vars.iter().map(|e| format!("\"{}\"", escape_str(e))).collect();
            format!("&[{}]", items.join(", "))
        };

        let default_lit = match &info.default {
            Some(d) => format!("Some(\"{}\")", escape_str(d)),
            None => "None".to_string(),
        };

        writeln!(out, "pub const {}: SchemaEntry = SchemaEntry {{", const_name).unwrap();
        writeln!(out, "    schema: Schema::Datadog,").unwrap();
        writeln!(out, "    yaml_path: \"{}\",", yaml_path).unwrap();
        writeln!(out, "    env_vars: {},", env_vars_lit).unwrap();
        writeln!(out, "    value_type: {},", vt).unwrap();
        writeln!(out, "    default: {},", default_lit).unwrap();
        writeln!(out, "}};").unwrap();
        writeln!(out).unwrap();
    }

    let path = dir.join("schema.rs");
    std::fs::write(&path, out).unwrap_or_else(|e| panic!("cannot write {}: {}", path.display(), e));
}

/// Convert a dotted YAML path (for example, `"dogstatsd.bind_host"`) to a `SCREAMING_SNAKE_CASE`
/// Rust identifier suitable for a `const` name.
pub fn yaml_path_to_const(yaml_path: &str) -> String {
    yaml_path
        .chars()
        .map(|c| if c == '.' || c == '-' { '_' } else { c })
        .collect::<String>()
        .to_uppercase()
}
