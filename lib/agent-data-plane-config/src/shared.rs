//! Cross-cutting values consumed by more than one domain.

use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use serde::Serialize;

use crate::defaults::{DEFAULT_ENCODER_FLUSH_TIMEOUT, DEFAULT_MAX_METRICS_PER_PAYLOAD};
use crate::{ConfigValue, Error};

/// Cross-cutting configuration shared across domains.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SharedConfiguration {
    /// Primary forwarder endpoints and transport.
    pub endpoints: Endpoints,

    /// Global and host-level tagging.
    pub tags: GlobalTags,

    /// Inputs used to derive deployment-wide static tags.
    pub static_tags: StaticTagSettings,

    /// Tags attached to basic liveness telemetry.
    pub basic_telemetry: BasicTelemetry,

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

/// Inputs used to derive deployment-wide static tags.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct StaticTagSettings {
    /// Deployment-provider classification added as `provider_kind:<value>` when non-empty.
    ///
    /// Defaults to empty, which adds no provider-kind tag.
    pub provider_kind: String,

    /// Whether the deployment uses EKS Fargate.
    ///
    /// Defaults to `false`. When enabled, the static-tag resolver adds EKS-specific tags in addition to global tags.
    pub eks_fargate: bool,

    /// Kubernetes node name used for the EKS Fargate node tag.
    ///
    /// Defaults to empty, which omits `eks_fargate_node` and emits a warning when EKS Fargate is enabled.
    pub kubernetes_kubelet_nodename: String,

    /// Kubernetes cluster name used for the EKS Fargate cluster tag.
    ///
    /// Defaults to empty, which omits `kube_cluster_name` unless the configured global tags already provide one.
    pub cluster_name: String,
}

/// Primary outbound endpoints plus the forwarder, proxy, TLS, and compression settings that apply
/// to every pipeline emitting to the intake.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Endpoints {
    /// API key for the primary intake.
    pub api_key: String,

    /// Base site domain for the primary intake (for example, `datadoghq.com`).
    ///
    /// The Datadog schema supplies a default value when nothing sets this key. Provenance is
    /// `Explicit` only when the value was explicitly configured.
    pub site: ConfigValue<String>,

    /// Full primary intake URL, which overrides [`site`](Self::site) when set explicitly.
    ///
    /// The Core Agent supplies this key at its schema default even when not set by the user or
    /// operator. Provenance is preserved so that we know when this was explicitly set and should
    /// override `site`.
    pub dd_url: ConfigValue<String>,

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

impl Endpoints {
    /// Returns the primary intake endpoint, as configured and without normalization.
    ///
    /// An explicitly configured [`dd_url`](Self::dd_url) overrides [`site`](Self::site), even when
    /// its value equals the schema default: the operator asked for that URL. Otherwise the endpoint
    /// is derived from `site`. An empty `site` cannot produce an endpoint, so the effective `dd_url`
    /// value is used instead; it already carries the source schema's default URL.
    pub fn primary_endpoint(&self) -> String {
        if self.dd_url.is_explicit() || self.site.value.is_empty() {
            self.dd_url.value.clone()
        } else {
            format!("https://app.{}", self.site.value)
        }
    }
}

/// An alternate metrics intake (Observability Pipelines Worker or Vector) that replaces the Datadog
/// intake when enabled.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct AltMetricsIntake {
    /// Whether this alternate intake replaces the default one.
    pub enabled: bool,

    /// URL of the alternate metrics intake.
    pub url: String,

    /// Whether metrics ship to this intake over the V3 series protocol
    /// (`observability_pipelines_worker.metrics.use_v3_api.series` / `vector.metrics.use_v3_api.series`).
    pub use_v3_series: bool,
}

/// Outbound HTTP proxy settings.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
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
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Tls {
    /// Whether server certificate validation is skipped.
    pub skip_ssl_validation: bool,

    /// Minimum TLS version enforced on outbound connections.
    pub min_tls_version: String,

    /// Path to which TLS session keys are logged, for debugging.
    pub sslkeylogfile: String,

    /// Timeout for completing the TLS handshake after a connection is established.
    ///
    /// Defaults to 10 seconds. Bounds only the handshake step, distinct from the overall request timeout. A value
    /// of zero disables the handshake-specific deadline, leaving the overall request timeout as the only bound.
    pub handshake_timeout: Duration,
}

