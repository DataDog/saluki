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

pub use self::datadog::ALL_KEYS;

/// Shared helpers for config smoke tests.
#[cfg(test)]
pub mod test_support;

/// Identifiers for known configuration structs.
///
/// Used as values in [`ConfigKey::used_by`] to declare which structs consume a given key. Adding
/// a new struct here is the first step when registering its configuration keys.
pub mod structs {
    /// `ProxyConfiguration`
    pub const PROXY_CONFIGURATION: &str = "ProxyConfiguration";
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
/// fields (`used_by`, `support_level`, etc.) live in [`ConfigKey`] annotations instead.
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

/// Specification for a single recognized configuration key.
///
/// Models the key from the operator's perspective: where it lives in config files, which
/// environment variables carry it, what kind of value it holds, and which internal config
/// structs it feeds.
#[derive(Debug)]
pub struct ConfigKey {
    /// All dot-separated YAML paths that deliver this value (e.g. `&["proxy.http"]`).
    ///
    /// Most keys have a single path, but some may be reachable via multiple aliases in the config
    /// file. Each path is aliased to a flat key at file-load time via `KEY_ALIASES`.
    pub yaml_paths: &'static [&'static str],

    /// All environment variables that deliver this value, in precedence order (highest last).
    ///
    /// Includes the `DD_`-prefixed canonical form and any non-`DD_` variables accepted via
    /// `DatadogRemapper` (e.g. `HTTP_PROXY`). The `DD_` form is always listed first.
    pub env_vars: &'static [&'static str],

    /// Shape of the value.
    pub value_type: ValueType,

    /// Config structs that incorporate this key, as [`structs`] constants.
    pub used_by: &'static [&'static str],
}
