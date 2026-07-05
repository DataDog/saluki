//! Deserialization types for `schema_overlay.yaml`.
//!
//! This crate provides the model for management of our inventory and metadata around Datadog Agent
//! configuration. It is designed to be used during build processes by `build.rs` and, for
//! simplicity, should not depend on any other crates from our workspace.
//!
//! The overlay is validated in two passes. First, standard serde deserialization enforces its
//! type integrity, then custom validation logic runs. This file is the source of truth on what
//! can and can-not be present in the overlay.

pub mod saluki_keys;
pub mod schema_gen;
pub mod smoke_test_support;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use indexmap::{IndexMap, IndexSet};
use serde::Deserialize;

use crate::smoke_test_support::ConfigurationStruct;

/// Top-level overlay structure.
///
/// `known` covers every key the team has reviewed and classified. `ignored` covers keys irrelevant
/// to ADP's domain. Together they must account for every key in `core_schema.yaml`.
#[derive(Debug, Clone, Deserialize)]
pub struct SchemaOverlay {
    pub inventory: IndexMap<String, KnownEntry>,
    pub excluded: IndexMap<String, String>,
}

/// Classification of a known (non-ignored) config key.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "support", rename_all = "snake_case")]
pub enum KnownEntry {
    /// ADP reads and fully supports this key; behavior matches the core Agent.
    Full(FullSupport),
    /// ADP reads this key but behavior diverges from the core Agent in some cases.
    Partial(PartialSupport),
    /// ADP does not support this key.
    #[serde(rename = "none")]
    Unsupported(Unsupported),
    /// ADP's compatibility with this key has not yet been determined.
    Unknown(UnknownSupport),
}

/// Metadata for a fully supported configuration key.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FullSupport {
    /// Which pipelines depend on this key (non-empty).
    pub pipelines: PipelineAffinity,
    /// Short description for documentation tables (<= 50 chars).
    pub description: String,
    /// Extended documentation (appears in generated docs).
    #[serde(default)]
    pub documentation: Option<String>,
    /// GitHub issue tracking number.
    #[serde(default)]
    pub issue: Option<String>,
    /// When true, the generated Datadog deserializer renders this witnessed key as an absence-aware
    /// `Option<T>` instead of baking in the schema `default`. ADP then supplies the effective
    /// default from the typed model's `Default`, so an unset key falls back to ADP's chosen value
    /// while any operator-set value (including one equal to the Agent default) round-trips
    /// unchanged. Use this only when ADP intentionally diverges from the Agent's schema default for
    /// a key it fully owns.
    #[serde(default)]
    pub saluki_overrides_default: bool,
    /// Fields to support the `config_registry` and configuration smoke tests.
    pub test_support: TestSupport,
}

/// Metadata for a partially supported configuration key.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PartialSupport {
    /// Which pipelines depend on this key (non-empty).
    pub pipelines: PipelineAffinity,
    /// Short description for documentation tables (<= 50 chars).
    pub description: String,
    /// Extended documentation explaining the behavioral divergence. Required for partial keys.
    pub documentation: String,
    /// When true, the runtime classifier emits a warning for non-default values of this key.
    #[serde(default)]
    pub warn: bool,
    /// GitHub issue tracking number.
    #[serde(default)]
    pub issue: Option<String>,
    /// See [`FullSupport::saluki_overrides_default`].
    #[serde(default)]
    pub saluki_overrides_default: bool,
    /// Fields to support the `config_registry` and configuration smoke tests.
    pub test_support: TestSupport,
}

/// Metadata for an unsupported configuration key.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Unsupported {
    /// Pipelines affected by the lack of support.
    pub pipelines: PipelineAffinity,
    /// Short description for documentation tables (<= 50 chars).
    pub description: String,
    /// Longer explanation of why it is unsupported and future plans.
    #[serde(default)]
    pub documentation: Option<String>,
    /// How severe the lack of support is.
    pub severity: Severity,
    /// Whether support is planned. When true, `issue` must be present.
    pub planned: bool,
    /// GitHub issue tracking number.
    #[serde(default)]
    pub issue: Option<String>,
}

