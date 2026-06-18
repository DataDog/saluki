//! Leaf crate for component-native configuration structs and dynamic config handles.
//!
//! Component config types in this crate are resolved, source-agnostic runtime inputs. Source-language
//! parsing, aliases, remapping, and update routing live above this crate.

pub mod dynamic;

use std::collections::BTreeMap;

pub use dynamic::ScopedConfig;
use serde::{de::Deserializer, Deserialize, Serialize};

/// Network listen address used by component-native configuration.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub enum ListenAddress {
    /// The listener is disabled.
    #[default]
    Disabled,
    /// The listener binds a TCP socket.
    Tcp(String),
    /// The listener binds a UDP socket.
    Udp(String),
    /// The listener binds a Unix-domain socket.
    Unix(String),
}

/// Common endpoint configuration for Datadog-style HTTP forwarders.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EndpointConfig {
    /// Intake endpoint URL.
    pub url: String,
    /// API key used for the endpoint.
    #[serde(skip_serializing)]
    pub api_key: String,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            url: "https://app.datadoghq.com".to_string(),
            api_key: String::new(),
        }
    }
}

/// Common retry queue settings.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RetryConfig {
    /// Directory used by disk-backed retry storage.
    pub storage_path: String,
    /// Maximum disk space for retry storage, in bytes.
    pub max_disk_size_bytes: u64,
    /// In-memory flush-to-disk ratio.
    pub flush_to_disk_mem_ratio: f64,
    /// Minimum retry backoff jitter factor.
    pub backoff_factor: f64,
    /// Base retry backoff, in seconds.
    pub backoff_base_secs: f64,
    /// Maximum retry backoff, in seconds.
    pub backoff_max_secs: f64,
    /// Error-count decrease factor after successful requests.
    pub recovery_interval: u32,
    /// Whether successful requests reset retry error state.
    pub recovery_reset: bool,
    /// Preferred in-memory retry queue size, in bytes.
    pub retry_queue_payloads_max_size_bytes: Option<u64>,
    /// Deprecated in-memory retry queue size, in bytes.
    pub retry_queue_max_size_bytes: Option<u64>,
    /// Maximum disk usage ratio for retry storage.
    pub storage_max_disk_ratio: f64,
    /// Maximum retry file age before startup cleanup, in days.
    pub outdated_file_in_days: u32,
    /// Retry queue capacity estimation window, in seconds.
    pub capacity_time_interval_secs: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            storage_path: String::new(),
            max_disk_size_bytes: 0,
            flush_to_disk_mem_ratio: 0.5,
            backoff_factor: 2.0,
            backoff_base_secs: 2.0,
            backoff_max_secs: 64.0,
            recovery_interval: 2,
            recovery_reset: false,
            retry_queue_payloads_max_size_bytes: None,
            retry_queue_max_size_bytes: None,
            storage_max_disk_ratio: 0.8,
            outdated_file_in_days: 10,
            capacity_time_interval_secs: 15 * 60,
        }
    }
}

/// Datadog forwarder proxy settings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct ProxyConfig {
    /// HTTP proxy URL.
    pub http: Option<String>,
    /// HTTPS proxy URL.
    pub https: Option<String>,
    /// Proxy bypass entries.
    pub no_proxy: Vec<String>,
    /// Whether proxy bypass entries use non-exact matching.
    pub no_proxy_nonexact_match: bool,
    /// Whether cloud metadata endpoints use the proxy.
    pub use_proxy_for_cloud_metadata: bool,
}

/// OPW or Vector metrics forwarding override.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct OpwMetricsConfig {
    /// Whether OPW metrics routing is enabled.
    pub opw_enabled: bool,
    /// OPW metrics endpoint URL.
    pub opw_url: String,
    /// Whether Vector metrics routing is enabled.
    pub vector_enabled: bool,
    /// Vector metrics endpoint URL.
    pub vector_url: String,
}

/// Common TLS client settings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TlsClientConfig {
    /// Whether TLS verification is enabled.
    pub verify: bool,
}

impl Default for TlsClientConfig {
    fn default() -> Self {
        Self { verify: true }
    }
}

