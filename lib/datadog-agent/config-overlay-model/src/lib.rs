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
    /// Configuration consumers that incorporate this key (non-empty).
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
    /// The Datadog Agent core schema file (`schema/core/core_schema.yaml`).
    pub datadog_schema: PathBuf,
    /// Directory containing the vendored OTel receiver schema (`schema/otel/`).
    pub otel_schema_dir: PathBuf,
    /// The schema overlay (`schema/schema_overlay.yaml`).
    pub overlay: PathBuf,
}

impl Default for Files {
    fn default() -> Self {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("config")
            .join("schema");
        Files {
            datadog_schema: dir.join("core").join("core_schema.yaml"),
            otel_schema_dir: dir.join("otel"),
            overlay: dir.join("schema_overlay.yaml"),
        }
    }
}

impl SchemaOverlay {
    pub fn load(files: Files) -> Result<Self, Error> {
        let loaded = Self::from_file(&files.overlay)?;
        loaded.validate(&files.datadog_schema, &files.otel_schema_dir)?;
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

    fn validate(&self, datadog_schema: &Path, otel_schema_dir: &Path) -> Result<(), Error> {
        self.validate_keys_match(datadog_schema, otel_schema_dir)?;
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
    fn validate_keys_match(&self, datadog_schema: &Path, otel_schema_dir: &Path) -> Result<(), Error> {
        let schema_keys = Self::schema_keys(datadog_schema, otel_schema_dir)?;

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

    fn schema_keys(datadog_schema: &Path, otel_schema_dir: &Path) -> Result<HashSet<String>, Error> {
        let schema = load_composed_schema(datadog_schema, otel_schema_dir)?;
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

// ─── OTel Schema Resolution ─────────────────────────────────────────────────
//
// The OTel Collector schemas use a different JSON Schema dialect than the Datadog schema:
// - `$ref: <name>` resolves against the current file's `$defs` (local reference)
// - `$ref: /config/<package>.<def_name>` loads another package's `config.schema.yaml`
//   and resolves against its `$defs` (package-qualified reference)
// - `allOf` merges multiple fragments into a single object
//
// These functions load the vendored OTel schemas from `schema/otel/`, resolve all references
// into a flat tree, strip excluded properties (`auth`, `middlewares`), convert to the Datadog
// schema dialect (`node_type: section` / `node_type: setting`), and patch the result into the
// Datadog schema's `otlp_config.receiver` subtree.

/// Load the Datadog schema with the OTel receiver schema patched in.
///
/// This loads the pristine Datadog schema (`core_schema.yaml`), loads and resolves the pristine
/// OTel receiver schema (`otel/config.schema.yaml`), and replaces the `otlp_config.receiver`
/// subtree in the Datadog schema with the resolved OTel subtree.
pub fn load_composed_schema(datadog_schema: &Path, otel_schema_dir: &Path) -> Result<serde_yaml::Value, Error> {
    let mut datadog_schema = load_resolved_schema(datadog_schema)?;
    let otel_receiver = load_otel_receiver(otel_schema_dir)?;
    patch_receiver(&mut datadog_schema, otel_receiver)?;
    Ok(datadog_schema)
}

/// Load and fully resolve the OTel receiver schema into a Datadog-dialect subtree.
fn load_otel_receiver(otel_dir: &Path) -> Result<serde_yaml::Value, Error> {
    let schema_path = otel_dir.join("config.schema.yaml");
    let doc = read_yaml(&schema_path)?;

    let defs = doc
        .get("$defs")
        .and_then(|v| v.as_mapping())
        .ok_or_else(|| Error::Validation("OTel schema missing $defs".to_string()))?
        .clone();

    // Start from properties.protocols (the root of the receiver config).
    let mut protocols = doc
        .get("properties")
        .and_then(|v| v.get("protocols"))
        .cloned()
        .ok_or_else(|| Error::Validation("OTel schema missing properties.protocols".to_string()))?;

    // Resolve all $ref, $defs, and allOf.
    resolve_otel_refs(&mut protocols, otel_dir, &defs)?;

    // Convert to Datadog schema dialect.
    convert_to_datadog_dialect(&mut protocols);

    // Wrap in a "receiver" section to match the Datadog schema structure.
    let mut receiver_section = serde_yaml::Mapping::new();
    receiver_section.insert(
        serde_yaml::Value::String("node_type".to_string()),
        serde_yaml::Value::String("section".to_string()),
    );
    receiver_section.insert(
        serde_yaml::Value::String("type".to_string()),
        serde_yaml::Value::String("object".to_string()),
    );
    let mut properties = serde_yaml::Mapping::new();
    properties.insert(serde_yaml::Value::String("protocols".to_string()), protocols);
    receiver_section.insert(
        serde_yaml::Value::String("properties".to_string()),
        serde_yaml::Value::Mapping(properties),
    );

    Ok(serde_yaml::Value::Mapping(receiver_section))
}

/// Replace the `otlp_config.receiver` subtree in the Datadog schema with the resolved OTel subtree.
///
/// If the Datadog schema does not have an `otlp_config.receiver` subtree (for example test schemas),
/// this is a no-op.
fn patch_receiver(datadog_schema: &mut serde_yaml::Value, otel_receiver: serde_yaml::Value) -> Result<(), Error> {
    let Some(otlp_config) = datadog_schema
        .get_mut("properties")
        .and_then(|v| v.get_mut("otlp_config"))
    else {
        return Ok(());
    };

    let Some(receiver) = otlp_config.get_mut("properties").and_then(|v| v.get_mut("receiver")) else {
        return Ok(());
    };

    *receiver = otel_receiver;
    Ok(())
}

/// Resolve OTel schema references into a flat tree.
///
/// Handles three OTel schema features:
/// - `$ref: <name>`: local lookup in the current file's `$defs`
/// - `$ref: /config/<package>.<def_name>`: load another package's schema file
/// - `allOf`: merge fragments into the parent
///
/// Also strips `auth` and `middlewares` properties (excluded from the receiver keyspace).
fn resolve_otel_refs(
    value: &mut serde_yaml::Value, otel_dir: &Path, current_defs: &serde_yaml::Mapping,
) -> Result<(), Error> {
    let Some(map) = value.as_mapping_mut() else {
        return Ok(());
    };

    // 1. Handle $ref: replace this node with the resolved definition.
    if let Some(ref_str) = map.get("$ref").and_then(|v| v.as_str()) {
        // Save sibling keys (everything except $ref): for example x-optional, description.
        let siblings: Vec<(serde_yaml::Value, serde_yaml::Value)> = map
            .iter()
            .filter(|(k, _)| k.as_str() != Some("$ref"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // Resolve the ref target, getting the definition value and its source $defs.
        let (mut resolved, source_defs) = resolve_otel_ref_target(ref_str, otel_dir, current_defs)?;

        // Recursively resolve refs inside the definition using the source file's $defs.
        resolve_otel_refs(&mut resolved, otel_dir, &source_defs)?;

        // Resolve siblings using current_defs (they belong to the current file).
        let mut resolved_siblings: Vec<(serde_yaml::Value, serde_yaml::Value)> = Vec::new();
        for (k, mut v) in siblings {
            resolve_otel_refs(&mut v, otel_dir, current_defs)?;
            resolved_siblings.push((k, v));
        }

        // Merge siblings into the resolved definition (siblings override def keys).
        if let Some(resolved_map) = resolved.as_mapping_mut() {
            for (k, v) in resolved_siblings {
                resolved_map.insert(k, v);
            }
        }

        *value = resolved;
        return Ok(());
    }

    // 2. Handle allOf: merge each fragment's properties into this node.
    if let Some(allof) = map.remove("allOf") {
        if let Some(allof_seq) = allof.as_sequence() {
            for fragment in allof_seq {
                let mut resolved_fragment = fragment.clone();
                resolve_otel_refs(&mut resolved_fragment, otel_dir, current_defs)?;

                // Merge fragment's properties into value's properties.
                if let Some(source_props) = resolved_fragment.get("properties").and_then(|v| v.as_mapping()) {
                    if map.get("properties").is_none() {
                        map.insert(
                            serde_yaml::Value::String("properties".to_string()),
                            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
                        );
                    }
                    if let Some(target_props) = map.get_mut("properties").and_then(|v| v.as_mapping_mut()) {
                        for (k, v) in source_props {
                            target_props.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
        }
    }

    // 3. Recurse into properties, stripping auth and middlewares that reference excluded packages.
    if let Some(props) = map.get_mut("properties").and_then(|v| v.as_mapping_mut()) {
        // Strip auth/middlewares if they have a $ref (direct or via allOf/items) that
        // transitively references configauth/configmiddleware. A plain `auth` field
        // (like tpm_config.auth: type: string) has no $ref and must be kept.
        if let Some(auth) = props.get("auth") {
            let auth_str = serde_yaml::to_string(auth).unwrap_or_default();
            if auth_str.contains("$ref") {
                props.remove(serde_yaml::Value::String("auth".to_string()));
            }
        }
        if let Some(middlewares) = props.get("middlewares") {
            let mw_str = serde_yaml::to_string(middlewares).unwrap_or_default();
            if mw_str.contains("$ref") {
                props.remove(serde_yaml::Value::String("middlewares".to_string()));
            }
        }
        for (_, v) in props.iter_mut() {
            resolve_otel_refs(v, otel_dir, current_defs)?;
        }
    }

    Ok(())
}

/// Resolve a single `$ref` target, returning the definition value and its source `$defs`.
///
/// For local refs (`$ref: protocols`), looks up `current_defs`.
/// For package-qualified refs (`$ref: /config/`configgrpc`.server_config`), loads the
/// corresponding vendored schema file and looks up its `$defs`.
/// For `configopaque` refs (not vendored), returns an inline type definition.
fn resolve_otel_ref_target(
    ref_str: &str, otel_dir: &Path, current_defs: &serde_yaml::Mapping,
) -> Result<(serde_yaml::Value, serde_yaml::Mapping), Error> {
    if let Some(stripped) = ref_str.strip_prefix('/') {
        // Package-qualified ref: /config/`configgrpc`.server_config
        let last_dot = stripped
            .rfind('.')
            .ok_or_else(|| Error::Validation(format!("invalid package-qualified ref (no dot): {ref_str}")))?;
        let package_path = &stripped[..last_dot];
        let def_name = &stripped[last_dot + 1..];

        // Handle configopaque refs inline (not vendored).
        if package_path == "config/configopaque" {
            let inline = match def_name {
                "string" => serde_yaml::Value::Mapping([("type".into(), "string".into())].into_iter().collect()),
                "map_list" => {
                    let mut m = serde_yaml::Mapping::new();
                    m.insert("type".into(), "object".into());
                    let mut addl = serde_yaml::Mapping::new();
                    addl.insert("type".into(), "string".into());
                    m.insert("additionalProperties".into(), serde_yaml::Value::Mapping(addl));
                    serde_yaml::Value::Mapping(m)
                }
                other => return Err(Error::Validation(format!("unknown configopaque ref: {other}"))),
            };
            return Ok((inline, serde_yaml::Mapping::new()));
        }

        // Reject refs to excluded packages (should not be reached if auth/middlewares are stripped).
        if package_path == "config/configauth" || package_path == "config/configmiddleware" {
            return Err(Error::Validation(format!(
                "unexpected ref to excluded package: {ref_str}"
            )));
        }

        // Load the package's schema file.
        let file_path = otel_dir.join(package_path).join("config.schema.yaml");
        let file_doc = read_yaml(&file_path)?;
        let file_defs = file_doc
            .get("$defs")
            .and_then(|v| v.as_mapping())
            .ok_or_else(|| Error::Validation(format!("OTel schema {file_path:?} missing $defs")))?
            .clone();
        let def_value = file_defs
            .get(def_name)
            .cloned()
            .ok_or_else(|| Error::Validation(format!("$defs.{def_name} not found in {file_path:?}")))?;

        Ok((def_value, file_defs))
    } else {
        // Local ref: protocols, http_config, sanitized_url_path, etc.
        let def_value = current_defs
            .get(ref_str)
            .cloned()
            .ok_or_else(|| Error::Validation(format!("local $def {ref_str} not found")))?;
        Ok((def_value, current_defs.clone()))
    }
}

/// Convert an OTel schema tree to the Datadog schema dialect.
///
/// Adds `node_type: section` to objects with `properties` and `node_type: setting` to leaves.
/// Removes OTel-specific extensions (`x-optional`, `x-customType`).
fn convert_to_datadog_dialect(value: &mut serde_yaml::Value) {
    let Some(map) = value.as_mapping_mut() else {
        return;
    };

    // Remove OTel-specific extensions.
    map.remove(serde_yaml::Value::String("x-optional".to_string()));
    map.remove(serde_yaml::Value::String("x-customType".to_string()));

    // Determine if this is a section (has properties) or a setting (leaf).
    let is_section = map.get("properties").is_some();
    let node_type = if is_section { "section" } else { "setting" };

    // Add node_type if not present.
    if map.get("node_type").is_none() {
        map.insert(
            serde_yaml::Value::String("node_type".to_string()),
            serde_yaml::Value::String(node_type.to_string()),
        );
    }

    // Recurse into properties.
    if let Some(props) = map.get_mut("properties").and_then(|v| v.as_mapping_mut()) {
        for (_, v) in props.iter_mut() {
            convert_to_datadog_dialect(v);
        }
    }
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

    /// Returns the OTel schema directory for tests that need the composed schema.
    fn otel_schema_dir_for_tests() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("config")
            .join("schema")
            .join("otel")
    }

    #[test]
    fn overlay_loads() {
        let test_files = Files {
            datadog_schema: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("test")
                .join("fake_schema.yaml"),
            otel_schema_dir: otel_schema_dir_for_tests(),
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
        let schema_path = dir.path().join("fake_schema.yaml");
        let overlay_path = dir.path().join("overlay.yaml");
        std::fs::write(&schema_path, schema).unwrap();
        std::fs::write(&overlay_path, overlay).unwrap();
        let otel_schema_dir = otel_schema_dir_for_tests();
        SchemaOverlay::load(Files {
            datadog_schema: schema_path,
            otel_schema_dir,
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
      used_by: [TypedConfigSystem]
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
      used_by: [TypedConfigSystem]
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
      used_by: [TypedConfigSystem]
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
        let schema_path = dir.path().join("core_schema.yaml");
        std::fs::write(&schema_path, "properties:\n  feature:\n    $ref: sub.yaml\n").unwrap();

        let keys = SchemaOverlay::schema_keys(&schema_path, &otel_schema_dir_for_tests()).unwrap();
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
        let schema_path = dir.path().join("core_schema.yaml");
        std::fs::write(&schema_path, "properties:\n  feature:\n    $ref: does_not_exist.yaml\n").unwrap();

        let err = SchemaOverlay::schema_keys(&schema_path, &otel_schema_dir_for_tests()).unwrap_err();
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
      used_by: [TypedConfigSystem]
  key_a:
    support: full
    pipelines: [cross_cutting]
    description: \"Key A\"
    test_support:
      used_by: [TypedConfigSystem]
excluded: {}
";
        let err = load_from_strs(schema, overlay).unwrap_err();
        assert!(
            err.to_string().contains("out of alphabetical order"),
            "unexpected error: {err}"
        );
    }

    // The per-entry validation rules (`validate_entries`) are documented in `VALIDATION_RULES` and
    // enforced independently of the schema cross-check, so these tests deserialize an overlay in
    // isolation (via `from_yaml`) and run only that pass: no matching core schema is needed.
    fn validate_entries_of(overlay: &str) -> Result<(), Error> {
        SchemaOverlay::from_yaml(overlay)
            .expect("overlay should deserialize")
            .validate_entries()
    }

    #[test]
    fn per_entry_validation_accepts_a_well_formed_entry_of_every_kind() {
        // Keys are alphabetically ordered (full < partial < unknown < unsupported) so the YAML lint
        // pass is satisfied and only per-entry validation is under test.
        let overlay = "\
inventory:
  full_key:
    support: full
    pipelines: [dogstatsd]
    description: \"Fully supported key\"
    test_support:
      used_by: [TypedConfigSystem]
      additional_yaml_paths: [full_alias]
  partial_key:
    support: partial
    pipelines: [traces]
    description: \"Partially supported key\"
    documentation: \"Behaves differently from the core agent.\"
    test_support:
      used_by: [TypedConfigSystem]
  unknown_key:
    support: unknown
    description: \"Not yet classified\"
  unsupported_key:
    support: none
    pipelines: [checks]
    description: \"Unsupported key\"
    severity: high
    planned: true
    issue: \"1234\"
excluded: {}
";
        validate_entries_of(overlay).expect("a well-formed overlay should pass per-entry validation");
    }

    #[test]
    fn per_entry_validation_rejects_over_long_description_for_every_entry_kind() {
        // A 60-character description exceeds the documented 50-char cap. Each entry kind that carries a
        // description enforces the same limit, so this walks all four.
        let long = "x".repeat(60);
        let cases: &[(&str, String)] = &[
            (
                "full",
                format!(
                    "inventory:\n  key_a:\n    support: full\n    pipelines: [dogstatsd]\n    \
                     description: \"{long}\"\n    test_support:\n      used_by: [TypedConfigSystem]\nexcluded: {{}}\n"
                ),
            ),
            (
                "partial",
                format!(
                    "inventory:\n  key_a:\n    support: partial\n    pipelines: [dogstatsd]\n    \
                     description: \"{long}\"\n    documentation: \"diverges\"\n    test_support:\n      \
                     used_by: [TypedConfigSystem]\nexcluded: {{}}\n"
                ),
            ),
            (
                "unsupported",
                format!(
                    "inventory:\n  key_a:\n    support: none\n    pipelines: [dogstatsd]\n    \
                     description: \"{long}\"\n    severity: low\n    planned: false\nexcluded: {{}}\n"
                ),
            ),
            (
                "unknown",
                format!("inventory:\n  key_a:\n    support: unknown\n    description: \"{long}\"\nexcluded: {{}}\n"),
            ),
        ];

        for (kind, overlay) in cases {
            let err = validate_entries_of(overlay).expect_err(&format!("{kind} entry should be rejected"));
            assert!(
                err.to_string().contains("description exceeds 50 chars"),
                "{kind}: unexpected error: {err}"
            );
        }
    }

    #[test]
    fn per_entry_validation_rejects_empty_used_by_for_full_and_partial_entries() {
        let full = "\
inventory:
  key_a:
    support: full
    pipelines: [dogstatsd]
    description: \"Key A\"
    test_support:
      used_by: []
excluded: {}
";
        let err = validate_entries_of(full).expect_err("full entry with empty used_by should be rejected");
        assert!(
            err.to_string().contains("full key 'key_a': used_by must be non-empty"),
            "unexpected error: {err}"
        );

        let partial = "\
inventory:
  key_a:
    support: partial
    pipelines: [dogstatsd]
    description: \"Key A\"
    documentation: \"diverges\"
    test_support:
      used_by: []
excluded: {}
";
        let err = validate_entries_of(partial).expect_err("partial entry with empty used_by should be rejected");
        assert!(
            err.to_string()
                .contains("partial key 'key_a': used_by must be non-empty"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn per_entry_validation_rejects_planned_unsupported_entry_without_issue() {
        let overlay = "\
inventory:
  key_a:
    support: none
    pipelines: [dogstatsd]
    description: \"Key A\"
    severity: medium
    planned: true
excluded: {}
";
        let err = validate_entries_of(overlay).expect_err("planned unsupported entry without issue should be rejected");
        assert!(
            err.to_string()
                .contains("unsupported key 'key_a': planned requires an issue"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn per_entry_validation_rejects_duplicate_additional_yaml_path() {
        let overlay = "\
inventory:
  key_a:
    support: full
    pipelines: [dogstatsd]
    description: \"Key A\"
    test_support:
      used_by: [TypedConfigSystem]
      additional_yaml_paths: [dup_alias, dup_alias]
excluded: {}
";
        let err = validate_entries_of(overlay).expect_err("duplicate additional_yaml_path should be rejected");
        assert!(
            err.to_string()
                .contains("key 'key_a': duplicate additional_yaml_path 'dup_alias'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn per_entry_validation_rejects_dotted_additional_yaml_path() {
        // Dotted aliases can't be represented as a serde field alias on the generated struct, so they're
        // rejected outright.
        let overlay = "\
inventory:
  key_a:
    support: full
    pipelines: [dogstatsd]
    description: \"Key A\"
    test_support:
      used_by: [TypedConfigSystem]
      additional_yaml_paths: [\"nested.alias\"]
excluded: {}
";
        let err = validate_entries_of(overlay).expect_err("dotted additional_yaml_path should be rejected");
        assert!(
            err.to_string()
                .contains("additional_yaml_path 'nested.alias' contains a dot"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn per_entry_validation_rejects_additional_yaml_path_colliding_with_canonical_key() {
        // `alpha`'s alias `beta` collides with the canonical key `beta`; two fields would deserialize
        // from the same YAML key. Keys are alphabetically ordered so the YAML lint pass is satisfied.
        let overlay = "\
inventory:
  alpha:
    support: full
    pipelines: [dogstatsd]
    description: \"Alpha\"
    test_support:
      used_by: [TypedConfigSystem]
      additional_yaml_paths: [beta]
  beta:
    support: full
    pipelines: [dogstatsd]
    description: \"Beta\"
    test_support:
      used_by: [TypedConfigSystem]
excluded: {}
";
        let err = validate_entries_of(overlay).expect_err("aliasing a canonical key should be rejected");
        assert!(
            err.to_string()
                .contains("additional_yaml_path 'beta' collides with a canonical"),
            "unexpected error: {err}"
        );
    }
}
