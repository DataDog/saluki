//! Shared OTLP receiver configuration.

use agent_data_plane_config::domains::otlp::{CumulativeMonotonicMode, HistogramMode, InitialCumulativeMonotonicValue};
use bytesize::ByteSize;
use facet::Facet;
use saluki_config::GenericConfiguration;
use saluki_error::{generic_error, GenericError};
use serde::{de::Error as _, Deserialize, Deserializer};

fn default_grpc_endpoint() -> String {
    "0.0.0.0:4317".to_string()
}

fn default_http_endpoint() -> String {
    "0.0.0.0:4318".to_string()
}

fn default_transport() -> String {
    "tcp".to_string()
}

fn default_max_recv_msg_size_mib() -> u64 {
    4
}

pub(crate) const fn default_traces_string_interner_size() -> ByteSize {
    ByteSize::kib(512)
}

/// Receiver configuration for OTLP endpoints.
///
/// This follows the Datadog Agent `otlp_config.receiver` structure.
#[derive(Deserialize, Debug, Default, Facet)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct Receiver {
    /// Protocol-specific receiver configuration.
    #[serde(default)]
    pub protocols: Protocols,
}

/// Protocol configuration for OTLP receiver.
#[derive(Deserialize, Debug, Default, Facet)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct Protocols {
    /// gRPC protocol configuration.
    #[serde(default)]
    pub grpc: GrpcConfig,

    /// HTTP protocol configuration.
    #[serde(default)]
    pub http: HttpConfig,
}

/// gRPC receiver configuration.
#[derive(Deserialize, Debug, Facet)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct GrpcConfig {
    /// The gRPC endpoint to listen on for OTLP requests.
    ///
    /// Defaults to `0.0.0.0:4317`.
    #[serde(default = "default_grpc_endpoint")]
    pub endpoint: String,

    /// The transport protocol to use for the gRPC listener.
    ///
    /// Defaults to `tcp`.
    #[serde(default = "default_transport")]
    pub transport: String,

    /// Maximum size (in MiB) of a gRPC message that can be received.
    ///
    /// Defaults to 4 MiB.
    #[serde(default = "default_max_recv_msg_size_mib", rename = "max_recv_msg_size_mib")]
    pub max_recv_msg_size_mib: u64,
}

/// HTTP receiver configuration.
#[derive(Deserialize, Debug, Facet)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct HttpConfig {
    /// The HTTP endpoint to listen on for OTLP requests.
    ///
    /// Defaults to `0.0.0.0:4318`.
    #[serde(default = "default_http_endpoint")]
    pub endpoint: String,

    /// The transport protocol to use for the HTTP listener.
    ///
    /// Defaults to `tcp`.
    #[serde(default = "default_transport")]
    pub transport: String,
}

impl Default for GrpcConfig {
    fn default() -> Self {
        Self {
            endpoint: default_grpc_endpoint(),
            transport: default_transport(),
            max_recv_msg_size_mib: default_max_recv_msg_size_mib(),
        }
    }
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            endpoint: default_http_endpoint(),
            transport: default_transport(),
        }
    }
}

/// OTLP configuration.
///
/// This mirrors the Agent's `otlp_config` and contains configuration for
/// the OTLP receiver as well as signal-specific settings (metrics, logs, traces).
#[derive(Deserialize, Debug, Default)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct OtlpConfig {
    /// OTLP receiver configuration.
    #[serde(default)]
    pub receiver: Receiver,

    /// Metrics-specific OTLP configuration.
    #[serde(default)]
    pub metrics: MetricsConfig,

    /// Logs-specific OTLP configuration.
    #[serde(default)]
    pub logs: LogsConfig,

    /// Traces-specific OTLP configuration.
    #[serde(default)]
    pub traces: TracesConfig,
}

/// Configuration for OTLP logs processing.
#[derive(Deserialize, Debug)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct LogsConfig {
    /// Whether to enable OTLP logs support.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_logs_enabled")]
    pub enabled: bool,
}

fn default_logs_enabled() -> bool {
    true
}

impl Default for LogsConfig {
    fn default() -> Self {
        Self {
            enabled: default_logs_enabled(),
        }
    }
}

// TODO: delete when this component uses typed config
fn deserialize_histogram_mode<'de, D>(deserializer: D) -> Result<HistogramMode, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer)?.parse().map_err(D::Error::custom)
}