/// Metadata for a key whose support level has not yet been determined.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct UnknownSupport {
    /// Short description for documentation tables (<= 50 chars), if known.
    #[serde(default)]
    pub description: Option<String>,
    /// Severity estimate, if there is intuition about the impact.
    #[serde(default)]
    pub severity: Option<Severity>,
    /// GitHub issue tracking the investigation.
    #[serde(default)]
    pub issue: Option<String>,
}

/// Metadata to support config smoke tests.
///
/// These fields support logic in the configuration smoke tests and are tightly bound to the
/// behavior of the test logic. They may change if the test methodology changes.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TestSupport {
    /// Environment variable overrides for this key. Checked by configuration smoke tests.
    #[serde(default)]
    pub env_var_override: Option<Vec<String>>,
    /// Alias YAML paths that map to the same config key. Checked by configuration smoke tests.
    #[serde(default)]
    pub additional_yaml_paths: Vec<String>,
    /// Override the type inferred from the schema.
    #[serde(default)]
    pub value_type_override: Option<ValueType>,
    /// Config structs that consume this key (non-empty) for configuration smoke tests.
    pub used_by: IndexSet<ConfigurationStruct>,
    /// Literal JSON value for smoke test injection.
    #[serde(default)]
    pub test_json: Option<String>,
    /// TRANSITIONAL BANDAID. Carries metadata needed only to reproduce the hand-written
    /// registry (filename partitioning, Saluki-only schema source/default). Delete with it.
    #[serde(default)]
    pub additional_attributes: IndexMap<String, String>,
}

/// Impact severity of an unsupported or unknown key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Low,
    Medium,
    High,
}

/// A single pipeline in the ADP vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pipeline {
    #[serde(rename = "dogstatsd")]
    DogStatsD,
    Checks,
    Otlp,
    Traces,
}

/// Which pipelines a config key is associated with.
///
/// Deserialized from a flat YAML list of pipeline tokens. An empty list is rejected. A list
/// containing only `cross_cutting` folds to [`PipelineAffinity::CrossCutting`]; `cross_cutting`
/// may not appear alongside other tokens.
#[derive(Debug, Clone)]
pub enum PipelineAffinity {
    /// The key affects all pipelines / ADP behaviour as a whole.
    CrossCutting,
    /// The key affects the listed pipelines (non-empty, in declaration order).
    Pipelines(Vec<Pipeline>),
}

impl<'de> serde::Deserialize<'de> for PipelineAffinity {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, PartialEq)]
        #[serde(rename_all = "snake_case")]
        enum Token {
            CrossCutting,
            #[serde(rename = "dogstatsd")]
            DogStatsD,
            Checks,
            Otlp,
            Traces,
        }

        let tokens: Vec<Token> = Vec::deserialize(d)?;

        if tokens.is_empty() {
            return Err(serde::de::Error::custom("pipelines must be non-empty"));
        }

        let has_cc = tokens.iter().any(|t| t == &Token::CrossCutting);

        if has_cc && tokens.len() > 1 {
            return Err(serde::de::Error::custom(
                "cross_cutting must appear alone in pipelines list",
            ));
        }

        if has_cc {
            return Ok(PipelineAffinity::CrossCutting);
        }

        let pipelines = tokens
            .into_iter()
            .map(|t| match t {
                Token::DogStatsD => Pipeline::DogStatsD,
                Token::Checks => Pipeline::Checks,
                Token::Otlp => Pipeline::Otlp,
                Token::Traces => Pipeline::Traces,
                Token::CrossCutting => unreachable!(),
            })
            .collect();

        Ok(PipelineAffinity::Pipelines(pipelines))
    }
}

/// Override type for when the schema under-specifies a key's value type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueType {
    Boolean,
    Integer,
    Float,
    String,
    StringList,
}

/// File paths to the two YAML files required as input by this library.
///
/// Defaults to the canonical location of the required schema files in this library.
pub struct Files {
    pub schema: PathBuf,
    pub overlay: PathBuf,
}

impl Default for Files {
    fn default() -> Self {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("config")
            .join("schema");
        let schema = dir.join("core").join("core_schema.yaml");
        let overlay = dir.join("schema_overlay.yaml");
        Files { schema, overlay }
    }
}

impl SchemaOverlay {
    pub fn load(files: Files) -> Result<Self, Error> {
        let loaded = Self::from_file(&files.overlay)?;
        loaded.validate(&files.schema)?;
        Ok(loaded)
    }