/// Payload compression settings applied before transmission.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Compression {
    /// Which compression algorithm the encoder uses.
    pub compressor_kind: String,

    /// Compression level used when the algorithm is zstd, as the Core Agent configures it.
    ///
    /// Defaults to the Agent's own default of `1`. ADP does not use this value directly: an encoder
    /// resolves the effective level from this and [`zstd_compressor_level_override`], preferring the
    /// override and otherwise honoring this value only when it differs from the Agent default.
    ///
    /// [`zstd_compressor_level_override`]: Compression::zstd_compressor_level_override
    pub zstd_compressor_level: i32,

    /// ADP-specific zstd compression level, taking precedence over [`zstd_compressor_level`].
    ///
    /// Defaults to unset, in which case the encoder falls back to [`zstd_compressor_level`] (when
    /// changed from the Agent default) and otherwise to its own default of `3`. ADP compresses more
    /// cheaply than the Agent, so it can afford a higher level; operators rarely need to set this.
    ///
    /// [`zstd_compressor_level`]: Compression::zstd_compressor_level
    pub zstd_compressor_level_override: Option<i32>,
}

/// HTTP protocol the forwarder negotiates with the intake.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum ForwarderHttpProtocol {
    #[default]
    Auto,
    Http1,
}

/// Forwarder retry, backoff, worker, and disk-storage settings.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
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
    ///
    /// Deprecated in favor of [`retry_queue_payloads_max_size`](Self::retry_queue_payloads_max_size).
    /// The Datadog schema supplies `0` when nothing sets this key. Because `0` is also a value an
    /// operator can set, honor this setting only when it is explicit.
    pub retry_queue_max_size: ConfigValue<u64>,

    /// Maximum total size, in bytes, of payloads held in the retry queue.
    ///
    /// The Datadog schema supplies 15 MiB when nothing sets this key. Takes precedence over
    /// [`retry_queue_max_size`](Self::retry_queue_max_size) when set explicitly.
    pub retry_queue_payloads_max_size: ConfigValue<u64>,

    /// Grace period the forwarder is given to drain before shutdown.
    pub stop_timeout: Duration,

    /// Fraction of available disk the on-disk retry store may use.
    pub storage_max_disk_ratio: f64,

    /// Maximum size, in bytes, of the on-disk retry store.
    pub storage_max_size_in_bytes: u64,

    /// Directory where retry payloads are persisted to disk.
    pub storage_path: PathBuf,

    /// Per-request timeout, in seconds, for calls to the intake.
    pub timeout: u64,
}

impl Forwarder {
    /// Returns the effective maximum size, in bytes, of the in-memory retry queue.
    ///
    /// An explicit [`retry_queue_payloads_max_size`](Self::retry_queue_payloads_max_size) wins, then
    /// an explicit [`retry_queue_max_size`](Self::retry_queue_max_size), and otherwise the effective
    /// payload-size value, which carries the source schema's default. Selection cannot look at the
    /// values themselves: `0` is both the deprecated setting's schema default and a value an
    /// operator can mean.
    pub fn effective_retry_queue_max_size_bytes(&self) -> u64 {
        if !self.retry_queue_payloads_max_size.is_explicit() && self.retry_queue_max_size.is_explicit() {
            self.retry_queue_max_size.value
        } else {
            self.retry_queue_payloads_max_size.value
        }
    }
}

/// Global / host tagging.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct GlobalTags {
    /// Tags configured through `tags` / `DD_TAGS`.
    pub tags: Vec<String>,

    /// Tags configured through `extra_tags` / `DD_EXTRA_TAGS`.
    pub extra_tags: Vec<String>,

    /// How long, after startup, host tags remain attached to emitted data.
    pub expected_tags_duration: Duration,
}

/// Tagging options for basic liveness telemetry.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct BasicTelemetry {
    /// Whether liveness signals include the process container's low-cardinality tags.
    ///
    /// Defaults to `false`. Enable this for containerized deployments that need to associate basic
    /// telemetry with the running container. If the container cannot be resolved, liveness signals
    /// are emitted without these tags.
    pub add_container_tags: bool,
}

