//! Configuration key registry.
//!
//! A programmatic registry of all recognized configuration keys. Each entry describes the key
//! purely from the configuration system's perspective: its canonical YAML path, the environment
//! variables that map to it, the shape of its value, and which internal config structs consume it.
//!
//! This registry is intentionally free of Rust field names and struct internals — it models the
//! configuration surface as an operator would see it, and can be used at runtime to detect
//! unknown or unsupported keys in a loaded configuration file.

pub mod datadog;
pub mod generated;

pub use self::datadog::{ALL_ANNOTATIONS, ALL_KEYS};

/// Shared helpers for config smoke tests.
#[cfg(test)]
pub mod test_support;

/// Identifiers for known configuration structs.
///
/// Used as values in [`SalukiAnnotation::used_by`] to declare which structs consume a given key.
/// Adding a new struct here is the first step when registering its configuration keys.
pub mod structs {
    /// `ProxyConfiguration`
    pub const PROXY_CONFIGURATION: &str = "ProxyConfiguration";
}

/// How well saluki supports a given configuration key.
///
/// Used in [`SalukiAnnotation`] to classify each key from saluki's perspective. `Ignored` is
/// reserved for keys in the schema that have no annotation at all — it must not appear in any
/// hand-written annotation.
///
/// Invariants enforced at test time:
/// - `Full` and `Partial` require a non-empty `used_by` list.
/// - `Incompatible` requires an empty `used_by` list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupportLevel {
    /// Fully supported. The key is wired up end-to-end and must be consumed by at least one struct.
    Full,
    /// Partially supported. The key is consumed by at least one struct, but support is incomplete.
    Partial,
    /// Explicitly incompatible. Saluki intentionally does not support this key; `used_by` must be
    /// empty.
    Incompatible,
    /// Not annotated. Applied implicitly at runtime to any schema key that has no
    /// [`SalukiAnnotation`]. Must not be set on a hand-written annotation.
    Ignored,
}

/// The shape of a configuration value.
///
/// Describes how a value should be parsed from both YAML and environment variables.
/// `StringList` values are represented as a YAML sequence or a space-separated string in env vars.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueType {
    /// A boolean (`true` / `false`).
    Bool,
    /// A UTF-8 string.
    String,
    /// An unsigned integer.
    Integer,
    /// A floating-point number.
    Float,
    /// A list of strings (YAML sequence or space-separated env var string).
    StringList,
}

/// Schema-derived metadata for a single configuration key.
///
/// Generated from the vendored Datadog Agent config schema. Contains only what the schema
/// knows: the canonical YAML path, declared environment variables, and value type. Saluki-specific
/// fields (`used_by`, etc.) live in [`SalukiAnnotation`] instead.
///
/// Do not construct these manually — they are produced by `cargo xtask gen-config-schema` and
/// live in `config_registry::generated::schema`.
#[derive(Debug)]
pub struct SchemaEntry {
    /// Canonical dot-separated YAML path for this key (e.g. `"proxy.http"`).
    pub yaml_path: &'static str,

    /// Environment variables that deliver this value, as declared in the schema.
    ///
    /// Empty when the schema marks the key `no-env` or lists no env vars. Annotations may
    /// override this with additional or corrected env vars.
    pub env_vars: &'static [&'static str],

    /// Shape of the value.
    pub value_type: ValueType,
}

/// Saluki-specific annotation for a single configuration key.
///
/// Pairs a [`SchemaEntry`] (generated from the vendored schema) with the metadata that only
/// saluki knows: which internal config structs consume the key, and any corrections to the
/// schema's env var list.
///
/// These are hand-written constants, one per key saluki cares about, and live in
/// `config_registry::datadog::*` submodules. They are never overwritten by codegen.
#[derive(Debug)]
pub struct SalukiAnnotation {
    /// The schema entry this annotation enriches.
    pub schema: &'static SchemaEntry,

    /// How well saluki supports this key.
    ///
    /// Must not be [`SupportLevel::Ignored`] — that level is reserved for unannotated schema keys.
    pub support_level: SupportLevel,

    /// Additional YAML paths beyond the canonical one in the schema (aliases).
    ///
    /// Most keys have no aliases; leave this as `&[]` unless the config system recognises
    /// the key under more than one dot-separated path.
    pub additional_yaml_paths: &'static [&'static str],

    /// Overrides the schema's `env_vars` list entirely when `Some`.
    ///
    /// Use when the schema marks a key `no-env` but env vars are actually supported, or when
    /// the schema's list is incorrect or incomplete (e.g. the proxy sub-keys).
    pub env_var_override: Option<&'static [&'static str]>,

    /// Config structs that incorporate this key, as [`structs`] constants.
    ///
    /// Must be non-empty for [`SupportLevel::Full`] and [`SupportLevel::Partial`].
    /// Must be empty for [`SupportLevel::Incompatible`].
    pub used_by: &'static [&'static str],
}

impl SalukiAnnotation {
    /// The canonical YAML path for this key (from the schema).
    pub fn yaml_path(&self) -> &'static str {
        self.schema.yaml_path
    }

    /// All YAML paths for this key: canonical first, then any aliases.
    pub fn all_yaml_paths(&self) -> impl Iterator<Item = &'static str> {
        std::iter::once(self.schema.yaml_path).chain(self.additional_yaml_paths.iter().copied())
    }

    /// Effective env vars: the override list if set, otherwise the schema's list.
    pub fn effective_env_vars(&self) -> &'static [&'static str] {
        self.env_var_override.unwrap_or(self.schema.env_vars)
    }

    /// Shape of the value (from the schema).
    pub fn value_type(&self) -> ValueType {
        self.schema.value_type
    }
}

/// A fully resolved configuration key, derived from a [`SalukiAnnotation`] at registry init time.
///
/// Used for runtime unknown-key detection and anywhere a flattened, owned view of a key is
/// needed. For test infrastructure, prefer working with [`SalukiAnnotation`] directly.
#[derive(Debug)]
pub struct ConfigKey {
    /// All dot-separated YAML paths that deliver this value.
    pub yaml_paths: Vec<&'static str>,

    /// All environment variables that deliver this value.
    pub env_vars: Vec<&'static str>,

    /// Shape of the value.
    pub value_type: ValueType,

    /// Config structs that incorporate this key, as [`structs`] constants.
    pub used_by: &'static [&'static str],
}

impl From<&SalukiAnnotation> for ConfigKey {
    fn from(a: &SalukiAnnotation) -> Self {
        ConfigKey {
            yaml_paths: a.all_yaml_paths().collect(),
            env_vars: a.effective_env_vars().to_vec(),
            value_type: a.value_type(),
            used_by: a.used_by,
        }
    }
}