    fn from_yaml(s: &str) -> Result<Self, Error> {
        let yaml: serde_yaml::Value = serde_yaml::from_str(s).map_err(Error::Yaml)?;
        Self::lint_yaml(&yaml)?;
        serde_yaml::from_value(yaml).map_err(Error::Yaml)
    }

    fn from_file(path: &Path) -> Result<Self, Error> {
        let contents = std::fs::read_to_string(path).map_err(|e| Error::Io((path.into(), e)))?;
        Self::from_yaml(&contents)
    }

    fn validate(&self, core_schema: &Path) -> Result<(), Error> {
        self.validate_keys_match(core_schema)?;
        self.validate_entries()?;
        Ok(())
    }

    /// Ensure that sections appear in the required order and that keys within each section are
    /// sorted alphabetically.
    fn lint_yaml(yaml: &serde_yaml::Value) -> Result<(), Error> {
        let mapping = yaml
            .as_mapping()
            .ok_or_else(|| Error::Validation("overlay must be a YAML mapping".to_string()))?;

        let section_names: Vec<&str> = mapping.keys().filter_map(|k| k.as_str()).collect();

        for required in ["inventory", "excluded"] {
            if !section_names.contains(&required) {
                return Err(Error::Validation(format!(
                    "overlay missing required section '{}'",
                    required
                )));
            }
        }

        let pos_known = section_names.iter().position(|&k| k == "inventory").unwrap();
        let pos_ignored = section_names.iter().position(|&k| k == "excluded").unwrap();

        if pos_known >= pos_ignored {
            return Err(Error::Validation(
                "sections must appear in order: known, ignored".to_string(),
            ));
        }

        for section_name in ["inventory", "excluded"] {
            if let Some(section) = yaml.get(section_name).and_then(|v| v.as_mapping()) {
                let mut prev = "";
                for key in section.keys().filter_map(|k| k.as_str()) {
                    if key < prev {
                        return Err(Error::Validation(format!(
                            "{}: key '{}' is out of alphabetical order (after '{}')",
                            section_name, key, prev
                        )));
                    }
                    prev = key;
                }
            }
        }

        Ok(())
    }

    /// Ensure that each core schema key appears exactly once across the overlay sections.
    fn validate_keys_match(&self, core_schema: &Path) -> Result<(), Error> {
        let schema_keys = Self::schema_keys(core_schema)?;

        for key in self.excluded.keys() {
            if self.inventory.contains_key(key.as_str()) {
                return Err(Error::Validation(format!(
                    "key '{}' appears in more than one overlay section",
                    key
                )));
            }
        }

        for key in self.inventory.keys().chain(self.excluded.keys()) {
            if !schema_keys.contains(key.as_str()) {
                return Err(Error::Validation(format!(
                    "overlay key '{}' is not present in the schema",
                    key
                )));
            }
        }

        let overlay_keys: HashSet<&str> = self
            .inventory
            .keys()
            .chain(self.excluded.keys())
            .map(|s| s.as_str())
            .collect();
        for key in &schema_keys {
            if !overlay_keys.contains(key.as_str()) {
                return Err(Error::Validation(format!(
                    "schema key '{}' is not covered by the overlay",
                    key
                )));
            }
        }

        Ok(())
    }

    fn schema_keys(schema_path: &Path) -> Result<HashSet<String>, Error> {
        let schema = load_resolved_schema(schema_path)?;
        let props = schema
            .get("properties")
            .and_then(|v| v.as_mapping())
            .ok_or_else(|| Error::Validation("schema missing 'properties' section".to_string()))?;
        let mut keys = HashSet::new();
        Self::collect_schema_keys(props, "", &mut keys);
        Ok(keys)
    }