/// Configuration for OTLP histogram processing.
#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct HistogramsConfig {
    /// Controls how histogram buckets are reported.
    ///
    /// Use `distributions` for distribution metrics, `counters` for one metric per bucket, or
    /// `nobuckets` to omit bucket metrics. The `nobuckets` mode requires histogram aggregation
    /// metrics to be enabled separately.
    ///
    /// Defaults to `distributions`.
    #[serde(default, deserialize_with = "deserialize_histogram_mode")]
    pub mode: HistogramMode,
}

// TODO: delete when this component uses typed config
fn deserialize_cumulative_monotonic_mode<'de, D>(deserializer: D) -> Result<CumulativeMonotonicMode, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer)?.parse().map_err(D::Error::custom)
}

// TODO: delete when this component uses typed config
fn deserialize_initial_cumulative_monotonic_value<'de, D>(
    deserializer: D,
) -> Result<InitialCumulativeMonotonicValue, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer)?.parse().map_err(D::Error::custom)
}

/// Configuration for OTLP metrics processing.
#[derive(Deserialize, Debug)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct MetricsConfig {
    /// Whether to enable OTLP metrics support.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_metrics_enabled")]
    pub enabled: bool,

    /// Histogram processing configuration.
    #[serde(default)]
    pub histograms: HistogramsConfig,

    /// Whether to add scalar resource attributes as raw tags on emitted metrics.
    ///
    /// Recognized mappings are always applied. When enabled, supported scalar resource attributes
    /// (string, bool, integer, float) are also emitted as raw `key:value` tags under their original
    /// keys. Corresponds to `otlp_config.metrics.resource_attributes_as_tags`.
    ///
    /// Defaults to `false`.
    #[serde(default)]
    pub resource_attributes_as_tags: bool,

    /// Comma-separated tags added to every emitted metric.
    ///
    /// Corresponds to `otlp_config.metrics.tags`.
    ///
    /// Defaults to empty.
    #[serde(default)]
    pub tags: String,

    /// Configuration for OTLP sums.
    #[serde(default)]
    pub sums: SumsConfig,
}

/// Configuration for OTLP sums.
#[derive(Deserialize, Debug, Default)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct SumsConfig {
    /// Controls how cumulative monotonic sums are emitted.
    ///
    /// The default `to_delta` converts each cumulative value to a delta and emits it as a count. Set this to
    /// `raw_value` to emit the cumulative value as a gauge instead. This affects only cumulative monotonic sums;
    /// delta sums and non-monotonic sums retain their existing behavior. Use `raw_value` when the receiver of the
    /// translated metrics needs the original cumulative value.
    ///
    /// Corresponds to `otlp_config.metrics.sums.cumulative_monotonic_mode`.
    #[serde(default, deserialize_with = "deserialize_cumulative_monotonic_mode")]
    pub cumulative_monotonic_mode: CumulativeMonotonicMode,

    /// Controls how the first value of a cumulative monotonic sum is emitted.
    ///
    /// The default `auto` reports the first value only when its series started after the translator process. Set this
    /// to `drop` to always discard the first value or `keep` to always report it. This affects only cumulative
    /// monotonic sums in `to_delta` mode; `raw_value` emits every value as a gauge.
    ///
    /// Corresponds to `otlp_config.metrics.sums.initial_cumulative_monotonic_value`.
    #[serde(default, deserialize_with = "deserialize_initial_cumulative_monotonic_value")]
    pub initial_cumulative_monotonic_value: InitialCumulativeMonotonicValue,
}

fn default_metrics_enabled() -> bool {
    true
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: default_metrics_enabled(),
            histograms: HistogramsConfig::default(),
            resource_attributes_as_tags: false,
            tags: String::new(),
            sums: SumsConfig::default(),
        }
    }
}

//TODO: remove env handling with typed config, unblocked by #2094
impl MetricsConfig {
    /// Applies environment-variable overrides for sum settings that normal nested deserialization cannot read.
    pub(crate) fn apply_env_overrides(&mut self, config: &GenericConfiguration) -> Result<(), GenericError> {
        if let Some(raw_mode) = config.try_get_typed::<String>("otlp_config_metrics_sums_cumulative_monotonic_mode")? {
            self.sums.cumulative_monotonic_mode = raw_mode.parse().map_err(|error| {
                generic_error!(
                    "invalid `otlp_config.metrics.sums.cumulative_monotonic_mode` environment override: {error}"
                )
            })?;
        }
        if let Some(raw_value) =
            config.try_get_typed::<String>("otlp_config_metrics_sums_initial_cumulative_monotonic_value")?
        {
            self.sums.initial_cumulative_monotonic_value = raw_value.parse().map_err(|error| {
                generic_error!(
                    "invalid `otlp_config.metrics.sums.initial_cumulative_monotonic_value` environment override: {error}"
                )
            })?;
        }
        Ok(())
    }
}

