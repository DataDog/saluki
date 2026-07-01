//! Schema loading and Rust codegen for the Datadog agent config schema.
//!
//! Parses `core_schema.yaml` (with any overlay applied upstream) into a flat map of
//! `yaml_path → FieldInfo`, then emits `schema.rs` containing one `SchemaEntry` constant
//! per config key. Used exclusively at build time.
//!
//! The schema may contain `$ref: <filename>` entries that reference subsystem schema files in
//! the same directory. These are resolved and inlined during loading.
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
    ///
    /// For `format: duration` fields the literal is normalized to an integer number of
    /// nanoseconds (rather than the schema's Go duration string) so it matches the form the
    /// Datadog Agent transmits over the config stream.
    pub default: Option<String>,
}

/// Load and flatten the schema at `schema_path` into a `yaml_path → FieldInfo` map.
///
/// Resolves `$ref: <filename>` entries by loading the referenced files from the same directory.
/// The map is sorted by key. Panics if the file cannot be read or parsed.
pub fn load_schema(schema_path: &Path) -> IndexMap<String, FieldInfo> {
    let doc = crate::load_resolved_schema(schema_path).unwrap_or_else(|e| panic!("failed to load schema: {e}"));
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

        // `$ref`s are inlined by `crate::load_resolved_schema` before we get here, so every node
        // is either a `setting`, a `section`, or some other container with `properties`.
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
    let is_duration = value.get("format").and_then(|v| v.as_str()) == Some("duration");
    let default = value.get("default").and_then(|d| {
        // Durations: the schema default is a Go duration string (for example, `10s`), but the
        // Datadog Agent transmits durations over the config stream as integer nanoseconds. Emit the
        // default in nanoseconds so the classifier's exact-equality check matches the Agent-supplied
        // value. Fall back to the string form if the default isn't a parseable duration.
        if is_duration {
            if let Some(nanos) = d.as_str().and_then(parse_go_duration_nanos) {
                return Some(nanos.to_string());
            }
        }
        yaml_value_to_json_str(d)
    });

    (
        yaml_path,
        FieldInfo {
            value_type,
            env_vars,
            default,
        },
    )
}

/// Parses a Go `time.Duration` string (for example, `"10s"`, `"500ms"`, `"1h30m0s"`) into
/// nanoseconds.
///
/// Supports the unit suffixes Go's `time.ParseDuration` accepts (`ns`, `us`/`µs`/`μs`, `ms`, `s`,
/// `m`, `h`), an optional leading sign, and fractional components. Returns `None` if the string is
/// not a well-formed duration.
fn parse_go_duration_nanos(input: &str) -> Option<i128> {
    let mut s = input.trim();
    if s.is_empty() {
        return None;
    }

    let negative = match s.as_bytes()[0] {
        b'-' => {
            s = &s[1..];
            true
        }
        b'+' => {
            s = &s[1..];
            false
        }
        _ => false,
    };

    // Go accepts a bare "0" (no unit) as a special case.
    if s == "0" {
        return Some(0);
    }

    let mut total_nanos: f64 = 0.0;
    let mut chars = s.char_indices().peekable();
    let mut consumed_any = false;

    while chars.peek().is_some() {
        // Parse the numeric portion (digits with an optional single decimal point).
        let start = chars.peek().map(|&(idx, _)| idx)?;
        let mut end = start;
        let mut seen_digit = false;
        let mut seen_dot = false;
        while let Some(&(idx, c)) = chars.peek() {
            if c.is_ascii_digit() {
                seen_digit = true;
                end = idx + c.len_utf8();
                chars.next();
            } else if c == '.' && !seen_dot {
                seen_dot = true;
                end = idx + c.len_utf8();
                chars.next();
            } else {
                break;
            }
        }
        if !seen_digit {
            return None;
        }
        let number: f64 = s[start..end].parse().ok()?;

        // Parse the unit portion (letters, including the micro sign variants).
        let unit_start = end;
        let mut unit_end = end;
        while let Some(&(idx, c)) = chars.peek() {
            if c.is_ascii_alphabetic() || c == 'µ' || c == 'μ' {
                unit_end = idx + c.len_utf8();
                chars.next();
            } else {
                break;
            }
        }
        let unit = &s[unit_start..unit_end];
        let unit_nanos = match unit {
            "ns" => 1.0,
            "us" | "µs" | "μs" => 1_000.0,
            "ms" => 1_000_000.0,
            "s" => 1_000_000_000.0,
            "m" => 60.0 * 1_000_000_000.0,
            "h" => 3_600.0 * 1_000_000_000.0,
            _ => return None,
        };

        total_nanos += number * unit_nanos;
        consumed_any = true;
    }

    if !consumed_any {
        return None;
    }

    let total_nanos = total_nanos.round();
    Some(if negative {
        -(total_nanos as i128)
    } else {
        total_nanos as i128
    })
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

#[cfg(test)]
mod tests {
    use super::parse_go_duration_nanos;

    #[test]
    fn parse_go_duration_nanos_handles_common_forms() {
        assert_eq!(parse_go_duration_nanos("10s"), Some(10_000_000_000));
        assert_eq!(parse_go_duration_nanos("500ms"), Some(500_000_000));
        assert_eq!(parse_go_duration_nanos("1m0s"), Some(60_000_000_000));
        assert_eq!(parse_go_duration_nanos("1h30m0s"), Some(5_400_000_000_000));
        assert_eq!(parse_go_duration_nanos("24h0m0s"), Some(86_400_000_000_000));
        assert_eq!(parse_go_duration_nanos("0s"), Some(0));
        assert_eq!(parse_go_duration_nanos("0"), Some(0));
        assert_eq!(parse_go_duration_nanos("1.5h"), Some(5_400_000_000_000));
        assert_eq!(parse_go_duration_nanos("250µs"), Some(250_000));
        assert_eq!(parse_go_duration_nanos("-5s"), Some(-5_000_000_000));
        assert_eq!(parse_go_duration_nanos("nonsense"), None);
        assert_eq!(parse_go_duration_nanos("10"), None);
        assert_eq!(parse_go_duration_nanos(""), None);
    }
}