/// Metrics-encoder settings reused across the metrics-emitting pipelines (DogStatsD, checks, and
/// OTLP): histogram settings, payload limits, and the encoder flush timeout.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MetricsEncoding {
    /// How long the encoder waits before flushing a partially filled payload. (not in Datadog Agent
    /// config schema)
    ///
    /// Shared by the metrics-emitting pipelines and the traces encoder, all of which read the
    /// `flush_timeout_secs` key. Defaults to 2 seconds.
    pub flush_timeout: Duration,

    /// Maximum number of metrics packed into a single payload. (not in Datadog Agent config schema)
    ///
    /// Defaults to [`DEFAULT_MAX_METRICS_PER_PAYLOAD`].
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

    /// V3 metrics-intake protocol settings (`serializer_experimental_use_v3_api.*`).
    pub v3_api: V3ApiEncoding,

    /// Global V3 series routing mode (`use_v3_api.series.enabled`).
    pub v3_series_mode: V3SeriesMode,

    /// Per-endpoint V3 series routing overrides, keyed by endpoint URL
    /// (`use_v3_api.series.endpoints`).
    pub v3_series_endpoint_modes: HashMap<String, V3SeriesMode>,
}

impl Default for MetricsEncoding {
    fn default() -> Self {
        Self {
            // The `flush_timeout_secs` key is Saluki-only, so its default belongs to the ADP config
            // crate rather than a source schema.
            flush_timeout: DEFAULT_ENCODER_FLUSH_TIMEOUT,
            max_metrics_per_payload: DEFAULT_MAX_METRICS_PER_PAYLOAD,
            max_payload_size: 0,
            max_series_payload_size: 0,
            max_series_points_per_payload: 0,
            max_series_uncompressed_payload_size: 0,
            max_uncompressed_payload_size: 0,
            use_v2_series_api: false,
            log_payloads: false,
            histogram: HistogramEncoding::default(),
            v3_api: V3ApiEncoding::default(),
            v3_series_mode: V3SeriesMode::default(),
            v3_series_endpoint_modes: HashMap::new(),
        }
    }
}

/// V3 metrics-intake protocol settings for the series and sketches payloads
/// (`serializer_experimental_use_v3_api.*`).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct V3ApiEncoding {
    /// V3 series intake settings.
    pub series: V3ApiSettings,

    /// V3 sketches intake settings (the series-only fields stay at their defaults).
    pub sketches: V3ApiSettings,

    /// zstd compression level for V3 payloads.
    pub compression_level: i32,
}

/// Per-payload V3 intake settings, reused for both series and sketches.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct V3ApiSettings {
    /// Endpoints enabled for the V3 intake.
    pub endpoints: Vec<String>,
}

/// Whether series are routed to the V3 metrics intake (`use_v3_api.series.*`).
///
/// Each variant serializes to the spelling [`FromStr`] reads, so a serialized mode round-trips.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum V3SeriesMode {
    /// Route series to the V3 intake.
    #[serde(rename = "true")]
    Enabled,

    /// Route series to the older intake.
    #[serde(rename = "false")]
    Disabled,

    /// Route series to the V3 intake only for endpoints that are Datadog intake URLs.
    #[default]
    #[serde(rename = "datadog_only")]
    DatadogOnly,
}

impl FromStr for V3SeriesMode {
    type Err = Error;

    // The Agent reads this setting as a string and then interprets it, accepting more spellings than
    // `strconv.ParseBool` does, so the accepted set is wider than that of a `boolean` leaf.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "t" | "yes" | "on" => Ok(Self::Enabled),
            "false" | "0" | "f" | "no" | "off" | "" => Ok(Self::Disabled),
            "datadog_only" => Ok(Self::DatadogOnly),
            other => Err(Error::new_without_source(format!(
                "unknown V3 series mode `{other}`; expected a boolean or `datadog_only`"
            ))),
        }
    }
}

