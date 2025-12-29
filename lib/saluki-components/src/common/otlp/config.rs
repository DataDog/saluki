//! Shared OTLP receiver configuration.

use serde::Deserialize;

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

/// Receiver configuration for OTLP endpoints.
///
/// This follows the Datadog Agent `otlp_config.receiver` structure.
#[derive(Deserialize, Debug, Default)]
pub struct Receiver {
    /// Protocol-specific receiver configuration.
    #[serde(default)]
    pub protocols: Protocols,
}

/// Protocol configuration for OTLP receiver.
#[derive(Deserialize, Debug, Default)]
pub struct Protocols {
    /// gRPC protocol configuration.
    #[serde(default)]
    pub grpc: GrpcConfig,

    /// HTTP protocol configuration.
    #[serde(default)]
    pub http: HttpConfig,
}

/// gRPC receiver configuration.
#[derive(Deserialize, Debug)]
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
#[derive(Deserialize, Debug)]
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

/// Configuration for OTLP metrics processing.
#[derive(Deserialize, Debug)]
pub struct MetricsConfig {
    /// Whether to enable OTLP metrics support.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_metrics_enabled")]
    pub enabled: bool,
}

fn default_metrics_enabled() -> bool {
    true
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: default_metrics_enabled(),
        }
    }
}

/// Configuration for OTLP traces processing.
///
/// Mirrors the Agent's `otlp_config.traces` configuration.
#[derive(Clone, Deserialize, Debug)]
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
    /// Defaults to `false`.
    #[serde(default = "default_enable_otlp_compute_top_level_by_span_kind")]
    pub enable_otlp_compute_top_level_by_span_kind: bool,

    /// Probabilistic sampler configuration for OTLP traces.
    ///
    /// Corresponds to `otlp_config.traces.probabilistic_sampler` in the Agent.
    #[serde(default)]
    pub probabilistic_sampler: ProbabilisticSampler,
}

/// Configuration for OTLP traces probabilistic sampling.
#[derive(Clone, Deserialize, Debug)]
pub struct ProbabilisticSampler {
    /// Percentage of traces to ingest (0, 100].
    ///
    /// Invalid values (<= 0 || > 100) are disconsidered and the default is used.
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
    false
}

fn default_traces_enabled() -> bool {
    true
}

impl Default for TracesConfig {
    fn default() -> Self {
        Self {
            enabled: default_traces_enabled(),
            ignore_missing_datadog_fields: false,
            enable_otlp_compute_top_level_by_span_kind: default_enable_otlp_compute_top_level_by_span_kind(),
            probabilistic_sampler: ProbabilisticSampler::default(),
        }
    }
}