/// Datadog forwarder runtime configuration.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DatadogForwarderConfig {
    /// Primary and additional intake endpoints.
    pub endpoints: Vec<EndpointConfig>,
    /// Number of concurrent requests per endpoint.
    pub endpoint_concurrency: usize,
    /// Multiplier for endpoint request concurrency.
    pub endpoint_concurrency_multiplier: usize,
    /// Request timeout in milliseconds.
    pub request_timeout_millis: u64,
    /// Maximum pending requests per endpoint.
    pub endpoint_buffer_size: usize,
    /// HTTP protocol selection.
    pub http_protocol: String,
    /// Connection reset interval, in seconds.
    pub connection_reset_interval_secs: u64,
    /// TLS key log file path.
    pub ssl_key_log_file: String,
    /// Minimum TLS version string.
    pub min_tls_version: String,
    /// API key validation interval, in minutes.
    pub api_key_validation_interval_mins: i64,
    /// Whether arbitrary tags are preserved.
    pub allow_arbitrary_tags: bool,
    /// TLS client settings.
    pub tls: TlsClientConfig,
    /// Retry queue settings.
    pub retry: RetryConfig,
    /// Proxy settings.
    pub proxy: ProxyConfig,
    /// OPW or Vector metrics routing settings.
    pub opw_metrics: OpwMetricsConfig,
}

impl Default for DatadogForwarderConfig {
    fn default() -> Self {
        Self {
            endpoints: vec![EndpointConfig::default()],
            endpoint_concurrency: 10,
            endpoint_concurrency_multiplier: 1,
            request_timeout_millis: 20_000,
            endpoint_buffer_size: 100,
            http_protocol: "auto".to_string(),
            connection_reset_interval_secs: 0,
            ssl_key_log_file: String::new(),
            min_tls_version: "tlsv1.2".to_string(),
            api_key_validation_interval_mins: 60,
            allow_arbitrary_tags: false,
            tls: TlsClientConfig::default(),
            retry: RetryConfig::default(),
            proxy: ProxyConfig::default(),
            opw_metrics: OpwMetricsConfig::default(),
        }
    }
}

/// Datadog metrics encoder runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DatadogMetricsEncoderConfig {
    /// Maximum input metrics per payload.
    pub max_metrics_per_payload: usize,
    /// Maximum compressed generic payload size.
    pub max_payload_size: usize,
    /// Maximum uncompressed generic payload size.
    pub max_uncompressed_payload_size: usize,
    /// Maximum compressed series payload size.
    pub max_series_payload_size: usize,
    /// Maximum uncompressed series payload size.
    pub max_series_uncompressed_payload_size: usize,
    /// Maximum series points per payload.
    pub max_series_points_per_payload: usize,
    /// Flush timeout, in seconds.
    pub flush_timeout_secs: u64,
    /// Compression algorithm name.
    pub compressor_kind: String,
    /// Zstd compression level.
    pub compression_level: i32,
    /// Whether series uses the V2 API.
    pub use_v2_api_series: bool,
    /// Whether metric payloads are logged.
    pub log_payloads: bool,
}

impl Default for DatadogMetricsEncoderConfig {
    fn default() -> Self {
        Self {
            max_metrics_per_payload: 10_000,
            max_payload_size: 2_621_440,
            max_uncompressed_payload_size: 4_194_304,
            max_series_payload_size: 512_000,
            max_series_uncompressed_payload_size: 5_242_880,
            max_series_points_per_payload: 10_000,
            flush_timeout_secs: 2,
            compressor_kind: "zstd".to_string(),
            compression_level: 3,
            use_v2_api_series: true,
            log_payloads: false,
        }
    }
}

/// Datadog logs encoder runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct DatadogLogsEncoderConfig {
    /// Whether logs encoding is enabled.
    pub enabled: bool,
}

/// Datadog events encoder runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct DatadogEventsEncoderConfig {
    /// Whether events encoding is enabled.
    pub enabled: bool,
}

/// Datadog service-check encoder runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct DatadogServiceChecksEncoderConfig {
    /// Whether service-check encoding is enabled.
    pub enabled: bool,
}

/// APM stats encoder runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct ApmStatsEncoderConfig {
    /// Whether stats encoding is enabled.
    pub enabled: bool,
}

/// APM stats transform runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct ApmStatsTransformConfig {
    /// Whether APM stats computation is enabled.
    pub enabled: bool,
}