/// Histogram aggregation/encoding settings, shared by the DogStatsD and checks metrics pipelines.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
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
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
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
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct AutoscalingFailover {
    /// Whether autoscaling metrics failover is active.
    pub enabled: bool,

    /// Metrics designated for failover.
    pub metrics: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::{Endpoints, Forwarder, V3SeriesMode};
    use crate::ConfigValue;

    #[test]
    fn v3_series_mode_parses_every_form_the_agent_interprets() {
        for (value, expected) in [
            ("true", V3SeriesMode::Enabled),
            ("TRUE", V3SeriesMode::Enabled),
            ("1", V3SeriesMode::Enabled),
            ("t", V3SeriesMode::Enabled),
            ("yes", V3SeriesMode::Enabled),
            ("on", V3SeriesMode::Enabled),
            ("false", V3SeriesMode::Disabled),
            ("0", V3SeriesMode::Disabled),
            ("f", V3SeriesMode::Disabled),
            ("no", V3SeriesMode::Disabled),
            ("off", V3SeriesMode::Disabled),
            ("", V3SeriesMode::Disabled),
            (" datadog_only ", V3SeriesMode::DatadogOnly),
        ] {
            assert_eq!(
                value.parse::<V3SeriesMode>().expect("mode should parse"),
                expected,
                "{value}"
            );
        }
    }

    #[test]
    fn v3_series_mode_rejects_an_uninterpretable_value() {
        let error = "sometimes"
            .parse::<V3SeriesMode>()
            .expect_err("an uninterpretable mode should be rejected");

        assert_eq!(
            error.to_string(),
            "unknown V3 series mode `sometimes`; expected a boolean or `datadog_only`"
        );
    }

    #[test]
    fn v3_series_mode_defaults_to_datadog_only() {
        assert_eq!(V3SeriesMode::default(), V3SeriesMode::DatadogOnly);
    }

    #[test]
    fn explicit_dd_url_overrides_site_even_at_the_schema_default() {
        // The source supplies `dd_url` at its schema default even when nothing set it, so an
        // explicit URL equal to that default still expresses an override.
        let endpoints = Endpoints {
            site: ConfigValue::explicit("datadoghq.eu".to_string()),
            dd_url: ConfigValue::explicit("https://app.datadoghq.com".to_string()),
            ..Default::default()
        };

        assert_eq!("https://app.datadoghq.com", endpoints.primary_endpoint());
    }

    #[test]
    fn defaulted_dd_url_leaves_the_endpoint_to_site() {
        let endpoints = Endpoints {
            site: ConfigValue::explicit("datadoghq.eu".to_string()),
            dd_url: ConfigValue::defaulted("https://app.datadoghq.com".to_string()),
            ..Default::default()
        };

        assert_eq!("https://app.datadoghq.eu", endpoints.primary_endpoint());
    }

    #[test]
    fn an_override_url_is_used_verbatim() {
        let endpoints = Endpoints {
            site: ConfigValue::defaulted("datadoghq.com".to_string()),
            dd_url: ConfigValue::explicit("https://proxy.internal.example.com:3128".to_string()),
            ..Default::default()
        };

        assert_eq!("https://proxy.internal.example.com:3128", endpoints.primary_endpoint());
    }

    #[test]
    fn an_empty_site_falls_back_to_the_effective_dd_url() {
        // `https://app.` is not an endpoint, and the model does not restate the source schema's
        // default site. The effective `dd_url` already carries the schema default URL.
        let endpoints = Endpoints {
            site: ConfigValue::explicit(String::new()),
            dd_url: ConfigValue::defaulted("https://app.datadoghq.com".to_string()),
            ..Default::default()
        };

        assert_eq!("https://app.datadoghq.com", endpoints.primary_endpoint());
    }

    #[test]
    fn retry_queue_size_prefers_the_explicit_payload_size() {
        let forwarder = Forwarder {
            retry_queue_payloads_max_size: ConfigValue::explicit(2048),
            retry_queue_max_size: ConfigValue::explicit(1024),
            ..Default::default()
        };

        assert_eq!(2048, forwarder.effective_retry_queue_max_size_bytes());
    }

    #[test]
    fn retry_queue_size_falls_back_to_the_explicit_deprecated_size() {
        let forwarder = Forwarder {
            retry_queue_payloads_max_size: ConfigValue::defaulted(15 * 1024 * 1024),
            retry_queue_max_size: ConfigValue::explicit(1024),
            ..Default::default()
        };

        assert_eq!(1024, forwarder.effective_retry_queue_max_size_bytes());
    }

    #[test]
    fn an_explicit_zero_retry_queue_size_is_honored() {
        // Zero is the deprecated setting's schema default and also a value an operator can mean, so
        // only provenance can tell the two apart.
        let explicitly_zero = Forwarder {
            retry_queue_payloads_max_size: ConfigValue::defaulted(15 * 1024 * 1024),
            retry_queue_max_size: ConfigValue::explicit(0),
            ..Default::default()
        };
        assert_eq!(0, explicitly_zero.effective_retry_queue_max_size_bytes());

        let defaulted_zero = Forwarder {
            retry_queue_payloads_max_size: ConfigValue::defaulted(15 * 1024 * 1024),
            retry_queue_max_size: ConfigValue::defaulted(0),
            ..Default::default()
        };
        assert_eq!(15 * 1024 * 1024, defaulted_zero.effective_retry_queue_max_size_bytes());
    }
}
