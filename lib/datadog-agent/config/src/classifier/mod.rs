//! Configuration key classifier.
//!
//! A programmatic registry of all recognized configuration keys. Each entry describes the key
//! purely from the configuration system's perspective: its canonical YAML path, the environment
//! variables that map to it, the shape of its value, and which internal config structs consume it.
//!
//! This registry is intentionally free of Rust field names and struct internals—it models the
//! configuration surface as an operator would see it, and can be used at runtime to detect
//! unknown or unsupported keys in a loaded configuration file.
//!
//! ## User Guide
//!
//! The registry is generated at build time from `schema_overlay.yaml`, which partitions every
//! key in `core_schema.yaml` into exactly one of: supported, unsupported, or ignored. Data
//! integrity (uniqueness, full coverage, sorted sections) is enforced by `SchemaOverlay::load()`
//! during the build.
//!
//! ### Adding a Configuration Key
//!
//! Add a `supported` entry in `lib/datadog-agent/config/schema/schema_overlay.yaml` with
//! `support_level`, `pipelines`, `used_by`, `description`, and `config_registry_filename`.
//! The build generates the annotation constants automatically.
//!
//! ### Updating the Vendored Schema
//!
//! After updating `core_schema.yaml`, the build will fail if any new keys are not covered
//! by the overlay. For each new key, add it to the appropriate section of
//! `schema_overlay.yaml`.

#[allow(clippy::module_inception)]
mod classifier;

pub use classifier::{Classification, ConfigClassifier};

/// Identifiers for known configuration structs.
///
/// Used as values in annotation `used_by` fields to declare which structs consume a given key.
/// Adding a new struct here is the first step when registering its configuration keys.
pub mod structs {
    /// Identifier for `ProxyConfiguration`.
    pub const PROXY_CONFIGURATION: &str = "ProxyConfiguration";
    /// Identifier for `ForwarderConfiguration`.
    pub const FORWARDER_CONFIGURATION: &str = "ForwarderConfiguration";
    /// Identifier for `DogStatsDConfiguration`.
    pub const DOGSTATSD_CONFIGURATION: &str = "DogStatsDConfiguration";
    /// Identifier for `ContainerdConfiguration`.
    pub const CONTAINERD_CONFIGURATION: &str = "ContainerdConfiguration";
    /// Identifier for `OtlpConfiguration`.
    pub const OTLP_CONFIGURATION: &str = "OtlpConfiguration";
    /// Identifier for `AggregateConfiguration`.
    pub const AGGREGATE_CONFIGURATION: &str = "AggregateConfiguration";
    /// Identifier for `DogStatsDMapperConfiguration`.
    pub const DOGSTATSD_MAPPER_CONFIGURATION: &str = "DogStatsDMapperConfiguration";
    /// Identifier for `DogStatsDDebugLogConfiguration`.
    pub const DOGSTATSD_DEBUG_LOG_CONFIGURATION: &str = "DogStatsDDebugLogConfiguration";
    /// Identifier for `DogStatsDPrefixFilterConfiguration`.
    pub const DOGSTATSD_PREFIX_FILTER_CONFIGURATION: &str = "DogStatsDPrefixFilterConfiguration";
    /// Identifier for `DatadogMetricsConfiguration`.
    pub const DATADOG_METRICS_CONFIGURATION: &str = "DatadogMetricsConfiguration";
    /// Identifier for `DatadogTraceConfiguration`.
    pub const DATADOG_TRACE_CONFIGURATION: &str = "DatadogTraceConfiguration";
    /// Identifier for `DatadogLogsConfiguration`.
    pub const DATADOG_LOGS_CONFIGURATION: &str = "DatadogLogsConfiguration";
    /// Identifier for `DatadogEventsConfiguration`.
    pub const DATADOG_EVENTS_CONFIGURATION: &str = "DatadogEventsConfiguration";
    /// Identifier for `DatadogServiceChecksConfiguration`.
    pub const DATADOG_SERVICE_CHECKS_CONFIGURATION: &str = "DatadogServiceChecksConfiguration";
    /// Identifier for `DatadogApmStatsEncoderConfiguration`.
    pub const DATADOG_APM_STATS_ENCODER_CONFIGURATION: &str = "DatadogApmStatsEncoderConfiguration";
    /// Identifier for `MrfConfiguration`.
    pub const MRF_CONFIGURATION: &str = "MrfConfiguration";
    /// Identifier for `OtlpDecoderConfiguration`.
    pub const OTLP_DECODER_CONFIGURATION: &str = "OtlpDecoderConfiguration";
    /// Identifier for `OtlpRelayConfiguration`.
    pub const OTLP_RELAY_CONFIGURATION: &str = "OtlpRelayConfiguration";
    /// Identifier for `TraceObfuscationConfiguration`.
    pub const TRACE_OBFUSCATION_CONFIGURATION: &str = "TraceObfuscationConfiguration";
    /// Identifier for `RemoteAgentClientConfiguration`.
    pub const REMOTE_AGENT_CLIENT_CONFIGURATION: &str = "RemoteAgentClientConfiguration";
    /// Identifier for `TagFilterlistConfiguration`.
    pub const TAG_FILTERLIST_CONFIGURATION: &str = "TagFilterlistConfiguration";
    /// Keys read via `get_typed` / `try_get_typed` rather than struct deserialization.
    pub const GET_TYPED: &str = "get_typed";
}

/// The ADP pipeline a config key affects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Pipeline {
    /// DogStatsD metrics pipeline.
    DogStatsD,
    /// Agent checks pipeline.
    Checks,
    /// OTLP ingestion frontend.
    Otlp,
    /// Internal trace processing. Active when OTLP is enabled and proxy/relay mode (which uses the
    /// core Agent for transport) is off.
    Traces,
}

/// Which pipelines a config key affects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipelineAffinity {
    /// The list of pipelines affected by the key.
    ///
    /// This list must be non-empty, enforced by test.
    Pipelines(&'static [Pipeline]),
    /// The key affects all pipelines or ADP behavior as a whole.
    CrossCutting,
}

/// The `Severity` level of a config key that Saluki doesn't support.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Saluki's incompatibility with the key is considered minor.
    Low,

    /// Saluki's incompatibility with the key is considered potentially impactful.
    Medium,

    /// Saluki's incompatibility with the key is considered problematic.
    High,
}

/// The support level for a given configuration key.
///
/// Full support is omitted from the enum and those keys are not classified since there is nothing
/// to be done about them downstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupportLevel {
    /// Partially supported.
    Partial,
    /// Explicitly incompatible.
    Incompatible(Severity),
    /// Intentionally ignored.
    #[allow(unused)]
    Ignored,
    /// Unrecognized.
    #[allow(unused)]
    Unrecognized,
}

/// Slim per-key data generated at build time for the classifier.
///
/// Carries only what the classifier needs: enough to look up a key, determine its support level,
/// check whether a value is the default, and report which pipelines are affected.
pub struct ClassifierEntry {
    /// Canonical dot-separated YAML path.
    pub yaml_path: &'static str,
    /// Additional YAML paths (aliases) that resolve to this key.
    pub aliases: &'static [&'static str],
    /// How well saluki supports this key.
    pub support_level: SupportLevel,
    /// Which pipelines this key affects.
    pub pipeline_affinity: PipelineAffinity,
    /// JSON-encoded default value from the Agent schema, if present.
    pub default: Option<&'static str>,
}

mod classifier_data;
use classifier_data::CLASSIFIER_ENTRIES;