/// Multi-region failover runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct MrfConfig {
    /// Whether failover is enabled.
    pub enabled: bool,
    /// Whether metrics should be sent to the failover branch.
    pub failover_metrics: bool,
    /// Secondary endpoint URL.
    pub endpoint: Option<String>,
    /// API key for the secondary endpoint.
    #[serde(skip_serializing)]
    pub api_key: Option<String>,
    /// Metric allowlist used by the failover branch.
    pub metric_allowlist: Vec<String>,
}

/// Checks IPC source runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct ChecksIpcConfig {
    /// Named pipe or Unix socket path.
    pub endpoint: String,
}

/// DogStatsD source runtime configuration.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DogStatsDConfig {
    /// UDP listen address.
    pub udp_address: ListenAddress,
    /// TCP listen address.
    pub tcp_address: ListenAddress,
    /// Unix datagram socket path.
    pub socket_path: Option<String>,
    /// Unix stream socket path.
    pub socket_stream_path: Option<String>,
    /// Packet buffer size in bytes.
    pub buffer_size: usize,
    /// Number of packet buffers.
    pub buffer_count: usize,
    /// Socket receive buffer size in bytes.
    pub socket_receive_buffer_size: usize,
    /// Whether stream frame oversize warnings are logged.
    pub stream_log_too_big: bool,
    /// Listener types requiring newline termination.
    pub eol_required: Vec<String>,
    /// Whether non-local UDP traffic is accepted.
    pub non_local_traffic: bool,
    /// Whether UDP listeners use SO_REUSEPORT autoscaling.
    pub autoscale_udp_listeners: bool,
    /// Whether context resolution may allocate on the heap.
    pub allow_context_heap_allocations: bool,
    /// Whether no-aggregation pipeline hints are parsed.
    pub no_aggregation_pipeline_support: bool,
    /// Core Agent string interner entry count.
    pub context_string_interner_entry_count: u64,
    /// Context interner size in bytes.
    pub context_string_interner_size_bytes: u64,
    /// Cached context limit.
    pub cached_contexts_limit: usize,
    /// Cached tagset limit.
    pub cached_tagsets_limit: usize,
    /// Context cache expiry, in seconds.
    pub context_expiry_seconds: u64,
    /// Whether permissive DogStatsD decoding is enabled.
    pub permissive_decoding: bool,
    /// Minimum accepted sample rate.
    pub minimum_sample_rate: f64,
    /// Payload type gates.
    pub enable_payloads: DogStatsDEnablePayloadsConfig,
    /// Origin enrichment settings.
    pub origin: DogStatsDOriginConfig,
    /// Extra tags added to all received payloads.
    pub additional_tags: Vec<String>,
    /// Default capture output path.
    pub capture_path: String,
    /// In-process capture queue depth.
    pub capture_depth: usize,
    /// Provider kind tag value.
    pub provider_kind: String,
    /// UDP packet forwarding host.
    pub statsd_forward_host: Option<String>,
    /// UDP packet forwarding port.
    pub statsd_forward_port: u16,
}

impl Default for DogStatsDConfig {
    fn default() -> Self {
        Self {
            udp_address: ListenAddress::Udp("127.0.0.1:8125".to_string()),
            tcp_address: ListenAddress::Disabled,
            socket_path: None,
            socket_stream_path: None,
            buffer_size: 8192,
            buffer_count: 128,
            socket_receive_buffer_size: 0,
            stream_log_too_big: false,
            eol_required: Vec::new(),
            non_local_traffic: false,
            autoscale_udp_listeners: false,
            allow_context_heap_allocations: true,
            no_aggregation_pipeline_support: true,
            context_string_interner_entry_count: 4096,
            context_string_interner_size_bytes: 2 * 1024 * 1024,
            cached_contexts_limit: 500_000,
            cached_tagsets_limit: 500_000,
            context_expiry_seconds: 20,
            permissive_decoding: true,
            minimum_sample_rate: 0.000000003845,
            enable_payloads: DogStatsDEnablePayloadsConfig::default(),
            origin: DogStatsDOriginConfig::default(),
            additional_tags: Vec::new(),
            capture_path: String::new(),
            capture_depth: 1024,
            provider_kind: String::new(),
            statsd_forward_host: None,
            statsd_forward_port: 0,
        }
    }
}

