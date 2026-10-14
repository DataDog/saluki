//! Schema loading for the Datadog agent config schema.
//!
//! Parses `core_schema.yaml` (with any overlay applied upstream) into a flat map of
//! `yaml_path → FieldInfo`. Used exclusively at build time.
//!
//! The schema may contain `$ref: <filename>` entries that reference subsystem schema files in
//! the same directory. These are resolved and inlined during loading.
use std::path::Path;

use indexmap::IndexMap;
use serde_json::{Map, Value};

/// Value type of a config field, as declared in the schema YAML.
///
/// `Unknown` is assigned when the YAML `type` is absent or unrecognised.
pub enum FieldType {
    String,
    Bool,
    Integer,
    Float,
    StringList,
    Duration,
    Unknown,
}

/// A named environment-variable parser declared by the schema's `env_parser` key.
///
/// The Datadog Agent registers a bespoke transform for these settings instead of the default
/// type-based cast. Codegen maps supported parsers onto runtime decoders and rejects unsupported
/// parsers for modeled keys. A setting without `env_parser` falls back to its declared type.
pub enum EnvParser {
    Json,
    CommaSeparated,
    CsvCommaSeparated,
    CommaAndSpaceSeparated,
    CommaThenSpaceSeparated,
    JsonListOrCommaSeparated,
    JsonListOrSpaceSeparated,
    TracesSpan,
}

impl EnvParser {
    /// Parses a schema `env_parser` value, panicking on an unrecognized name so a not-modeled parser
    /// fails the build rather than silently degrading to the type-based fallback.
    fn parse(name: &str) -> Self {
        match name {
            "json" => EnvParser::Json,
            "comma_separated" => EnvParser::CommaSeparated,
            "csv_comma_separated" => EnvParser::CsvCommaSeparated,
            "comma_and_space_separated" => EnvParser::CommaAndSpaceSeparated,
            "comma_then_space_separated" => EnvParser::CommaThenSpaceSeparated,
            "json_list_or_comma_separated" => EnvParser::JsonListOrCommaSeparated,
            "json_list_or_space_separated" => EnvParser::JsonListOrSpaceSeparated,
            "traces_span" => EnvParser::TracesSpan,
            other => panic!("unknown env_parser `{other}` in schema; add it to EnvParser and env_decode"),
        }
    }
}

/// How a config field is reachable through an environment variable.
///
/// Mirrors the Datadog Agent's `bindEnv`: a setting either has no env var, uses the standard
/// `DD_` + `UPPER(dotted_path)` name derived from its key, or declares explicit names that
/// replace that derived default. The distinction matters to codegen: the standard name is
/// computable from the path, an overridden name is not, and a field with no env var must never be
/// treated as env-reachable.
pub enum EnvBinding {
    /// The field carries a `no-env` tag: no environment variable maps to it.
    None,
    /// No explicit env vars are declared, so the Agent derives `DD_` + `UPPER(dotted_path)`.
    Standard,
    /// The schema declares explicit environment variable names, which replace the derived default. Non-empty.
    Overridden(Vec<String>),
}

/// Parsed metadata for a single config field.
pub struct FieldInfo {
    /// Resolved value type (see [`FieldType`]).
    pub value_type: FieldType,
    /// How the field is reachable through an environment variable (see [`EnvBinding`]).
    pub env: EnvBinding,
    /// The schema's named `env_parser`, if any (see [`EnvParser`]); `None` uses the type fallback.
    pub env_parser: Option<EnvParser>,
    /// Default value serialised as a JSON literal, or `None` if the schema omits one.
    pub default: Option<String>,
}

/// Load and flatten the schema at `schema_path` into a `yaml_path → FieldInfo` map.
///
/// Resolves `$ref: <filename>` entries by loading the referenced files from the same directory.
/// The map is sorted by key. Panics if the file cannot be read or parsed.
pub fn load_schema(datadog_schema: &Path, otel_schema_dir: &Path) -> IndexMap<String, FieldInfo> {
    let doc = crate::load_composed_schema(datadog_schema, otel_schema_dir)
        .unwrap_or_else(|e| panic!("failed to load composed schema: {e}"));
    load_schema_from_value(&doc)
}

/// Builds the flat `yaml_path → FieldInfo` map from a pre-loaded composed schema.
///
/// Callers that already hold the composed schema should use this instead of [`load_schema`] to
/// avoid a redundant disk read and guarantee every consumer sees the same document.
pub fn load_schema_from_value(doc: &Value) -> IndexMap<String, FieldInfo> {
    let properties = doc
        .get("properties")
        .and_then(|v| v.as_object())
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

fn collect_entries(mapping: &Map<String, Value>, path_parts: &[&str], out: &mut Vec<(String, FieldInfo)>) {
    for (key, value) in mapping {
        let mut parts = path_parts.to_vec();
        parts.push(key);

        // `$ref`s are inlined by `crate::load_resolved_schema` before we get here, so every node
        // is either a `setting`, a `section`, or some other container with `properties`.
        let node_type = value.get("node_type").and_then(|v| v.as_str()).unwrap_or("");

        match node_type {
            "setting" => out.push(parse_setting(&parts, value)),
            "section" => {
                if let Some(props) = value.get("properties").and_then(|v| v.as_object()) {
                    collect_entries(props, &parts, out);
                }
            }
            _ => {
                if let Some(props) = value.get("properties").and_then(|v| v.as_object()) {
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
        .and_then(|v| v.as_array())
        .map(|tags| tags.iter().any(|t| t.as_str() == Some("no-env")))
        .unwrap_or(false);

    let explicit_env_vars: Vec<String> = value
        .get("env_vars")
        .and_then(|v| v.as_array())
        .map(|seq| seq.iter().filter_map(|v| v.as_str()).map(|s| s.to_string()).collect())
        .unwrap_or_default();

    // A `no-env` tag wins over any declared names: the field is not env-reachable at all.
    let env = if has_no_env_tag {
        EnvBinding::None
    } else if explicit_env_vars.is_empty() {
        EnvBinding::Standard
    } else {
        EnvBinding::Overridden(explicit_env_vars)
    };

    let value_type = parse_value_type(value);
    let default = value.get("default").and_then(yaml_value_to_json_str);
    let env_parser = value.get("env_parser").and_then(|v| v.as_str()).map(EnvParser::parse);

    (
        yaml_path,
        FieldInfo {
            value_type,
            env,
            env_parser,
            default,
        },
    )
}

fn parse_value_type(value: &Value) -> FieldType {
    let ty = value.get("type").and_then(|v| v.as_str());
    let format = value.get("format").and_then(|v| v.as_str());

    // `format: duration` folds into a single effective `Duration` type regardless of the declared
    // base type. The vendored Agent schema currently declares these as `type: number` (nanoseconds
    // on the wire) but has also used `type: string` (Go duration text); both are durations to us.
    if format == Some("duration") {
        return match ty {
            Some("string") | Some("number") | Some("integer") => FieldType::Duration,
            other => panic!(
                "config schema field has `format: duration` with unsupported base type {:?}; \
                 expected string, number, or integer",
                other
            ),
        };
    }

    match ty {
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

fn yaml_value_to_json_str(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => {
            let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
            Some(format!("\"{}\"", escaped))
        }
        Value::Array(seq) => {
            let items: Option<Vec<String>> = seq.iter().map(yaml_value_to_json_str).collect();
            items.map(|elems| format!("[{}]", elems.join(",")))
        }
        Value::Object(map) if map.is_empty() => Some("{}".to_string()),
        _ => None,
    }
}

/// Escape backslashes and double-quotes in `s` for use inside a Rust string literal.
pub fn escape_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
