//! Component-native configuration for the Datadog forwarder and multi-region failover.
//!
//! Mirrors `DatadogForwarderConfiguration` / `ForwarderConfiguration` and `MrfConfiguration` in
//! `saluki-components`, with all source key names, aliases, and `Deserialize` impls stripped. The
//! retained `Option<GenericConfiguration>` and the parsed-from-string `TlsMinimumVersion` are
//! excluded as injected/derived state.

use std::path::PathBuf;

/// Configuration for the Datadog forwarder component.
///
/// Mirrors `ForwarderConfiguration` in `saluki-components`. The original
/// `DatadogForwarderConfiguration` wrapper retained an `Option<GenericConfiguration>` for dynamic
/// API-key refresh; that retained raw map is injected runtime state and is excluded here.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct DatadogForwarderConfig {
    /// Maximum number of concurrent requests for an individual endpoint.
    ///
    /// Defaults to 10. If set to `0`, request concurrency is clamped to 1.
    pub endpoint_concurrency: usize,

    /// Multiplier for endpoint request concurrency.
    ///
    /// Defaults to 1. Also sizes the HTTP idle connection pool. If set to `0`, idle connection
    /// retention is disabled and the multiplier is treated as 1.
    pub endpoint_concurrency_multiplier: usize,

    /// Request timeout, in seconds.
    ///
    /// Defaults to 20 seconds.
    pub request_timeout_secs: u64,

    /// Maximum number of pending requests for an individual endpoint.
    ///
    /// Defaults to 100.
    pub endpoint_buffer_size: usize,

    /// Endpoint configuration.
    pub endpoint: EndpointConfiguration,

    /// Retry configuration.
    pub retry: RetryConfiguration,

    /// Proxy configuration.
    pub proxy: Option<ProxyConfiguration>,

    /// Observability Pipelines Worker metrics routing configuration.
    pub opw_metrics: OpwMetricsConfiguration,

    /// HTTP protocol selection for outgoing forwarder requests.
    ///
    /// Defaults to [`ForwarderHttpProtocol::Auto`], which negotiates HTTP/2 with HTTP/1.1 fallback.
    pub http_protocol: ForwarderHttpProtocol,

    /// Connection reset interval, in seconds.
    ///
    /// Defaults to 0 (disabled).
    pub connection_reset_interval_secs: u64,

    /// Whether to disable TLS certificate validation for Datadog intake forwarding.
    ///
    /// Defaults to `false`. When `true`, HTTPS clients accept invalid server certificates.
    pub skip_ssl_validation: bool,

    /// File path to write TLS key material to for all HTTPS connections to the backend.
    ///
    /// When non-empty, enables logging of TLS key material in NSS key log format. Defaults to empty.
    pub sslkeylogfile: String,

    /// Minimum TLS protocol version for Datadog intake forwarding.
    ///
    /// Defaults to TLS 1.2. Older versions are accepted for compatibility but clamped to 1.2.
    pub min_tls_version: String,

    /// Whether to signal that the backend should allow arbitrary tag values.
    ///
    /// Defaults to `false`. When `true`, the forwarder adds `Allow-Arbitrary-Tag-Value: true` to
    /// every outbound intake request.
    pub allow_arbitrary_tags: bool,

    /// API key validation interval, in minutes.
    ///
    /// Values less than or equal to zero are ignored and the default is used. Defaults to 60.
    pub api_key_validation_interval_mins: i64,
}

impl Default for DatadogForwarderConfig {
    fn default() -> Self {
        Self {
            endpoint_concurrency: 10,
            endpoint_concurrency_multiplier: 1,
            request_timeout_secs: 20,
            endpoint_buffer_size: 100,
            endpoint: EndpointConfiguration::default(),
            retry: RetryConfiguration::default(),
            proxy: None,
            opw_metrics: OpwMetricsConfiguration::default(),
            http_protocol: ForwarderHttpProtocol::default(),
            connection_reset_interval_secs: 0,
            skip_ssl_validation: false,
            sslkeylogfile: String::new(),
            min_tls_version: "tls1.2".to_string(),
            allow_arbitrary_tags: false,
            api_key_validation_interval_mins: 60,
        }
    }
}

/// HTTP protocol selection for the Datadog forwarder.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
pub enum ForwarderHttpProtocol {
    /// Automatically negotiate HTTP/2 with HTTP/1.1 fallback.
    #[default]
    Auto,

    /// Use HTTP/1.1 only.
    Http1,
}

/// Endpoint configuration for the Datadog forwarder.
///
/// Mirrors `EndpointConfiguration` in `saluki-components`. The injected
/// `api_key_refresh_config_path` is excluded as runtime-injected state.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct EndpointConfiguration {
    /// The API key to use.
    pub api_key: String,

    /// The site to send data to.
    ///
    /// The base domain for the Datadog site the API key originates from (for example,
    /// `datadoghq.com`). Defaults to `datadoghq.com`.
    pub site: String,

    /// The full URL base to send data to.
    ///
    /// Takes precedence over `site` and is used verbatim. Defaults to unset.
    pub dd_url: Option<String>,

    /// Additional endpoints and API keys for dual shipping.
    ///
    /// Maps a destination URL to the API keys to ship to it. Defaults to empty.
    pub additional_endpoints: AdditionalEndpoints,
}

/// Additional endpoints for dual shipping, mapping a destination URL to one or more API keys.
///
/// Mirrors the resolved shape of `AdditionalEndpoints` in `saluki-components`.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct AdditionalEndpoints(pub std::collections::HashMap<String, Vec<String>>);