/// DogStatsD payload type gates.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DogStatsDEnablePayloadsConfig {
    /// Whether series payloads are accepted.
    pub series: bool,
    /// Whether sketch payloads are accepted.
    pub sketches: bool,
    /// Whether event payloads are accepted.
    pub events: bool,
    /// Whether service-check payloads are accepted.
    pub service_checks: bool,
}

impl Default for DogStatsDEnablePayloadsConfig {
    fn default() -> Self {
        Self {
            series: true,
            sketches: true,
            events: true,
            service_checks: true,
        }
    }
}

/// DogStatsD origin enrichment settings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DogStatsDOriginConfig {
    /// Whether origin enrichment is enabled.
    pub enabled: bool,
    /// Whether client entity IDs take precedence.
    pub entity_id_precedence: bool,
    /// Default origin tag cardinality.
    pub tag_cardinality: String,
    /// Whether unified origin detection is used.
    pub unified_detection: bool,
    /// Whether DogStatsD origin opt-out is enabled.
    pub optout_enabled: bool,
    /// Whether client-provided origin fields are parsed.
    pub client_detection: bool,
}

impl Default for DogStatsDOriginConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            entity_id_precedence: false,
            tag_cardinality: "low".to_string(),
            unified_detection: false,
            optout_enabled: true,
            client_detection: false,
        }
    }
}

/// Action applied by a per-metric tag filter entry.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub enum TagFilterAction {
    /// Keep only the configured tags.
    Include,
    /// Remove the configured tags.
    #[default]
    Exclude,
}

impl<'de> Deserialize<'de> for TagFilterAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Option::<String>::deserialize(deserializer)?.as_deref() {
            Some("include") => Ok(Self::Include),
            Some("exclude") | None | Some("") => Ok(Self::Exclude),
            Some(_) => Ok(Self::Exclude),
        }
    }
}

/// One per-metric tag filter rule.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MetricTagFilterEntry {
    /// Exact metric name this rule applies to.
    pub metric_name: String,
    /// Whether the rule keeps or removes listed tags.
    #[serde(default)]
    pub action: TagFilterAction,
    /// Tag key names controlled by this rule.
    pub tags: Vec<String>,
}

/// DogStatsD prefix and blocklist filter runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DogStatsDPrefixFilterConfig {
    /// Metric namespace prepended to outgoing metric names.
    pub metric_prefix: String,
    /// Metric prefixes skipped when applying the namespace.
    pub metric_prefix_blocklist: Vec<String>,
    /// Current Agent metric filterlist values.
    pub metric_filterlist: Vec<String>,
    /// Whether current filterlist values match by prefix.
    pub metric_filterlist_match_prefix: bool,
    /// Legacy DogStatsD metric blocklist values.
    pub metric_blocklist: Vec<String>,
    /// Whether legacy blocklist values match by prefix.
    pub metric_blocklist_match_prefix: bool,
}

impl Default for DogStatsDPrefixFilterConfig {
    fn default() -> Self {
        Self {
            metric_prefix: String::new(),
            metric_prefix_blocklist: default_metric_prefix_blocklist(),
            metric_filterlist: Vec::new(),
            metric_filterlist_match_prefix: false,
            metric_blocklist: Vec::new(),
            metric_blocklist_match_prefix: false,
        }
    }
}

/// DogStatsD post-aggregate filter runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DogStatsDPostAggregateFilterConfig {
    /// Current Agent metric filterlist values.
    pub metric_filterlist: Vec<String>,
    /// Whether current filterlist values match by prefix.
    pub metric_filterlist_match_prefix: bool,
    /// Legacy DogStatsD metric blocklist values.
    pub metric_blocklist: Vec<String>,
    /// Whether legacy blocklist values match by prefix.
    pub metric_blocklist_match_prefix: bool,
    /// Histogram aggregate suffixes produced by aggregation.
    pub histogram_aggregates: Vec<String>,
    /// Histogram percentile suffixes produced by aggregation.
    pub histogram_percentiles: Vec<String>,
}

impl Default for DogStatsDPostAggregateFilterConfig {
    fn default() -> Self {
        Self {
            metric_filterlist: Vec::new(),
            metric_filterlist_match_prefix: false,
            metric_blocklist: Vec::new(),
            metric_blocklist_match_prefix: false,
            histogram_aggregates: ["max", "median", "avg", "count"]
                .into_iter()
                .map(String::from)
                .collect(),
            histogram_percentiles: ["0.95"].into_iter().map(String::from).collect(),
        }
    }
}

