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

/// Declares a set of [`SalukiAnnotation`] constants and generates a companion `ALL` slice.
///
/// Each entry declares one named `pub const` annotation. The macro also emits:
///
/// ```ignore
/// pub const ALL: &[&SalukiAnnotation] = &[&NAME1, &NAME2, ...];
/// ```
///
/// so that `datadog/mod.rs` can aggregate submodules with a single
/// `v.extend_from_slice(my_module::ALL)` line rather than listing every constant by name.
///
/// # Example
///
/// ```ignore
/// declare_annotations! {
///     /// Doc comment for PROXY_HTTP.
///     PROXY_HTTP = SalukiAnnotation { ... };
///     PROXY_HTTPS = SalukiAnnotation { ... };
/// }
/// ```
#[macro_export]
macro_rules! declare_annotations {
    ( $( $(#[$attr:meta])* $name:ident = $val:expr ;)+ ) => {
        $(
            $(#[$attr])*
            pub const $name: $crate::config_registry::SalukiAnnotation = $val;
        )+

        /// All annotations declared in this module, in declaration order.
        pub const ALL: &[$crate::config_registry::SalukiAnnotationRef] = &[
            $( &$name, )+
        ];
    };
}

/// Shared helpers for config smoke tests.
#[cfg(test)]
pub mod test_support;

/// Identifiers for known configuration structs.
///
/// Used as values in [`SalukiAnnotation::used_by`] to declare which structs consume a given key.
/// Adding a new struct here is the first step when registering its configuration keys.
pub mod structs {
    /// Identifier for [`crate::common::datadog::proxy::ProxyConfiguration`].
    pub const PROXY_CONFIGURATION: &str = "ProxyConfiguration";
    /// Identifier for [`crate::common::datadog::config::ForwarderConfiguration`].
    pub const FORWARDER_CONFIGURATION: &str = "ForwarderConfiguration";
    /// Identifier for [`crate::sources::dogstatsd::DogStatsDConfiguration`].
    pub const DOGSTATSD_CONFIGURATION: &str = "DogStatsDConfiguration";
    /// Identifier for [`crate::sources::otlp::OtlpConfiguration`].
    pub const OTLP_CONFIGURATION: &str = "OtlpConfiguration";
    /// Identifier for [`crate::transforms::aggregate::AggregateConfiguration`].
    pub const AGGREGATE_CONFIGURATION: &str = "AggregateConfiguration";
    /// Identifier for [`crate::transforms::dogstatsd_mapper::DogStatsDMapperConfiguration`].
    pub const DOGSTATSD_MAPPER_CONFIGURATION: &str = "DogStatsDMapperConfiguration";
    /// Identifier for [`crate::transforms::dogstatsd_prefix_filter::DogStatsDPrefixFilterConfiguration`].
    pub const DOGSTATSD_PREFIX_FILTER_CONFIGURATION: &str = "DogStatsDPrefixFilterConfiguration";
    /// Identifier for [`crate::encoders::datadog::metrics::DatadogMetricsConfiguration`].
    pub const DATADOG_METRICS_CONFIGURATION: &str = "DatadogMetricsConfiguration";
    /// Identifier for [`crate::encoders::datadog::traces::DatadogTraceConfiguration`].
    pub const DATADOG_TRACE_CONFIGURATION: &str = "DatadogTraceConfiguration";
    /// Identifier for [`crate::encoders::datadog::logs::DatadogLogsConfiguration`].
    pub const DATADOG_LOGS_CONFIGURATION: &str = "DatadogLogsConfiguration";
    /// Identifier for [`crate::encoders::datadog::events::DatadogEventsConfiguration`].
    pub const DATADOG_EVENTS_CONFIGURATION: &str = "DatadogEventsConfiguration";
    /// Identifier for [`crate::encoders::datadog::service_checks::DatadogServiceChecksConfiguration`].
    pub const DATADOG_SERVICE_CHECKS_CONFIGURATION: &str = "DatadogServiceChecksConfiguration";
    /// Identifier for [`crate::encoders::datadog::stats::DatadogApmStatsEncoderConfiguration`].
    pub const DATADOG_APM_STATS_ENCODER_CONFIGURATION: &str = "DatadogApmStatsEncoderConfiguration";
    /// Identifier for [`crate::decoders::otlp::OtlpDecoderConfiguration`].
    pub const OTLP_DECODER_CONFIGURATION: &str = "OtlpDecoderConfiguration";
    /// Identifier for [`crate::relays::otlp::OtlpRelayConfiguration`].
    pub const OTLP_RELAY_CONFIGURATION: &str = "OtlpRelayConfiguration";
    /// Identifier for [`crate::transforms::trace_obfuscation::TraceObfuscationConfiguration`].
    pub const TRACE_OBFUSCATION_CONFIGURATION: &str = "TraceObfuscationConfiguration";
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

/// A reference to a [`SalukiAnnotation`], used as the element type of `ALL` slices generated by
/// [`declare_annotations!`].
pub type SalukiAnnotationRef = &'static SalukiAnnotation;

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
