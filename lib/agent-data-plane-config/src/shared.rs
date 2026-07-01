//! Cross-cutting values consumed by more than one domain.

use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;

/// Cross-cutting configuration shared across domains.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SharedConfiguration {
    /// Primary forwarder endpoints and transport.
    pub endpoints: Endpoints,

    /// Global and host-level tagging.
    pub tags: GlobalTags,

    /// Metrics-encoder settings reused across the metrics-emitting pipelines.
    pub metrics_encoding: MetricsEncoding,

    /// Cluster Agent connection, shared by checks, DogStatsD, and OTLP.
    pub cluster_agent: ClusterAgent,

    /// Autoscaling failover, shared by checks, DogStatsD, and OTLP.
    pub autoscaling_failover: AutoscalingFailover,

    /// Verbosity of the internal telemetry emitted about the runtime itself. (not in Datadog Agent
    /// config schema)
    pub metrics_level: String,
}

/// Primary outbound endpoints plus the forwarder, proxy, TLS, and compression settings that apply
/// to every pipeline emitting to the intake.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Endpoints {
    /// API key for the primary intake.
    pub api_key: String,

    // TODO(#1965): `site` and `dd_url` are carried raw rather than resolved into a primary
    // endpoint. Correct resolution has to know whether `dd_url` was actually set by the user or
    // just filled from the schema default, and that source signal is not available here yet.
    /// Base site domain for the primary intake, when set (for example, `datadoghq.com`).
    pub site: Option<String>,

    /// Full primary intake URL override, when set. Intended to take precedence over `site`.
    pub dd_url: Option<String>,

    /// Additional dual-shipping endpoints, keyed by intake URL with their API keys.
    pub additional_endpoints: HashMap<String, Vec<String>>,

    /// Whether metrics may carry arbitrary tags.
    pub allow_arbitrary_tags: bool,

    /// Outbound HTTP proxy settings.
    pub proxy: Proxy,

    /// Outbound TLS client settings.
    pub tls: Tls,

    /// Payload compression settings.
    pub compression: Compression,

    /// Forwarder retry, backoff, worker, and disk-storage settings.
    pub forwarder: Forwarder,

    /// Alternate metrics intake for the Observability Pipelines Worker, used in place of the
    /// default intake when enabled.
    pub opw_intake: AltMetricsIntake,

    /// Alternate metrics intake for Vector, used in place of the default intake when enabled.
    pub vector_intake: AltMetricsIntake,
}

/// An alternate metrics intake (Observability Pipelines Worker or Vector) that replaces the Datadog
/// intake when enabled.
#[derive(Clone, Debug, Default, Serialize)]
pub struct AltMetricsIntake {
    /// Whether this alternate intake replaces the default one.
    pub enabled: bool,

    /// URL of the alternate metrics intake.
    pub url: String,
}

/// Outbound HTTP proxy settings.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Proxy {
    /// Proxy URL for plain HTTP requests.
    pub http: String,

    /// Proxy URL for HTTPS requests.
    pub https: String,

    /// Hosts that bypass the proxy.
    pub no_proxy: Vec<String>,

    /// Whether no-proxy entries match by suffix rather than exact host.
    pub no_proxy_nonexact_match: bool,

    /// Whether cloud-metadata requests also go through the proxy.
    pub use_proxy_for_cloud_metadata: bool,
}

/// Outbound TLS client settings.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Tls {
    /// Whether server certificate validation is skipped.
    pub skip_ssl_validation: bool,

    /// Minimum TLS version enforced on outbound connections.
    pub min_tls_version: String,

    /// Path to which TLS session keys are logged, for debugging.
    pub sslkeylogfile: String,
}

/// Payload compression settings applied before transmission.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Compression {
    /// Which compression algorithm the encoder uses.
    pub compressor_kind: String,

    /// Compression level used when the algorithm is zstd.
    pub zstd_compressor_level: i32,
}

/// HTTP protocol the forwarder negotiates with the intake.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum ForwarderHttpProtocol {
    #[default]
    Auto,
    Http1,
}

