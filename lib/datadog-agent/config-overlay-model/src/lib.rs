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

/// Top-level overlay structure keyed by YAML path.
///
/// Sections partition every `core_schema.yaml` key into exactly one of: supported, unsupported,
/// investigate, or ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SchemaOverlay {
    /// Keys that ADP actively reads and uses.
    pub supported: IndexMap<String, Supported>,

    /// Keys that are relevant to ADP's domain, but which are not supported.
    pub unsupported: IndexMap<String, Unsupported>,

    /// Keys that should not be in the ignored list but whose classification is not yet determined.
    /// Investigation may lead to supported, unsupported, or back to ignored.
    #[serde(default)]
    pub investigate: IndexMap<String, Investigate>,

    /// Keys that are irrelevant to ADP's domain. Value is a short reason string. These must not be
    /// mechanically added. A human should at least review the key and reason string when it is
    /// added to the list of ignored keys.
    pub ignored: IndexMap<String, String>,
}

/// Metadata for a supported configuration key.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Supported {
    /// Whether the key is fully or partially supported.
    pub support_level: SupportLevel,

    /// Which pipelines depend on this key (non-empty).
    pub pipelines: PipelineAffinity,

    /// Override the type inferred from the schema.
    #[serde(default)]
    pub value_type_override: Option<ValueType>,

    // Documentation support
    /// Short description for documentation tables (<= 50 chars).
    pub description: String,

    /// Extended documentation (appears in generated docs).
    #[serde(default)]
    pub documentation: Option<String>,

    /// GitHub issue tracking number.
    #[serde(default)]
    pub issue: Option<String>,

    // Direct deserialization support
    //
    // These fields support ADP's implementation of the Agent's overlay and deserialization
    // mechanism. It is hoped that these may be removed once ADP is fully abstracted from Agent
    // configuration logic.
    /// Environment variable overrides for this key. Checked by configuration smoke tests.
    #[serde(default)]
    pub env_var_override: Option<Vec<String>>,

    /// Alias YAML paths that map to the same config key. Checked by configuration smoke tests.
    #[serde(default)]
    pub additional_yaml_paths: Vec<String>,

    // Test support
    //
    // These fields support logic in the configuration smoke tests. It is hoped that these may be
    // removed once ADP is fully abstracted from Agent configuration deserialization which would
    // render the smoke tests inert.
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

/// Metadata for an unsupported configuration key.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Unsupported {
    /// How severe the lack of support is.
    pub severity: Severity,

    /// Whether support is planned. When true, `issue` must be present.
    pub planned: bool,

    /// Pipelines affected by the lack of support.
    pub pipelines: PipelineAffinity,

    /// Short description for documentation tables.
    pub description: String,

    /// Longer explanation of why it is unsupported and future plans.
    #[serde(default)]
    pub documentation: Option<String>,

    /// GitHub issue tracking number.
    #[serde(default)]
    pub issue: Option<String>,
}

/// A configuration key whose classification has not yet been determined.
///
/// Keys in this section are known to need investigation but have not been assessed for support
/// level or severity. They are excluded from the runtime classifier and appear only in generated
/// documentation. Once investigated, a key moves to supported, unsupported, or ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Investigate {
    /// Provisional severity carried over from prior classification. When present and the key
    /// ultimately lands in unsupported, this value is a starting point. Has no runtime effect.
    #[serde(default)]
    pub severity: Option<Severity>,

    /// Short description for documentation tables (<= 50 chars).
    pub description: String,

    /// GitHub issue tracking the investigation.
    #[serde(default)]
    pub issue: Option<String>,
}

/// Whether a key is fully or partially supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupportLevel {
    Full,
    Partial,
}