/// Configuration for OTLP traces processing.
///
/// Mirrors the Agent's `otlp_config.traces` configuration.
#[derive(Clone, Deserialize, Debug)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct TracesConfig {
    /// Whether to enable OTLP traces support.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_traces_enabled")]
    pub enabled: bool,

    /// Whether to skip deriving Datadog fields from standard OTLP attributes.
    ///
    /// When true, only uses explicit `datadog.*` prefixed attributes and skips
    /// fallback resolution from OTLP semantic conventions.
    ///
    /// Corresponds to `otlp_config.traces.ignore_missing_datadog_fields` in the Agent.
    ///
    /// Defaults to `false`.
    #[serde(default)]
    pub ignore_missing_datadog_fields: bool,

    /// When true, `_top_level` and `_dd.measured` are derived using the OTLP span kind.
    ///
    /// Corresponds to the `enable_otlp_compute_top_level_by_span_kind` feature flag
    /// in the Agent's `apm_config.features`.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_enable_otlp_compute_top_level_by_span_kind")]
    pub enable_otlp_compute_top_level_by_span_kind: bool,

    /// Probabilistic sampler configuration for OTLP traces.
    ///
    /// Corresponds to `otlp_config.traces.probabilistic_sampler` in the Agent.
    #[serde(default)]
    pub probabilistic_sampler: ProbabilisticSampler,

    /// Total size of the string interner used for OTLP traces.
    ///
    /// Defaults to 512 KiB.
    #[serde(rename = "string_interner_size", default = "default_traces_string_interner_size")]
    pub string_interner_bytes: ByteSize,

    /// The internal port on the Core Agent to forward traces to.
    ///
    /// Defaults to 5003.
    #[serde(default = "default_internal_port")]
    #[allow(unused)]
    pub internal_port: u16,
}

const fn default_internal_port() -> u16 {
    5003
}

/// Configuration for OTLP traces probabilistic sampling.
#[derive(Clone, Deserialize, Debug)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct ProbabilisticSampler {
    /// Percentage of traces to ingest (0, 100].
    ///
    /// Invalid values (<= 0 || > 100) are disregarded and the default is used.
    ///
    /// Corresponds to `otlp_config.traces.probabilistic_sampler.sampling_percentage` in the Agent.
    ///
    /// Defaults to 100.0 (100% sampling).
    #[serde(default = "default_sampling_percentage")]
    pub sampling_percentage: f64,
}

const fn default_sampling_percentage() -> f64 {
    100.0
}

impl Default for ProbabilisticSampler {
    fn default() -> Self {
        Self {
            sampling_percentage: default_sampling_percentage(),
        }
    }
}

const fn default_enable_otlp_compute_top_level_by_span_kind() -> bool {
    true
}

fn default_traces_enabled() -> bool {
    true
}

impl TracesConfig {
    /// Applies env var overrides for keys whose `DD_`-stripped flat form can't reach the nested
    /// struct through normal serde deserialization.
    ///
    /// `DD_OTLP_CONFIG_TRACES_PROBABILISTIC_SAMPLER_SAMPLING_PERCENTAGE` strips to flat Figment key
    /// `otlp_config_traces_probabilistic_sampler_sampling_percentage`. KEY_ALIASES ensures YAML and
    /// env var land on the same key, but a nested struct can't see a flat key—so we read it
    /// explicitly and override.
    pub(crate) fn apply_env_overrides(&mut self, config: &GenericConfiguration) -> Result<(), GenericError> {
        if let Some(pct) =
            config.try_get_typed::<f64>("otlp_config_traces_probabilistic_sampler_sampling_percentage")?
        {
            self.probabilistic_sampler.sampling_percentage = pct;
        }
        Ok(())
    }
}

impl Default for TracesConfig {
    fn default() -> Self {
        Self {
            enabled: default_traces_enabled(),
            ignore_missing_datadog_fields: false,
            enable_otlp_compute_top_level_by_span_kind: default_enable_otlp_compute_top_level_by_span_kind(),
            probabilistic_sampler: ProbabilisticSampler::default(),
            string_interner_bytes: default_traces_string_interner_size(),
            internal_port: default_internal_port(),
        }
    }
}
