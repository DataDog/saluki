//! Shared OTLP receiver configuration.
//!
//! These runtime types mirror their leaf counterparts in `saluki_component_config::otlp` and are
//! built from them via `from_native`. They carry no source-language serde attributes; the
//! configuration system owns source-to-native mapping (including env-var overrides that previously
//! lived in `apply_env_overrides`).

use bytesize::ByteSize;
use saluki_component_config::otlp as leaf;

/// Receiver configuration for OTLP endpoints.
#[derive(Debug, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub struct Receiver {
    /// Protocol-specific receiver configuration.
    pub protocols: Protocols,
}

impl Receiver {
    /// Builds the runtime receiver configuration from its leaf mirror.
    pub fn from_native(cfg: &leaf::Receiver) -> Self {
        Self {
            protocols: Protocols::from_native(&cfg.protocols),
        }
    }
}

/// Protocol configuration for OTLP receiver.
#[derive(Debug, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub struct Protocols {
    /// gRPC protocol configuration.
    pub grpc: GrpcConfig,

    /// HTTP protocol configuration.
    pub http: HttpConfig,
}

impl Protocols {
    fn from_native(cfg: &leaf::Protocols) -> Self {
        Self {
            grpc: GrpcConfig::from_native(&cfg.grpc),
            http: HttpConfig::from_native(&cfg.http),
        }
    }
}

/// gRPC receiver configuration.
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub struct GrpcConfig {
    /// The gRPC endpoint to listen on for OTLP requests.
    ///
    /// Defaults to `0.0.0.0:4317`.
    pub endpoint: String,

    /// The transport protocol to use for the gRPC listener.
    ///
    /// Defaults to `tcp`.
    pub transport: String,

    /// Maximum size (in MiB) of a gRPC message that can be received.
    ///
    /// Defaults to 4 MiB.
    pub max_recv_msg_size_mib: u64,
}

impl GrpcConfig {
    fn from_native(cfg: &leaf::GrpcConfig) -> Self {
        Self {
            endpoint: cfg.endpoint.clone(),
            transport: cfg.transport.clone(),
            max_recv_msg_size_mib: cfg.max_recv_msg_size_mib,
        }
    }
}

/// HTTP receiver configuration.
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub struct HttpConfig {
    /// The HTTP endpoint to listen on for OTLP requests.
    ///
    /// Defaults to `0.0.0.0:4318`.
    pub endpoint: String,

    /// The transport protocol to use for the HTTP listener.
    ///
    /// Defaults to `tcp`.
    pub transport: String,
}

impl HttpConfig {
    fn from_native(cfg: &leaf::HttpConfig) -> Self {
        Self {
            endpoint: cfg.endpoint.clone(),
            transport: cfg.transport.clone(),
        }
    }
}

impl Default for GrpcConfig {
    fn default() -> Self {
        Self::from_native(&leaf::GrpcConfig::default())
    }
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self::from_native(&leaf::HttpConfig::default())
    }
}

/// OTLP configuration.
///
/// This mirrors the leaf `OtlpConfig` and contains configuration for the OTLP receiver as well as
/// signal-specific settings (metrics, logs, traces).
#[derive(Debug, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub struct OtlpConfig {
    /// OTLP receiver configuration.
    pub receiver: Receiver,

    /// Metrics-specific OTLP configuration.
    pub metrics: MetricsConfig,

    /// Logs-specific OTLP configuration.
    pub logs: LogsConfig,

    /// Traces-specific OTLP configuration.
    pub traces: TracesConfig,
}

impl OtlpConfig {
    /// Builds the runtime OTLP configuration from its leaf mirror.
    pub fn from_native(cfg: &leaf::OtlpConfig) -> Self {
        Self {
            receiver: Receiver::from_native(&cfg.receiver),
            metrics: MetricsConfig::from_native(&cfg.metrics),
            logs: LogsConfig::from_native(&cfg.logs),
            traces: TracesConfig::from_native(&cfg.traces),
        }
    }
}

/// Configuration for OTLP logs processing.
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub struct LogsConfig {
    /// Whether to enable OTLP logs support.
    ///
    /// Defaults to `true`.
    pub enabled: bool,
}

impl LogsConfig {
    fn from_native(cfg: &leaf::LogsConfig) -> Self {
        Self { enabled: cfg.enabled }
    }
}

impl Default for LogsConfig {
    fn default() -> Self {
        Self::from_native(&leaf::LogsConfig::default())
    }
}

/// Configuration for OTLP metrics processing.
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub struct MetricsConfig {
    /// Whether to enable OTLP metrics support.
    ///
    /// Defaults to `true`.
    pub enabled: bool,
}

impl MetricsConfig {
    fn from_native(cfg: &leaf::MetricsConfig) -> Self {
        Self { enabled: cfg.enabled }
    }
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self::from_native(&leaf::MetricsConfig::default())
    }
}

/// Configuration for OTLP traces processing.
///
/// Mirrors the leaf `otlp_config.traces` configuration.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub struct TracesConfig {
    /// Whether to enable OTLP traces support.
    ///
    /// Defaults to `true`.
    pub enabled: bool,

    /// Whether to skip deriving Datadog fields from standard OTLP attributes.
    ///
    /// When true, only uses explicit `datadog.*` prefixed attributes and skips
    /// fallback resolution from OTLP semantic conventions.
    ///
    /// Defaults to `false`.
    pub ignore_missing_datadog_fields: bool,

    /// When true, `_top_level` and `_dd.measured` are derived using the OTLP span kind.
    ///
    /// Defaults to `true`.
    pub enable_otlp_compute_top_level_by_span_kind: bool,

    /// Probabilistic sampler configuration for OTLP traces.
    pub probabilistic_sampler: ProbabilisticSampler,

    /// Total size of the string interner used for OTLP traces.
    ///
    /// Defaults to 512 KiB.
    pub string_interner_bytes: ByteSize,

    /// The internal port on the Core Agent to forward traces to.
    ///
    /// Defaults to 5003.
    #[allow(unused)]
    pub internal_port: u16,
}

impl TracesConfig {
    /// Builds the runtime traces configuration from its leaf mirror.
    pub fn from_native(cfg: &leaf::TracesConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            ignore_missing_datadog_fields: cfg.ignore_missing_datadog_fields,
            enable_otlp_compute_top_level_by_span_kind: cfg.enable_otlp_compute_top_level_by_span_kind,
            probabilistic_sampler: ProbabilisticSampler::from_native(&cfg.probabilistic_sampler),
            string_interner_bytes: cfg.string_interner_bytes,
            internal_port: cfg.internal_port,
        }
    }
}

/// Configuration for OTLP traces probabilistic sampling.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub struct ProbabilisticSampler {
    /// Percentage of traces to ingest (0, 100].
    ///
    /// Invalid values (<= 0 || > 100) are disregarded and the default is used.
    ///
    /// Defaults to 100.0 (100% sampling).
    pub sampling_percentage: f64,
}

impl ProbabilisticSampler {
    fn from_native(cfg: &leaf::ProbabilisticSampler) -> Self {
        Self {
            sampling_percentage: cfg.sampling_percentage,
        }
    }
}

impl Default for ProbabilisticSampler {
    fn default() -> Self {
        Self::from_native(&leaf::ProbabilisticSampler::default())
    }
}

impl Default for TracesConfig {
    fn default() -> Self {
        Self::from_native(&leaf::TracesConfig::default())
    }
}
