//! Loads Datadog Agent configuration schemas for build-time code generation.
//!
//! Resolves schema file references and incorporates the OTel receiver schema, producing a map
//! from dotted YAML paths to [`FieldInfo`].
use std::collections::HashSet;
use std::path::Path;

use indexmap::IndexMap;
use serde_json::{Map, Value};

/// Value type of a config field, as declared in the schema YAML.
///
/// `Unknown` covers missing or unsupported types, including arrays of non-string items.
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
    /// Former dotted names of the setting, from the schema's `renamed_from`, highest priority first.
    ///
    /// The Agent's codegen orders them by the version that deprecated each, oldest first, then by
    /// name. A `no-env` setting has none: the Agent declares it with `SetDefault`, which registers no
    /// former names.
    pub renamed_from: Vec<String>,
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
    let renamed_from = if has_no_env_tag {
        Vec::new()
    } else {
        parse_renamed_from(&yaml_path, value)
    };

    (
        yaml_path,
        FieldInfo {
            value_type,
            env,
            env_parser,
            default,
            renamed_from,
        },
    )
}

/// Parses `renamed_from`, a map of former name to the `MAJOR.MINOR.PATCH` Agent version that
/// deprecated it, into former names ordered as the Agent's `deprecated_names` orders them.
///
/// Panics on a malformed entry, which the Agent's schema lint also rejects.
fn parse_renamed_from(yaml_path: &str, value: &Value) -> Vec<String> {
    let Some(renamed_from) = value.get("renamed_from") else {
        return Vec::new();
    };
    let entries = renamed_from
        .as_object()
        .unwrap_or_else(|| panic!("`{yaml_path}`: `renamed_from` must map former names to versions"));
    let mut names: Vec<((u64, u64, u64), String)> = entries
        .iter()
        .map(|(name, version)| {
            let version = version
                .as_str()
                .and_then(parse_version)
                .unwrap_or_else(|| panic!("`{yaml_path}`: `renamed_from.{name}` must be a `MAJOR.MINOR.PATCH` string"));
            (version, name.clone())
        })
        .collect();
    names.sort();
    names.into_iter().map(|(_, name)| name).collect()
}

fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.').map(|part| {
        (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
            .then(|| part.parse().ok())
            .flatten()
    });
    let parsed = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(parsed)
}

/// A former name of a schema setting, from its `renamed_from`.
#[derive(Debug, PartialEq)]
pub struct FormerName<'a> {
    /// The former dotted name.
    pub former: &'a str,
    /// The dotted name of the setting.
    pub key: &'a str,
    /// The environment variable derived from the former name, if the environment reader reads the
    /// setting. It is also among the setting's [`env_var_names`].
    pub env_var: Option<String>,
}

/// Derives the Agent's standard environment name for a key: `DD_` + `UPPER(dotted_path)`.
pub fn standard_env_name(key: &str) -> String {
    format!("DD_{}", key.replace('.', "_").to_uppercase())
}

/// The environment variables the Agent reads for the setting at `key`, highest priority first.
///
/// Explicit `env_vars` are used verbatim, in declared order. Otherwise each former name derives a
/// name ahead of the key's own, as `BindEnvAndSetDefaultWithDeprecation` binds them. A `no-env`
/// setting, or a key without schema metadata, derives only its own name.
///
/// Panics when a setting has both `env_vars` and `renamed_from`.
pub fn env_var_names(key: &str, info: Option<&FieldInfo>) -> Vec<String> {
    match info.map(|i| &i.env) {
        Some(EnvBinding::Overridden(env_vars)) => {
            if info.is_some_and(|info| !info.renamed_from.is_empty()) {
                panic!(
                    "`{key}` has both `env_vars` and `renamed_from`: confirm which environment variables the \
                     Agent reads for its former names, and generate them here"
                );
            }
            env_vars.clone()
        }
        Some(EnvBinding::Standard) => info
            .into_iter()
            .flat_map(|i| &i.renamed_from)
            .map(String::as_str)
            .chain(std::iter::once(key))
            .map(standard_env_name)
            .collect(),
        None | Some(EnvBinding::None) => vec![standard_env_name(key)],
    }
}