fn default_metric_prefix_blocklist() -> Vec<String> {
    [
        "datadog.agent",
        "datadog.dogstatsd",
        "datadog.process",
        "datadog.trace_agent",
        "datadog.tracer",
        "activemq",
        "activemq_58",
        "airflow",
        "cassandra",
        "confluent",
        "hazelcast",
        "hive",
        "ignite",
        "jboss",
        "jvm",
        "kafka",
        "presto",
        "sidekiq",
        "solr",
        "tomcat",
        "runtime",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// DogStatsD mapper runtime configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Default)]
pub struct DogStatsDMapperConfig {
    /// Mapping profiles in source-neutral JSON form until the mapper owns a typed model.
    pub profiles: Vec<serde_json::Value>,
    /// Mapping cache size.
    pub cache_size: usize,
}

/// Metric tag filterlist runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TagFilterlistConfig {
    /// Per-metric tag filter entries.
    pub entries: Vec<MetricTagFilterEntry>,
    /// Cache capacity for filter decisions.
    pub cache_capacity: usize,
}

impl Default for TagFilterlistConfig {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            cache_capacity: 100_000,
        }
    }
}

/// Aggregate transform runtime configuration.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AggregateConfig {
    /// Flush interval in milliseconds.
    pub flush_interval_millis: u64,
    /// Whether open buckets are flushed on shutdown.
    pub flush_open_windows: bool,
    /// Whether timestamped metrics bypass aggregation.
    pub passthrough_timestamped_metrics: bool,
    /// Whether histograms are copied to distributions.
    pub histogram_copy_to_distribution: bool,
    /// Prefix for copied histogram distribution names.
    pub histogram_copy_to_distribution_prefix: String,
}

impl Default for AggregateConfig {
    fn default() -> Self {
        Self {
            flush_interval_millis: 10_000,
            flush_open_windows: false,
            passthrough_timestamped_metrics: true,
            histogram_copy_to_distribution: false,
            histogram_copy_to_distribution_prefix: String::new(),
        }
    }
}

/// DogStatsD debug log runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DogStatsDDebugLogConfig {
    /// Whether metric-level stats should be written.
    pub metrics_stats_enabled: bool,
    /// Whether the debug log destination should be built.
    pub logging_enabled: bool,
    /// Output file path.
    pub log_file: String,
    /// Maximum active log file size before rotation, in bytes.
    pub log_file_max_size_bytes: u64,
    /// Number of rotated log files to keep.
    pub log_file_max_rolls: usize,
}

impl Default for DogStatsDDebugLogConfig {
    fn default() -> Self {
        Self {
            metrics_stats_enabled: false,
            logging_enabled: true,
            log_file: String::new(),
            log_file_max_size_bytes: 10 * 1024 * 1024,
            log_file_max_rolls: 3,
        }
    }
}

/// DogStatsD statistics destination runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct DogStatsDStatisticsConfig;

/// Trace encoder runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct DatadogTraceEncoderConfig {
    /// Whether trace encoding is enabled.
    pub enabled: bool,
}

/// Trace sampler runtime configuration.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TraceSamplerConfig {
    /// Global sampling rate.
    pub rate: f64,
}

impl Default for TraceSamplerConfig {
    fn default() -> Self {
        Self { rate: 1.0 }
    }
}

/// Trace obfuscation runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct TraceObfuscationConfig {
    /// Credit card obfuscation settings.
    pub credit_cards: CreditCardObfuscationConfig,
    /// HTTP obfuscation settings.
    pub http: HttpObfuscationConfig,
    /// Memcached obfuscation settings.
    pub memcached: MemcachedObfuscationConfig,
    /// Redis obfuscation settings.
    pub redis: RedisObfuscationConfig,
    /// Valkey obfuscation settings.
    pub valkey: ValkeyObfuscationConfig,
    /// Elasticsearch obfuscation settings.
    pub elasticsearch: JsonObfuscationConfig,
    /// MongoDB obfuscation settings.
    pub mongodb: JsonObfuscationConfig,
    /// OpenSearch obfuscation settings.
    pub opensearch: JsonObfuscationConfig,
}

/// Credit card trace obfuscation settings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct CreditCardObfuscationConfig {
    /// Whether credit card obfuscation is enabled.
    pub enabled: bool,
    /// Tag keys that are kept unchanged.
    pub keep_values: Vec<String>,
    /// Whether Luhn validation is used.
    pub luhn: bool,
}