    fn collect_schema_keys(props: &serde_yaml::Mapping, prefix: &str, keys: &mut HashSet<String>) {
        for (k, v) in props {
            if let Some(name) = k.as_str() {
                let full_key = if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{}.{}", prefix, name)
                };
                // `$ref`s have already been inlined by `load_resolved_schema`, so a node either
                // carries `properties` (recurse) or is a leaf key.
                if let Some(sub_props) = v.get("properties").and_then(|p| p.as_mapping()) {
                    Self::collect_schema_keys(sub_props, &full_key, keys);
                } else {
                    keys.insert(full_key);
                }
            }
        }
    }

    /// Validate per-entry constraints: description length, `used_by` non-empty, no duplicate
    /// `additional_yaml_paths`, and planned+issue consistency for unsupported entries.
    fn validate_entries(&self) -> Result<(), Error> {
        let canonical_keys: HashSet<&str> = self.inventory.keys().map(String::as_str).collect();

        for (key, entry) in &self.inventory {
            match entry {
                KnownEntry::Full(f) => {
                    if f.test_support.used_by.is_empty() {
                        return Err(Error::Validation(format!(
                            "full key '{}': used_by must be non-empty",
                            key
                        )));
                    }
                    if f.description.len() > 50 {
                        return Err(Error::Validation(format!(
                            "full key '{}': description exceeds 50 chars ({} chars)",
                            key,
                            f.description.len()
                        )));
                    }
                    Self::validate_additional_yaml_paths(key, &f.test_support.additional_yaml_paths, &canonical_keys)?;
                }
                KnownEntry::Partial(p) => {
                    if p.test_support.used_by.is_empty() {
                        return Err(Error::Validation(format!(
                            "partial key '{}': used_by must be non-empty",
                            key
                        )));
                    }
                    if p.description.len() > 50 {
                        return Err(Error::Validation(format!(
                            "partial key '{}': description exceeds 50 chars ({} chars)",
                            key,
                            p.description.len()
                        )));
                    }
                    Self::validate_additional_yaml_paths(key, &p.test_support.additional_yaml_paths, &canonical_keys)?;
                }
                KnownEntry::Unsupported(u) => {
                    if u.description.len() > 50 {
                        return Err(Error::Validation(format!(
                            "unsupported key '{}': description exceeds 50 chars ({} chars)",
                            key,
                            u.description.len()
                        )));
                    }
                    if u.planned && u.issue.is_none() {
                        return Err(Error::Validation(format!(
                            "unsupported key '{}': planned requires an issue",
                            key
                        )));
                    }
                }
                KnownEntry::Unknown(u) => {
                    if let Some(desc) = &u.description {
                        if desc.len() > 50 {
                            return Err(Error::Validation(format!(
                                "unknown key '{}': description exceeds 50 chars ({} chars)",
                                key,
                                desc.len()
                            )));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_additional_yaml_paths(
        key: &str, paths: &[String], canonical_keys: &HashSet<&str>,
    ) -> Result<(), Error> {
        let mut seen: HashSet<&str> = HashSet::new();
        for path in paths {
            if !seen.insert(path.as_str()) {
                return Err(Error::Validation(format!(
                    "key '{}': duplicate additional_yaml_path '{}'",
                    key, path
                )));
            }
            if path.contains('.') {
                return Err(Error::Validation(format!(
                    "key '{}': additional_yaml_path '{}' contains a dot. \
                     Dotted aliases land at a different nesting depth in the YAML tree, \
                     which cannot be represented as a serde field alias on the generated \
                     struct. Supporting dotted aliases requires a post-deserialization \
                     merge step or custom Deserialize impl.",
                    key, path
                )));
            }
            if canonical_keys.contains(path.as_str()) {
                return Err(Error::Validation(format!(
                    "key '{}': additional_yaml_path '{}' collides with a canonical \
                     overlay key. Two fields would deserialize from the same YAML key.",
                    key, path
                )));
            }
        }
        Ok(())
    }
}

/// Read a YAML file into a [`serde_yaml::Value`], mapping failures onto [`Error`].
fn read_yaml(path: &Path) -> Result<serde_yaml::Value, Error> {
    let contents = std::fs::read_to_string(path).map_err(|e| Error::Io((path.into(), e)))?;
    serde_yaml::from_str(&contents).map_err(Error::Yaml)
}

/// Load the core schema and recursively inline every `$ref: <file>` reference into a single
/// resolved document with no remaining `$ref` nodes.
///
/// Referenced files are resolved relative to the directory containing `schema_path`. This is the
/// one place that reads subsystem schema files; downstream consumers traverse the returned tree
/// and never handle `$ref` themselves. Build-time only.
///
/// # Errors
///
/// Returns [`Error::Io`] if `schema_path` or any referenced file cannot be read, and
/// [`Error::Yaml`] if any file fails to parse. The offending path is carried in the error.
pub fn load_resolved_schema(schema_path: &Path) -> Result<serde_yaml::Value, Error> {
    let schema_dir = schema_path.parent().unwrap_or_else(|| Path::new("."));
    let mut doc = read_yaml(schema_path)?;
    resolve_refs(&mut doc, schema_dir)?;
    Ok(doc)
}

/// Recursively replace any mapping node containing a `$ref: <file>` entry with the (also resolved)
/// contents of the referenced file, found relative to `schema_dir`.
fn resolve_refs(value: &mut serde_yaml::Value, schema_dir: &Path) -> Result<(), Error> {
    if let Some(map) = value.as_mapping_mut() {
        if let Some(ref_path) = map.get("$ref").and_then(|v| v.as_str()) {
            let ref_file = schema_dir.join(ref_path);
            let mut ref_doc = read_yaml(&ref_file)?;
            resolve_refs(&mut ref_doc, schema_dir)?;
            *value = ref_doc;
            return Ok(());
        }
        for (_k, v) in map.iter_mut() {
            resolve_refs(v, schema_dir)?;
        }
    }
    Ok(())
}

const VALIDATION_RULES: &str = "\n\
    \n\
    Rules that must hold in schema_overlay.yaml:\n\
    - Every core_schema.yaml key appears in exactly one section (known / ignored).\n\
    - No key appears in more than one section.\n\
    - Sections appear in order: known, ignored.\n\
    - Keys within each section are sorted alphabetically.\n\
    - full entries: pipelines non-empty, used_by non-empty, description <= 50 chars.\n\
    - partial entries: pipelines non-empty, used_by non-empty, description <= 50 chars, documentation required.\n\
    - unsupported entries: pipelines non-empty, description <= 50 chars, planned+issue consistent.\n\
    - unknown entries: description <= 50 chars (when present).\n\
    - additional_yaml_paths: no duplicates within a single entry, no dots, no collisions with canonical keys.\n\
    Fix: edit lib/datadog-agent/config/schema/schema_overlay.yaml.";

/// Errors that can occur when loading a schema overlay.
#[derive(Debug)]
pub enum Error {
    Io((PathBuf, std::io::Error)),
    Yaml(serde_yaml::Error),
    Validation(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "Error reading {}: {}", e.0.display(), e.1),
            Error::Yaml(e) => write!(f, "YAML parse error in overlay: {e}"),
            Error::Validation(s) => write!(f, "schema_overlay.yaml validation failed: {s}{VALIDATION_RULES}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(&e.1),
            Error::Yaml(e) => Some(e),
            Error::Validation(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_loads() {
        let test_files = Files {
            schema: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("test")
                .join("fake_schema.yaml"),
            overlay: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("test")
                .join("fake_overlay.yaml"),
        };
        let validated = SchemaOverlay::load(test_files).unwrap();
        assert_eq!(validated.inventory.len(), 18);
    }

    #[test]
    fn pipeline_affinity_cross_cutting() {
        let yaml = "pipelines: [cross_cutting]";
        #[derive(Deserialize)]
        struct W {
            pipelines: PipelineAffinity,
        }
        let w: W = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(w.pipelines, PipelineAffinity::CrossCutting));
    }

    #[test]
    fn pipeline_affinity_multi() {
        let yaml = "pipelines: [dogstatsd, traces]";
        #[derive(Deserialize)]
        struct W {
            pipelines: PipelineAffinity,
        }
        let w: W = serde_yaml::from_str(yaml).unwrap();
        if let PipelineAffinity::Pipelines(ps) = w.pipelines {
            assert_eq!(ps.len(), 2);
            assert!(matches!(ps[0], Pipeline::DogStatsD));
            assert!(matches!(ps[1], Pipeline::Traces));
        } else {
            panic!("expected Pipelines");
        }
    }

    #[test]
    fn pipeline_affinity_cross_cutting_must_be_alone() {
        let yaml = "pipelines: [cross_cutting, dogstatsd]";
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct W {
            pipelines: PipelineAffinity,
        }
        assert!(serde_yaml::from_str::<W>(yaml).is_err());
    }

    fn load_from_strs(schema: &str, overlay: &str) -> Result<SchemaOverlay, Error> {
        let dir = tempfile::tempdir().unwrap();
        let schema_path = dir.path().join("schema.yaml");
        let overlay_path = dir.path().join("overlay.yaml");
        std::fs::write(&schema_path, schema).unwrap();
        std::fs::write(&overlay_path, overlay).unwrap();
        SchemaOverlay::load(Files {
            schema: schema_path,
            overlay: overlay_path,
        })
    }

    #[test]
    fn validation_rejects_schema_key_missing_from_overlay() {
        let schema = "\
properties:
  key_a:
    type: string
  key_b:
    type: string
";
        let overlay = "\
inventory:
  key_a:
    support: full
    pipelines: [cross_cutting]
    description: \"Key A\"
    test_support:
      used_by: [ForwarderConfiguration]
excluded: {}
";
        let err = load_from_strs(schema, overlay).unwrap_err();
        assert!(
            err.to_string().contains("schema key 'key_b' is not covered"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validation_rejects_overlay_key_absent_from_schema() {
        let schema = "\
properties:
  key_a:
    type: string
";
        let overlay = "\
inventory:
  key_a:
    support: full
    pipelines: [cross_cutting]
    description: \"Key A\"
    test_support:
      used_by: [ForwarderConfiguration]
excluded:
  key_b: \"not in schema\"
";
        let err = load_from_strs(schema, overlay).unwrap_err();
        assert!(
            err.to_string().contains("overlay key 'key_b' is not present"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validation_rejects_key_in_two_sections() {
        let schema = "\
properties:
  key_a:
    type: string
  key_b:
    type: string
";
        let overlay = "\
inventory:
  key_a:
    support: full
    pipelines: [cross_cutting]
    description: \"Key A\"
    test_support:
      used_by: [ForwarderConfiguration]
excluded:
  key_a: \"duplicate\"
  key_b: \"ok\"
";
        let err = load_from_strs(schema, overlay).unwrap_err();
        assert!(
            err.to_string()
                .contains("key 'key_a' appears in more than one overlay section"),
            "unexpected error: {err}"
        );
    }

    /// A `$ref` is inlined and its leaf keys are namespaced under the parent key.
    #[test]
    fn schema_ref_is_resolved_and_keys_namespaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("sub.yaml"),
            "properties:\n  enabled:\n    type: boolean\n",
        )
        .unwrap();
        let schema_path = dir.path().join("schema.yaml");
        std::fs::write(&schema_path, "properties:\n  feature:\n    $ref: sub.yaml\n").unwrap();

        let keys = SchemaOverlay::schema_keys(&schema_path).unwrap();
        assert_eq!(
            keys,
            HashSet::from(["feature.enabled".to_string()]),
            "unexpected keys: {keys:?}"
        );
    }

    /// A missing `$ref` target surfaces a clear I/O error naming the file, not a misleading
    /// "key not covered" validation error.
    #[test]
    fn missing_schema_ref_reports_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let schema_path = dir.path().join("schema.yaml");
        std::fs::write(&schema_path, "properties:\n  feature:\n    $ref: does_not_exist.yaml\n").unwrap();

        let err = SchemaOverlay::schema_keys(&schema_path).unwrap_err();
        assert!(matches!(err, Error::Io(_)), "expected Io error, got: {err}");
        assert!(
            err.to_string().contains("does_not_exist.yaml"),
            "error should name the missing file: {err}"
        );
    }

    #[test]
    fn validation_rejects_unsorted_inventory_keys() {
        let schema = "\
properties:
  key_a:
    type: string
  key_b:
    type: string
";
        let overlay = "\
inventory:
  key_b:
    support: full
    pipelines: [cross_cutting]
    description: \"Key B\"
    test_support:
      used_by: [ForwarderConfiguration]
  key_a:
    support: full
    pipelines: [cross_cutting]
    description: \"Key A\"
    test_support:
      used_by: [ForwarderConfiguration]
excluded: {}
";
        let err = load_from_strs(schema, overlay).unwrap_err();
        assert!(
            err.to_string().contains("out of alphabetical order"),
            "unexpected error: {err}"
        );
    }
}