/// Every former name of every setting in `schema`, grouped by setting and highest priority first
/// within a setting.
///
/// A former name's environment variable is derived for a `modeled` setting, which is the only kind
/// the environment reader reads.
///
/// Panics when a former name is a schema path, or a setting or another former name contains or holds
/// it, since the file reader could not tell which one a key addresses.
pub fn former_names<'a>(schema: &'a IndexMap<String, FieldInfo>, modeled: &HashSet<&str>) -> Vec<FormerName<'a>> {
    let leaves: HashSet<&str> = schema.keys().map(String::as_str).collect();
    let sections: HashSet<&str> = leaves.iter().flat_map(|key| proper_prefixes(key)).collect();
    let mut names: Vec<FormerName<'a>> = Vec::new();
    for (key, info) in schema {
        for former in &info.renamed_from {
            let clash = std::iter::once(former.as_str())
                .filter(|path| leaves.contains(path) || sections.contains(path))
                .chain(proper_prefixes(former).filter(|prefix| leaves.contains(prefix)))
                .next();
            if let Some(clash) = clash {
                panic!("former name `{former}` of `{key}` overlaps the schema path `{clash}`");
            }
            let other = names.iter().find(|other| {
                other.former == former
                    || proper_prefixes(former).any(|prefix| prefix == other.former)
                    || proper_prefixes(other.former).any(|prefix| prefix == former)
            });
            if let Some(other) = other {
                panic!(
                    "former name `{former}` of `{key}` overlaps the former name `{}` of `{}`",
                    other.former, other.key
                );
            }

            let env_var = match info.env {
                EnvBinding::Standard if modeled.contains(key.as_str()) => Some(standard_env_name(former)),
                _ => None,
            };
            names.push(FormerName { former, key, env_var });
        }
    }
    names
}