/// Impact severity of an unsupported key.
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
        let schema = dir.join("core_schema.yaml");
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

    /// Deserialize a [`SchemaOverlay`] from a YAML string.
    fn from_yaml(s: &str) -> Result<Self, Error> {
        let yaml: serde_yaml::Value = serde_yaml::from_str(s).map_err(Error::Yaml)?;
        Self::lint_yaml(&yaml)?;
        serde_yaml::from_value(yaml).map_err(Error::Yaml)
    }

    /// Load and deserialize a `SchemaOverlay` from a file path.
    fn from_file(path: &Path) -> Result<Self, Error> {
        let contents = std::fs::read_to_string(path).map_err(|e| Error::Io((path.into(), e)))?;
        Self::from_yaml(&contents)
    }

    /// Check that the loaded schema overlay is consistent with our required invariants.
    fn validate(&self, core_schema: &Path) -> Result<(), Error> {
        self.validate_keys_match(core_schema)?;
        self.validate_used_by()?;
        Ok(())
    }

    /// Ensure our diffs are manageable by requiring a stable ordering of the overlay file.
    ///
    /// Checks that sections appear in the required order (supported, unsupported, investigate,
    /// ignored) and that keys within each section are in alphabetical order. The investigate
    /// section is optional.
    fn lint_yaml(yaml: &serde_yaml::Value) -> Result<(), Error> {
        let mapping = yaml
            .as_mapping()
            .ok_or_else(|| Error::Validation("overlay must be a YAML mapping".to_string()))?;

        let section_names: Vec<&str> = mapping.keys().filter_map(|k| k.as_str()).collect();

        let required = ["supported", "unsupported", "ignored"];
        let positions: Vec<Option<usize>> = required
            .iter()
            .map(|&s| section_names.iter().position(|&k| k == s))
            .collect();
        for (i, pos) in positions.iter().enumerate() {
            if pos.is_none() {
                return Err(Error::Validation(format!(
                    "overlay missing required section '{}'",
                    required[i]
                )));
            }
        }
        let (pos_sup, pos_uns, pos_ign) = (positions[0].unwrap(), positions[1].unwrap(), positions[2].unwrap());

        let pos_inv = section_names.iter().position(|&k| k == "investigate");

        if let Some(pos_inv) = pos_inv {
            if !(pos_sup < pos_uns && pos_uns < pos_inv && pos_inv < pos_ign) {
                return Err(Error::Validation(
                    "sections must appear in order: supported, unsupported, investigate, ignored".to_string(),
                ));
            }
        } else if !(pos_sup < pos_uns && pos_uns < pos_ign) {
            return Err(Error::Validation(
                "sections must appear in order: supported, unsupported, [investigate,] ignored".to_string(),
            ));
        }

        let mut lint_sections: Vec<&str> = required.to_vec();
        if pos_inv.is_some() {
            lint_sections.push("investigate");
        }
        for section_name in &lint_sections {
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

    /// Ensure that each core schema key appears exactly once in the schema overlay file.
    fn validate_keys_match(&self, core_schema: &Path) -> Result<(), Error> {
        let schema_keys = Self::schema_keys(core_schema)?;

        type SectionCheck<'a> = (&'a str, &'a dyn Fn(&str) -> bool);
        // No key may appear in more than one section.
        let sections: &[SectionCheck<'_>] = &[
            ("supported", &|k: &str| self.supported.contains_key(k)),
            ("unsupported", &|k: &str| self.unsupported.contains_key(k)),
            ("investigate", &|k: &str| self.investigate.contains_key(k)),
            ("ignored", &|k: &str| self.ignored.contains_key(k)),
        ];
        let all_keys: Vec<(&str, &str)> = self
            .supported
            .keys()
            .map(|k| ("supported", k.as_str()))
            .chain(self.unsupported.keys().map(|k| ("unsupported", k.as_str())))
            .chain(self.investigate.keys().map(|k| ("investigate", k.as_str())))
            .chain(self.ignored.keys().map(|k| ("ignored", k.as_str())))
            .collect();
        for &(from_section, key) in &all_keys {
            for &(section_name, check_fn) in sections {
                if section_name != from_section && check_fn(key) {
                    return Err(Error::Validation(format!(
                        "key '{}' appears in more than one overlay section",
                        key
                    )));
                }
            }
        }

        // Every overlay key must exist in the schema.
        for key in self
            .supported
            .keys()
            .chain(self.unsupported.keys())
            .chain(self.investigate.keys())
            .chain(self.ignored.keys())
        {
            if !schema_keys.contains(key.as_str()) {
                return Err(Error::Validation(format!(
                    "overlay key '{}' is not present in the schema",
                    key
                )));
            }
        }

        // Every schema key must be covered by the overlay.
        let overlay_keys: HashSet<&str> = self
            .supported
            .keys()
            .chain(self.unsupported.keys())
            .chain(self.investigate.keys())
            .chain(self.ignored.keys())
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
        let contents = std::fs::read_to_string(schema_path).map_err(|e| Error::Io((schema_path.into(), e)))?;
        let schema: serde_yaml::Value = serde_yaml::from_str(&contents).map_err(Error::Yaml)?;
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
                if let Some(sub_props) = v.get("properties").and_then(|p| p.as_mapping()) {
                    Self::collect_schema_keys(sub_props, &full_key, keys);
                } else {
                    keys.insert(full_key);
                }
            }
        }
    }

    /// Ensure that every supported field declares at least one `used_by` struct, that descriptions
    /// fit within the 50-character table limit, and that `additional_yaml_paths` contains no
    /// duplicates. Pipeline non-emptiness is enforced by `PipelineAffinity`'s custom Deserialize.
    fn validate_used_by(&self) -> Result<(), Error> {
        for (key, entry) in &self.supported {
            if entry.used_by.is_empty() {
                return Err(Error::Validation(format!(
                    "supported key '{}': used_by must be non-empty",
                    key
                )));
            }
            if entry.description.len() > 50 {
                return Err(Error::Validation(format!(
                    "supported key '{}': description exceeds 50 chars ({} chars)",
                    key,
                    entry.description.len()
                )));
            }
            let mut seen: HashSet<&str> = HashSet::new();
            for path in &entry.additional_yaml_paths {
                if !seen.insert(path.as_str()) {
                    return Err(Error::Validation(format!(
                        "supported key '{}': duplicate additional_yaml_path '{}'",
                        key, path
                    )));
                }
            }
        }
        for (key, entry) in &self.unsupported {
            if entry.description.len() > 50 {
                return Err(Error::Validation(format!(
                    "unsupported key '{}': description exceeds 50 chars ({} chars)",
                    key,
                    entry.description.len()
                )));
            }
            if entry.planned && entry.issue.is_none() {
                return Err(Error::Validation(format!(
                    "unsupported key '{}': planned requires an issue",
                    key
                )));
            }
        }
        for (key, entry) in &self.investigate {
            if entry.description.len() > 50 {
                return Err(Error::Validation(format!(
                    "investigate key '{}': description exceeds 50 chars ({} chars)",
                    key,
                    entry.description.len()
                )));
            }
        }
        Ok(())
    }
}