/// Retry configuration for the Datadog forwarder.
///
/// Mirrors `RetryConfiguration` in `saluki-components`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct RetryConfiguration {
    /// The minimum backoff factor used when retrying requests.
    ///
    /// Defaults to 2.
    pub backoff_factor: f64,

    /// The base growth rate of the backoff duration, in seconds.
    ///
    /// Defaults to 2 seconds.
    pub backoff_base: f64,

    /// The upper bound of the backoff duration, in seconds.
    ///
    /// Defaults to 64 seconds.
    pub backoff_max: f64,

    /// The amount to decrease the error count by on a successful request.
    ///
    /// Controls how quickly previous errors are forgotten. Defaults to 2.
    pub recovery_error_decrease_factor: u32,

    /// Whether a successful request completely resets the error count.
    ///
    /// Defaults to `false`.
    pub recovery_reset: bool,

    /// The maximum in-memory size of the retry queue, in bytes.
    ///
    /// Defaults to 15 MiB (unset means the default applies).
    pub retry_queue_payloads_max_size: Option<u64>,

    /// The maximum in-memory size of the retry queue, in bytes (deprecated).
    ///
    /// Defaults to 0.
    pub retry_queue_max_size: Option<u64>,

    /// The maximum size of the retry queue on disk, in bytes.
    ///
    /// Defaults to 0 (disabled).
    pub storage_max_size_bytes: u64,

    /// The ratio of in-memory retry queue bytes to flush to disk when the queue is full.
    ///
    /// Defaults to 0.5. If set to `0`, only enough old transactions are moved to disk to make room.
    pub flush_to_disk_mem_ratio: f64,

    /// The path to the on-disk retry queue directory.
    ///
    /// Defaults to `/opt/datadog-agent/run/transactions_to_retry`.
    pub storage_path: PathBuf,

    /// The maximum disk usage ratio for storing transactions on disk.
    ///
    /// Defaults to 0.80.
    pub storage_max_disk_ratio: f64,

    /// Maximum age, in days, for on-disk retry files before deletion at startup.
    ///
    /// Defaults to 10.
    pub outdated_file_in_days: u32,

    /// The time window used to estimate retry queue capacity, in seconds.
    ///
    /// Values below 10 seconds are clamped to 10. Defaults to 900 seconds.
    pub capacity_time_interval_secs: u64,
}

impl Default for RetryConfiguration {
    fn default() -> Self {
        Self {
            backoff_factor: 2.0,
            backoff_base: 2.0,
            backoff_max: 64.0,
            recovery_error_decrease_factor: 2,
            recovery_reset: false,
            retry_queue_payloads_max_size: None,
            retry_queue_max_size: None,
            storage_max_size_bytes: 0,
            flush_to_disk_mem_ratio: 0.5,
            storage_path: PathBuf::new(),
            storage_max_disk_ratio: 0.80,
            outdated_file_in_days: 10,
            capacity_time_interval_secs: 900,
        }
    }
}

/// Proxy configuration for the Datadog forwarder.
///
/// Mirrors `ProxyConfiguration` in `saluki-components`.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct ProxyConfiguration {
    /// The proxy server for HTTP requests.
    pub http_server: Option<String>,

    /// The proxy server for HTTPS requests.
    pub https_server: Option<String>,

    /// List of hosts that should bypass the proxy.
    pub no_proxy: Vec<String>,

    /// Whether `no_proxy` uses full domain/CIDR/wildcard matching.
    ///
    /// When `false` (the default), only exact host matches apply; suffix and CIDR entries are
    /// ignored.
    pub no_proxy_nonexact_match: bool,

    /// Whether proxy settings apply to cloud provider metadata endpoint requests.
    ///
    /// When `false` (the default), the well-known cloud metadata addresses bypass the proxy.
    pub use_proxy_for_cloud_metadata: bool,
}

/// Observability Pipelines Worker metrics routing configuration for the Datadog forwarder.
///
/// Mirrors `OpwMetricsConfiguration` in `saluki-components`.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct OpwMetricsConfiguration {
    /// Whether to route all metrics to Observability Pipelines Worker.
    ///
    /// Defaults to `false`.
    pub observability_pipelines_worker_enabled: bool,

    /// Endpoint of the Observability Pipelines Worker instance to route metrics to.
    ///
    /// Defaults to unset.
    pub observability_pipelines_worker_url: String,

    /// Whether to route all metrics to Vector (deprecated).
    ///
    /// Defaults to `false`.
    pub vector_enabled: bool,

    /// Endpoint of the Vector instance to route metrics to (deprecated).
    ///
    /// Defaults to unset.
    pub vector_url: String,
}

/// Configuration for multi-region failover.
///
/// Mirrors `MrfConfiguration` in `saluki-components`.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct MrfConfig {
    /// Whether multi-region failover is enabled.
    ///
    /// Defaults to `false`.
    pub enabled: bool,

    /// Whether metrics participate in failover.
    ///
    /// Defaults to `false`.
    pub failover_metrics: bool,

    /// Allowlist of metric names eligible for failover.
    ///
    /// Defaults to empty.
    pub metric_allowlist: Vec<String>,

    /// The API key for the failover region.
    ///
    /// Defaults to unset.
    pub api_key: Option<String>,

    /// The site for the failover region.
    ///
    /// Defaults to unset.
    pub site: Option<String>,

    /// The full URL base for the failover region.
    ///
    /// Defaults to unset.
    pub dd_url: Option<String>,
}