/// Every dotted proper prefix of `key`.
fn proper_prefixes(key: &str) -> impl Iterator<Item = &str> {
    key.match_indices('.').map(|(end, _)| &key[..end])
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn renamed_from(setting: Value) -> Vec<String> {
        parse_setting(&["new_name"], &setting).1.renamed_from
    }

    fn env_names(key: &str, setting: Value) -> Vec<String> {
        let path: Vec<&str> = key.split('.').collect();
        env_var_names(key, Some(&parse_setting(&path, &setting).1))
    }

    /// The flattened schema whose root `properties` are `properties`.
    fn schema(properties: Value) -> IndexMap<String, FieldInfo> {
        load_schema_from_value(&json!({ "properties": properties }))
    }

    fn setting(renamed_from: Value) -> Value {
        json!({"node_type": "setting", "type": "boolean", "renamed_from": renamed_from})
    }

    fn plain_setting() -> Value {
        json!({"node_type": "setting", "type": "boolean"})
    }

    #[test]
    fn former_names_are_ordered_by_deprecating_version_then_name() {
        let setting = json!({
            "node_type": "setting",
            "type": "boolean",
            "renamed_from": {"c_name": "7.10.0", "b_name": "7.9.1", "a_name": "7.10.0", "d_name": "6.99.0"},
        });

        assert_eq!(renamed_from(setting), ["d_name", "b_name", "a_name", "c_name"]);
    }

    #[test]
    fn a_setting_without_renamed_from_or_with_no_env_has_no_former_names() {
        assert!(renamed_from(json!({"node_type": "setting", "type": "boolean"})).is_empty());
        assert!(renamed_from(json!({
            "node_type": "setting",
            "type": "boolean",
            "tags": ["no-env"],
            "renamed_from": {"old_name": "7.86.0"},
        }))
        .is_empty());
    }

    #[test]
    #[should_panic(expected = "`renamed_from` must map former names to versions")]
    fn a_renamed_from_that_is_not_a_map_is_rejected() {
        renamed_from(json!({"node_type": "setting", "renamed_from": ["old_name"]}));
    }

    #[test]
    fn former_names_derive_environment_variables_ahead_of_the_current_name() {
        let names = env_names(
            "section.new_name",
            setting(json!({"legacy.old_name": "7.1.0", "oldest_name": "6.0.0"})),
        );

        assert_eq!(names, ["DD_OLDEST_NAME", "DD_LEGACY_OLD_NAME", "DD_SECTION_NEW_NAME"]);
        assert_eq!(env_names("new_name", plain_setting()), ["DD_NEW_NAME"]);
    }

    #[test]
    fn a_no_env_setting_or_unknown_key_derives_only_its_own_variable() {
        let no_env = json!({
            "node_type": "setting",
            "type": "boolean",
            "tags": ["no-env"],
            "renamed_from": {"old_name": "7.1.0"},
        });

        assert_eq!(env_names("new_name", no_env), ["DD_NEW_NAME"]);
        assert_eq!(env_var_names("new_name", None), ["DD_NEW_NAME"]);
    }

    #[test]
    fn explicit_environment_variables_are_read_verbatim() {
        let setting = json!({"node_type": "setting", "type": "boolean", "env_vars": ["DD_B", "DD_A"]});

        assert_eq!(env_names("new_name", setting), ["DD_B", "DD_A"]);
    }

    #[test]
    #[should_panic(expected = "`new_name` has both `env_vars` and `renamed_from`")]
    fn a_setting_with_explicit_environment_variables_and_former_names_is_rejected() {
        env_names(
            "new_name",
            json!({
                "node_type": "setting",
                "type": "boolean",
                "env_vars": ["DD_NEW_NAME"],
                "renamed_from": {"old_name": "7.1.0"},
            }),
        );
    }

    #[test]
    fn former_names_are_listed_per_setting_with_variables_for_modeled_settings_only() {
        let schema = schema(json!({
            "modeled": setting(json!({"old_modeled": "7.2.0", "oldest_modeled": "7.1.0"})),
            "section": {
                "node_type": "section",
                "properties": {"unmodeled": setting(json!({"legacy.old_unmodeled": "7.1.0"}))},
            },
            "untouched": plain_setting(),
        }));

        let names = former_names(&schema, &HashSet::from(["modeled", "untouched"]));

        let name = |former, key, env_var: Option<&str>| FormerName {
            former,
            key,
            env_var: env_var.map(str::to_owned),
        };
        assert_eq!(
            names,
            [
                name("oldest_modeled", "modeled", Some("DD_OLDEST_MODELED")),
                name("old_modeled", "modeled", Some("DD_OLD_MODELED")),
                name("legacy.old_unmodeled", "section.unmodeled", None),
            ]
        );
    }

    #[test]
    #[should_panic(expected = "former name `taken` of `new_name` overlaps the schema path `taken`")]
    fn a_former_name_that_is_a_setting_is_rejected() {
        former_names(
            &schema(json!({"new_name": setting(json!({"taken": "7.1.0"})), "taken": plain_setting()})),
            &HashSet::new(),
        );
    }

    #[test]
    #[should_panic(expected = "former name `section` of `new_name` overlaps the schema path `section`")]
    fn a_former_name_that_is_a_section_is_rejected() {
        former_names(
            &schema(json!({
                "new_name": setting(json!({"section": "7.1.0"})),
                "section": {"node_type": "section", "properties": {"leaf": plain_setting()}},
            })),
            &HashSet::new(),
        );
    }

    #[test]
    #[should_panic(expected = "former name `taken.old_name` of `new_name` overlaps the schema path `taken`")]
    fn a_former_name_inside_a_setting_is_rejected() {
        former_names(
            &schema(json!({"new_name": setting(json!({"taken.old_name": "7.1.0"})), "taken": plain_setting()})),
            &HashSet::new(),
        );
    }

    #[test]
    #[should_panic(expected = "former name `old.inner` of `second` overlaps the former name `old` of `first`")]
    fn former_names_that_hold_each_other_are_rejected() {
        former_names(
            &schema(json!({
                "first": setting(json!({"old": "7.1.0"})),
                "second": setting(json!({"old.inner": "7.1.0"})),
            })),
            &HashSet::new(),
        );
    }

    #[test]
    #[should_panic(expected = "former name `old` of `second` overlaps the former name `old.inner` of `first`")]
    fn a_former_name_that_holds_an_earlier_former_name_is_rejected() {
        former_names(
            &schema(json!({
                "first": setting(json!({"old.inner": "7.1.0"})),
                "second": setting(json!({"old": "7.1.0"})),
            })),
            &HashSet::new(),
        );
    }

    #[test]
    #[should_panic(expected = "former name `old` of `second` overlaps the former name `old` of `first`")]
    fn a_former_name_of_two_settings_is_rejected() {
        former_names(
            &schema(json!({
                "first": setting(json!({"old": "7.1.0"})),
                "second": setting(json!({"old": "7.1.0"})),
            })),
            &HashSet::new(),
        );
    }

    #[test]
    #[should_panic(expected = "must be a `MAJOR.MINOR.PATCH` string")]
    fn a_partial_version_is_rejected() {
        renamed_from(json!({"node_type": "setting", "renamed_from": {"old_name": "7.86"}}));
    }

    #[test]
    fn versions_parse_only_in_full_semantic_form() {
        assert_eq!(parse_version("7.86.0"), Some((7, 86, 0)));
        for invalid in ["7.86", "7.86.0.1", "v7.86.0", "7.86.0-rc.1", "7..0", "7.+1.0"] {
            assert_eq!(parse_version(invalid), None, "{invalid}");
        }
    }
}