/// Appended to every `Error::Validation` message so the reader knows what rules apply and where
/// to make the fix.
const VALIDATION_RULES: &str = "\n\
    \n\
    Rules that must hold in schema_overlay.yaml:\n\
    - Every core_schema.yaml key appears in exactly one section (supported / unsupported / investigate / ignored).\n\
    - No key appears in more than one section.\n\
    - Sections appear in order: supported, unsupported, investigate (optional), ignored.\n\
    - Keys within each section are sorted alphabetically.\n\
    - supported entries: pipelines non-empty, used_by non-empty, description <= 50 chars.\n\
    - unsupported entries: pipelines non-empty, description <= 50 chars, planned+issue consistent.\n\
    - investigate entries: description <= 50 chars.\n\
    - additional_yaml_paths: no duplicates within a single entry.\n\
    Fix: edit lib/datadog-agent-config/schema/schema_overlay.yaml.";

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
        assert_eq!(validated.supported.len(), 14);
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
supported:
  key_a:
    support_level: full
    used_by: [ForwarderConfiguration]
    pipelines: [cross_cutting]
    description: \"Key A\"
unsupported: {}
ignored: {}
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
supported:
  key_a:
    support_level: full
    used_by: [ForwarderConfiguration]
    pipelines: [cross_cutting]
    description: \"Key A\"
unsupported: {}
ignored:
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
supported:
  key_a:
    support_level: full
    used_by: [ForwarderConfiguration]
    pipelines: [cross_cutting]
    description: \"Key A\"
unsupported: {}
ignored:
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
}