/// HTTP trace obfuscation settings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct HttpObfuscationConfig {
    /// Whether HTTP path segments containing digits are removed.
    pub remove_paths_with_digits: bool,
    /// Whether HTTP query strings are removed.
    pub remove_query_string: bool,
}

/// Memcached trace obfuscation settings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct MemcachedObfuscationConfig {
    /// Whether memcached obfuscation is enabled.
    pub enabled: bool,
    /// Whether the memcached command name is kept.
    pub keep_command: bool,
}

/// Redis trace obfuscation settings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct RedisObfuscationConfig {
    /// Whether Redis obfuscation is enabled.
    pub enabled: bool,
    /// Whether all Redis command arguments are removed.
    pub remove_all_args: bool,
}

/// Valkey trace obfuscation settings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct ValkeyObfuscationConfig {
    /// Whether Valkey obfuscation is enabled.
    pub enabled: bool,
    /// Whether all Valkey command arguments are removed.
    pub remove_all_args: bool,
}

/// JSON trace obfuscation settings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct JsonObfuscationConfig {
    /// Whether JSON obfuscation is enabled.
    pub enabled: bool,
    /// JSON keys whose values are kept unchanged.
    pub keep_values: Vec<String>,
    /// JSON keys whose values are SQL-obfuscated.
    pub obfuscate_sql_values: Vec<String>,
}

/// OTTL filter runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct OttlFilterConfig {
    /// Filter statements by signal name.
    pub statements: BTreeMap<String, Vec<String>>,
}

/// OTTL transform runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct OttlTransformConfig {
    /// Transform statements by signal name.
    pub statements: BTreeMap<String, Vec<String>>,
}

/// OTLP source runtime configuration.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OtlpConfig {
    /// gRPC listen endpoint.
    pub grpc_endpoint: String,
    /// gRPC transport name.
    pub grpc_transport: String,
    /// Maximum gRPC receive message size, in MiB.
    pub grpc_max_recv_msg_size_mib: u64,
    /// HTTP listen endpoint.
    pub http_endpoint: String,
    /// HTTP transport name.
    pub http_transport: String,
    /// Whether OTLP metrics are accepted.
    pub metrics_enabled: bool,
    /// Whether OTLP logs are accepted.
    pub logs_enabled: bool,
    /// Whether OTLP traces are accepted.
    pub traces_enabled: bool,
    /// Core Agent traces internal port.
    pub traces_internal_port: u16,
    /// OTLP traces sampling percentage.
    pub traces_sampling_percentage: f64,
    /// String interner size.
    pub string_interner_size: usize,
    /// Cached contexts limit.
    pub cached_contexts_limit: usize,
}

impl Default for OtlpConfig {
    fn default() -> Self {
        Self {
            grpc_endpoint: "127.0.0.1:4317".to_string(),
            grpc_transport: "tcp".to_string(),
            grpc_max_recv_msg_size_mib: 4,
            http_endpoint: "127.0.0.1:4318".to_string(),
            http_transport: "tcp".to_string(),
            metrics_enabled: true,
            logs_enabled: true,
            traces_enabled: true,
            traces_internal_port: 5003,
            traces_sampling_percentage: 100.0,
            string_interner_size: 32_768,
            cached_contexts_limit: 500_000,
        }
    }
}

/// OTLP relay runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct OtlpRelayConfig {
    /// gRPC relay endpoint.
    pub grpc_endpoint: String,
}

/// OTLP decoder runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct OtlpDecoderConfig {
    /// Whether strict decoding is enabled.
    pub strict: bool,
}

/// OTLP forwarder runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct OtlpForwarderConfig {
    /// Core-agent OTLP endpoint.
    pub endpoint: String,
}

/// Host tag enrichment runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct HostTagsConfig {
    /// Whether host tags are requested.
    pub enabled: bool,
}

/// Host enrichment runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct HostEnrichmentConfig;

/// Workload metadata runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct WorkloadConfig {
    /// Whether workload metadata collection is enabled.
    pub enabled: bool,
    /// CRI connection timeout, in seconds.
    pub cri_connection_timeout_secs: u64,
    /// CRI query timeout, in seconds.
    pub cri_query_timeout_secs: u64,
}