/// Forwarder retry, backoff, worker, and disk-storage settings.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Forwarder {
    /// How often, in seconds, API keys are checked for validity against the intake.
    pub apikey_validation_interval: i64,

    /// Base delay, in seconds, for retry backoff.
    pub backoff_base: f64,

    /// Multiplier applied to the backoff delay after each failed attempt.
    pub backoff_factor: f64,

    /// Maximum retry backoff delay, in seconds.
    pub backoff_max: f64,

    /// How often, in seconds, idle connections are reset.
    pub connection_reset_interval: u64,

    /// Fraction of the in-memory retry queue at which payloads spill to disk.
    pub flush_to_disk_mem_ratio: f64,

    /// Capacity of the high-priority send buffer.
    pub high_prio_buffer_size: usize,

    /// HTTP protocol the forwarder negotiates with the intake.
    pub http_protocol: ForwarderHttpProtocol,

    /// Maximum number of in-flight requests to the intake.
    pub max_concurrent_requests: usize,

    /// Number of forwarder worker tasks.
    pub num_workers: usize,

    /// Age, in days, after which payloads queued on disk are discarded.
    pub outdated_file_in_days: u32,

    /// Number of retry cycles between attempts to recover a failed endpoint.
    pub recovery_interval: u32,

    /// Whether the recovery interval resets after a successful send.
    pub recovery_reset: bool,

    /// Retry-queue capacity expressed as seconds of buffered payloads.
    pub retry_queue_capacity_time_interval_sec: u64,

    /// Maximum number of payloads held in the in-memory retry queue.
    pub retry_queue_max_size: Option<u64>,

    /// Maximum total size, in bytes, of payloads held in the retry queue.
    pub retry_queue_payloads_max_size: Option<u64>,

    /// Grace period, in seconds, the forwarder is given to drain before shutdown.
    pub stop_timeout: u64,

    /// Fraction of available disk the on-disk retry store may use.
    pub storage_max_disk_ratio: f64,

    /// Maximum size, in bytes, of the on-disk retry store.
    pub storage_max_size_in_bytes: u64,

    /// Directory where retry payloads are persisted to disk.
    pub storage_path: PathBuf,

    /// Per-request timeout, in seconds, for calls to the intake.
    pub timeout: u64,
}

/// Global / host tagging.
#[derive(Clone, Debug, Default, Serialize)]
pub struct GlobalTags {
    /// How long, after startup, host tags remain attached to emitted data.
    pub expected_tags_duration: Duration,
}

/// Metrics-encoder settings reused across the metrics-emitting pipelines (DogStatsD, checks, and
/// OTLP): histogram settings, payload limits, and the encoder flush timeout.
#[derive(Clone, Debug, Default, Serialize)]
pub struct MetricsEncoding {
    /// How long the encoder waits before flushing a partially filled payload. (not in Datadog Agent
    /// config schema)
    pub flush_timeout: Duration,

    /// Maximum number of metrics packed into a single payload. (not in Datadog Agent config schema)
    pub max_metrics_per_payload: usize,

    /// Maximum compressed payload size, in bytes.
    pub max_payload_size: usize,

    /// Maximum compressed size, in bytes, of a series payload.
    pub max_series_payload_size: usize,

    /// Maximum number of series data points per payload.
    pub max_series_points_per_payload: usize,

    /// Maximum uncompressed size, in bytes, of a series payload.
    pub max_series_uncompressed_payload_size: usize,

    /// Maximum uncompressed payload size, in bytes.
    pub max_uncompressed_payload_size: usize,

    /// Whether series are submitted via the v2 intake API.
    pub use_v2_series_api: bool,

    /// Whether outgoing payloads are logged for debugging.
    pub log_payloads: bool,

    /// Histogram aggregation and encoding settings.
    pub histogram: HistogramEncoding,
}

/// Histogram aggregation/encoding settings, shared by the DogStatsD and checks metrics pipelines.
#[derive(Clone, Debug, Default, Serialize)]
pub struct HistogramEncoding {
    /// Which histogram aggregations (for example, `max` or `median`) are computed.
    pub aggregates: Vec<String>,

    /// Whether histograms are also emitted as distributions.
    pub copy_to_distribution: bool,

    /// Metric-name prefix applied to the distribution copies.
    pub copy_to_distribution_prefix: String,

    /// Which percentile aggregations are computed for histograms.
    pub percentiles: Vec<String>,
}

/// Cluster Agent connection, shared by checks, DogStatsD, and OTLP.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ClusterAgent {
    /// Whether the Cluster Agent connection is used.
    pub enabled: bool,

    /// URL of the Cluster Agent.
    pub url: Option<String>,

    /// Token used to authenticate to the Cluster Agent.
    pub auth_token: Option<String>,

    /// Kubernetes service name used to discover the Cluster Agent.
    pub kubernetes_service_name: Option<String>,
}

/// Autoscaling failover, shared by checks, DogStatsD, and OTLP.
#[derive(Clone, Debug, Default, Serialize)]
pub struct AutoscalingFailover {
    /// Whether autoscaling metrics failover is active.
    pub enabled: bool,

    /// Metrics designated for failover.
    pub metrics: Vec<String>,
}
