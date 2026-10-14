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

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Map, Value};

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

/// A top-level overlay section as seen by [`SchemaOverlay::lint_yaml`]: only the key order of
/// mapping sections matters, so values are skipped.
#[derive(Deserialize)]
#[serde(untagged)]
enum LintSection {
    Mapping(IndexMap<String, serde::de::IgnoredAny>),
    Other(#[allow(dead_code)] serde::de::IgnoredAny),
}

impl SchemaOverlay {
    pub fn load(files: Files) -> Result<Self, Error> {
        let loaded = Self::from_file(&files.overlay)?;
        loaded.validate(&files.datadog_schema, &files.otel_schema_dir)?;
        Ok(loaded)
    }

    fn from_yaml(s: &str) -> Result<Self, Error> {
        // Linting needs the keys in file order, so parse into insertion-ordered maps first.
        let sections: IndexMap<String, LintSection> = serde_saphyr::from_str(s).map_err(Error::Yaml)?;
        Self::lint_yaml(&sections)?;
        serde_saphyr::from_str(s).map_err(Error::Yaml)
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
    fn lint_yaml(sections: &IndexMap<String, LintSection>) -> Result<(), Error> {
        let section_names: Vec<&str> = sections.keys().map(String::as_str).collect();

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
            if let Some(LintSection::Mapping(section)) = sections.get(section_name) {
                let mut prev = "";
                for key in section.keys().map(String::as_str) {
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
            .and_then(|v| v.as_object())
            .ok_or_else(|| Error::Validation("schema missing 'properties' section".to_string()))?;
        let mut keys = HashSet::new();
        Self::collect_schema_keys(props, "", &mut keys);
        Ok(keys)
    }

    fn collect_schema_keys(props: &Map<String, Value>, prefix: &str, keys: &mut HashSet<String>) {
        for (name, v) in props {
            let full_key = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{}.{}", prefix, name)
            };
            // `$ref`s have already been inlined by `load_resolved_schema`, so a node either
            // carries `properties` (recurse) or is a leaf key.
            if let Some(sub_props) = v.get("properties").and_then(|p| p.as_object()) {
                Self::collect_schema_keys(sub_props, &full_key, keys);
            } else {
                keys.insert(full_key);
            }
        }
    }

    /// Validate per-entry constraints: description length and planned+issue consistency for
    /// unsupported entries.
    fn validate_entries(&self) -> Result<(), Error> {
        for (key, entry) in &self.inventory {
            match entry {
                KnownEntry::Full(f) => {
                    if f.description.len() > 50 {
                        return Err(Error::Validation(format!(
                            "full key '{}': description exceeds 50 chars ({} chars)",
                            key,
                            f.description.len()
                        )));
                    }
                }
                KnownEntry::Partial(p) => {
                    if p.description.len() > 50 {
                        return Err(Error::Validation(format!(
                            "partial key '{}': description exceeds 50 chars ({} chars)",
                            key,
                            p.description.len()
                        )));
                    }
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
}

/// Read a YAML file into a [`Value`], mapping failures onto [`Error`].
fn read_yaml(path: &Path) -> Result<Value, Error> {
    let contents = std::fs::read_to_string(path).map_err(|e| Error::Io((path.into(), e)))?;
    serde_saphyr::from_str(&contents).map_err(Error::Yaml)
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
pub fn load_resolved_schema(schema_path: &Path) -> Result<Value, Error> {
    let schema_dir = schema_path.parent().unwrap_or_else(|| Path::new("."));
    let mut doc = read_yaml(schema_path)?;
    resolve_refs(&mut doc, schema_dir)?;
    Ok(doc)
}

/// Recursively replace any mapping node containing a `$ref: <file>` entry with the (also resolved)
/// contents of the referenced file, found relative to `schema_dir`.
fn resolve_refs(value: &mut Value, schema_dir: &Path) -> Result<(), Error> {
    if let Some(map) = value.as_object_mut() {
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

/// Load the Datadog schema with the OTel receiver schema patched in.
pub fn load_composed_schema(datadog_schema: &Path, otel_schema_dir: &Path) -> Result<Value, Error> {
    let mut datadog_schema = load_resolved_schema(datadog_schema)?;
    let otel_receiver = load_otel_receiver(otel_schema_dir)?;
    patch_receiver(&mut datadog_schema, otel_receiver)?;
    Ok(datadog_schema)
}

/// Load and completely resolve the OTel receiver schema into a Datadog compatible subtree.
fn load_otel_receiver(otel_dir: &Path) -> Result<Value, Error> {
    let schema_path = otel_dir.join("config.schema.yaml");
    let doc = read_yaml(&schema_path)?;

    let defs = doc
        .get("$defs")
        .and_then(|v| v.as_object())
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

    // Tag env bindings. Only a subset is is registered and gets an explicit `env_vars` designation; the rest get `no-env`.
    tag_otel_env_bindings(&mut protocols, &["protocols"]);

    // Wrap in a "receiver" section to match the Datadog schema structure.
    let mut receiver_section = Map::new();
    receiver_section.insert("node_type".to_string(), Value::String("section".to_string()));
    receiver_section.insert("type".to_string(), Value::String("object".to_string()));
    let mut properties = Map::new();
    properties.insert("protocols".to_string(), protocols);
    receiver_section.insert("properties".to_string(), Value::Object(properties));

    Ok(Value::Object(receiver_section))
}

/// Merge the resolved OTel receiver subtree into the Datadog schema's `otlp_config.receiver` while
/// honoring default values not native to OTel.
fn patch_receiver(datadog_schema: &mut Value, otel_receiver: Value) -> Result<(), Error> {
    let Some(otlp_config) = datadog_schema
        .get_mut("properties")
        .and_then(|v| v.get_mut("otlp_config"))
    else {
        return Ok(());
    };

    let Some(receiver) = otlp_config.get_mut("properties").and_then(|v| v.get_mut("receiver")) else {
        return Ok(());
    };

    let datadog_receiver = std::mem::take(receiver);
    let mut merged = otel_receiver;
    merge_receiver_node(&mut merged, &datadog_receiver);
    *receiver = merged;
    Ok(())
}

/// Recursively overlay Datadog-specific metadata from the core schema onto the resolved OTel node.
fn merge_receiver_node(otel: &mut Value, datadog: &Value) {
    let Some(otel_map) = otel.as_object_mut() else { return };
    let Some(datadog_map) = datadog.as_object() else {
        return;
    };

    // Overlay all Datadog metadata fields onto the OTel node, except `properties`
    // which is merged recursively to preserve OTel's additional keys.
    for (key, val) in datadog_map.iter() {
        if key == "properties" {
            continue;
        }
        otel_map.insert(key.clone(), val.clone());
    }

    // Recursively merge `properties` containers.
    if let (Some(otel_props), Some(datadog_props)) = (
        otel_map.get_mut("properties").and_then(|v| v.as_object_mut()),
        datadog_map.get("properties").and_then(|v| v.as_object()),
    ) {
        // Merge overlapping OTel properties with corresponding Datadog properties.
        let otel_keys: Vec<String> = otel_props.keys().cloned().collect();
        for key in &otel_keys {
            if let Some(datadog_val) = datadog_props.get(key) {
                if let Some(otel_val) = otel_props.get_mut(key) {
                    merge_receiver_node(otel_val, datadog_val);
                }
            }
        }
        // Add Datadog-only properties that don't exist in OTel.
        for (key, val) in datadog_props.iter() {
            if !otel_props.contains_key(key) {
                otel_props.insert(key.clone(), val.clone());
            }
        }
    }
}

/// Resolve OTel schema references into a flat tree.
fn resolve_otel_refs(value: &mut Value, otel_dir: &Path, current_defs: &Map<String, Value>) -> Result<(), Error> {
    let Some(map) = value.as_object_mut() else {
        return Ok(());
    };

    // 1. Handle $ref: replace this node with the resolved definition.
    if let Some(ref_str) = map.get("$ref").and_then(|v| v.as_str()) {
        // Save sibling keys (everything except $ref): for example x-optional, description.
        let siblings: Vec<(String, Value)> = map
            .iter()
            .filter(|(k, _)| *k != "$ref")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // Resolve the ref target, getting the definition value and its source $defs.
        let (mut resolved, source_defs) = resolve_otel_ref_target(ref_str, otel_dir, current_defs)?;

        // Recursively resolve refs inside the definition using the source file's $defs.
        resolve_otel_refs(&mut resolved, otel_dir, &source_defs)?;

        // Resolve siblings using current_defs (they belong to the current file).
        let mut resolved_siblings: Vec<(String, Value)> = Vec::new();
        for (k, mut v) in siblings {
            resolve_otel_refs(&mut v, otel_dir, current_defs)?;
            resolved_siblings.push((k, v));
        }

        // Merge siblings into the resolved definition (siblings override def keys).
        if let Some(resolved_map) = resolved.as_object_mut() {
            for (k, v) in resolved_siblings {
                resolved_map.insert(k, v);
            }
        }

        *value = resolved;
        return Ok(());
    }

    // 2. Handle allOf: merge each fragment's properties into this node.
    if let Some(allof) = map.remove("allOf") {
        if let Some(allof_seq) = allof.as_array() {
            for fragment in allof_seq {
                let mut resolved_fragment = fragment.clone();
                resolve_otel_refs(&mut resolved_fragment, otel_dir, current_defs)?;

                // Merge fragment's properties into value's properties.
                if let Some(source_props) = resolved_fragment.get("properties").and_then(|v| v.as_object()) {
                    if map.get("properties").is_none() {
                        map.insert("properties".to_string(), Value::Object(Map::new()));
                    }
                    if let Some(target_props) = map.get_mut("properties").and_then(|v| v.as_object_mut()) {
                        for (k, v) in source_props {
                            target_props.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
        }
    }

    // 3. Recurse into properties, stripping auth and middlewares that reference excluded packages.
    if let Some(props) = map.get_mut("properties").and_then(|v| v.as_object_mut()) {
        // Strip auth/middlewares if they have a $ref (direct or via allOf/items) that
        // transitively references configauth/configmiddleware. A plain `auth` field
        // (like tpm_config.auth: type: string) has no $ref and must be kept.
        if let Some(auth) = props.get("auth") {
            let auth_str = serde_json::to_string(auth).unwrap_or_default();
            if auth_str.contains("$ref") {
                props.remove("auth");
            }
        }
        if let Some(middlewares) = props.get("middlewares") {
            let mw_str = serde_json::to_string(middlewares).unwrap_or_default();
            if mw_str.contains("$ref") {
                props.remove("middlewares");
            }
        }
        for (_, v) in props.iter_mut() {
            resolve_otel_refs(v, otel_dir, current_defs)?;
        }
    }

    Ok(())
}

/// Resolve a single `$ref` target, returning the definition value and its source `$defs`.
fn resolve_otel_ref_target(
    ref_str: &str, otel_dir: &Path, current_defs: &Map<String, Value>,
) -> Result<(Value, Map<String, Value>), Error> {
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
                "string" => Value::Object([("type".into(), "string".into())].into_iter().collect()),
                "map_list" => {
                    let mut m = Map::new();
                    m.insert("type".into(), "object".into());
                    let mut addl = Map::new();
                    addl.insert("type".into(), "string".into());
                    m.insert("additionalProperties".into(), Value::Object(addl));
                    Value::Object(m)
                }
                other => return Err(Error::Validation(format!("unknown configopaque ref: {other}"))),
            };
            return Ok((inline, Map::new()));
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
            .and_then(|v| v.as_object())
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

/// Convert an OTel schema tree to the Datadog compatible schema dialect.
fn convert_to_datadog_dialect(value: &mut Value) {
    let Some(map) = value.as_object_mut() else {
        return;
    };

    // Remove OTel-specific extensions.
    map.remove("x-optional");
    map.remove("x-customType");

    // Determine if this is a section (has properties) or a setting (leaf).
    let is_section = map.get("properties").is_some();
    let node_type = if is_section { "section" } else { "setting" };

    // Add node_type if not present.
    if map.get("node_type").is_none() {
        map.insert("node_type".to_string(), Value::String(node_type.to_string()));
    }

    // Recurse into properties.
    if let Some(props) = map.get_mut("properties").and_then(|v| v.as_object_mut()) {
        for (_, v) in props.iter_mut() {
            convert_to_datadog_dialect(v);
        }
    }
}

/// List of env-reachable `otlp_config.receiver` keys
const ENV_REGISTERED_OTEL_KEYS: &[&str] = &[
    "otlp_config.receiver.protocols.grpc.endpoint",
    "otlp_config.receiver.protocols.grpc.transport",
    "otlp_config.receiver.protocols.grpc.max_recv_msg_size_mib",
    "otlp_config.receiver.protocols.grpc.max_concurrent_streams",
    "otlp_config.receiver.protocols.grpc.read_buffer_size",
    "otlp_config.receiver.protocols.grpc.write_buffer_size",
    "otlp_config.receiver.protocols.grpc.include_metadata",
    "otlp_config.receiver.protocols.grpc.keepalive.enforcement_policy.min_time",
    "otlp_config.receiver.protocols.http.endpoint",
    "otlp_config.receiver.protocols.http.max_request_body_size",
    "otlp_config.receiver.protocols.http.include_metadata",
    "otlp_config.receiver.protocols.http.cors.allowed_headers",
    "otlp_config.receiver.protocols.http.cors.allowed_origins",
];

/// Walks the OTel receiver subtree and tags each setting with `env_vars` or `no-env`.
fn tag_otel_env_bindings(value: &mut Value, path_parts: &[&str]) {
    let Some(map) = value.as_object_mut() else { return };

    if map.get("node_type").and_then(|v| v.as_str()) == Some("setting") {
        let full_path = format!("otlp_config.receiver.{}", path_parts.join("."));
        if ENV_REGISTERED_OTEL_KEYS.contains(&full_path.as_str()) {
            let env_var = format!("DD_{}", full_path.replace('.', "_").to_uppercase());
            let env_vars = Value::Array(vec![Value::String(env_var)]);
            map.insert("env_vars".to_string(), env_vars);
        } else {
            let mut tags = map.get("tags").and_then(|v| v.as_array().cloned()).unwrap_or_default();
            if !tags.iter().any(|t| t.as_str() == Some("no-env")) {
                tags.push(Value::String("no-env".to_string()));
            }
            map.insert("tags".to_string(), Value::Array(tags));
        }
        return;
    }

    if let Some(props) = map.get_mut("properties").and_then(|v| v.as_object_mut()) {
        for (key, val) in props.iter_mut() {
            let mut parts = path_parts.to_vec();
            parts.push(key);
            tag_otel_env_bindings(val, &parts);
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
    - full entries: pipelines non-empty, description <= 50 chars.\n\
    - partial entries: pipelines non-empty, description <= 50 chars, documentation required.\n\
    - unsupported entries: pipelines non-empty, description <= 50 chars, planned+issue consistent.\n\
    - unknown entries: description <= 50 chars (when present).\n\
    Fix: edit lib/datadog-agent/config/schema/schema_overlay.yaml.";

/// Errors that can occur when loading a schema overlay.
#[derive(Debug)]
pub enum Error {
    Io((PathBuf, std::io::Error)),
    Yaml(serde_saphyr::Error),
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
        let w: W = serde_saphyr::from_str(yaml).unwrap();
        assert!(matches!(w.pipelines, PipelineAffinity::CrossCutting));
    }

    #[test]
    fn pipeline_affinity_multi() {
        let yaml = "pipelines: [dogstatsd, traces]";
        #[derive(Deserialize)]
        struct W {
            pipelines: PipelineAffinity,
        }
        let w: W = serde_saphyr::from_str(yaml).unwrap();
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
        assert!(serde_saphyr::from_str::<W>(yaml).is_err());
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
  key_a:
    support: full
    pipelines: [cross_cutting]
    description: \"Key A\"
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
  partial_key:
    support: partial
    pipelines: [traces]
    description: \"Partially supported key\"
    documentation: \"Behaves differently from the core agent.\"
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
                     description: \"{long}\"\nexcluded: {{}}\n"
                ),
            ),
            (
                "partial",
                format!(
                    "inventory:\n  key_a:\n    support: partial\n    pipelines: [dogstatsd]\n    \
                     description: \"{long}\"\n    documentation: \"diverges\"\nexcluded: {{}}\n"
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
    fn verify_otel_receiver_env_bindings() {
        let schema_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("config")
            .join("schema")
            .join("core")
            .join("core_schema.yaml");
        let otel_schema_dir = otel_schema_dir_for_tests();
        let schema_map = crate::schema_gen::load_schema(&schema_path, &otel_schema_dir);

        let registered: HashSet<&str> = super::ENV_REGISTERED_OTEL_KEYS.iter().copied().collect();

        for (key, info) in &schema_map {
            if !key.starts_with("otlp_config.receiver.protocols.") {
                continue;
            }
            if registered.contains(key.as_str()) {
                assert!(
                    matches!(info.env, crate::schema_gen::EnvBinding::Overridden(_)),
                    "registered key `{key}` should have explicit env_vars"
                );
            } else {
                assert!(
                    matches!(info.env, crate::schema_gen::EnvBinding::None),
                    "unregistered key `{key}` should be no-env"
                );
            }
        }
    }
}
