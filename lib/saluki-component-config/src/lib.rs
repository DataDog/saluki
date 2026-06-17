//! Leaf crate for component-native configuration structs and dynamic config handles.
//!
//! Component config types in this crate are resolved, source-agnostic runtime inputs. Source-language
//! parsing, aliases, remapping, and update routing live above this crate.

pub mod dynamic;

use std::collections::BTreeMap;

pub use dynamic::ScopedConfig;
use serde::{de::Deserializer, Deserialize, Serialize};

/// Network listen address used by component-native configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum ListenAddress {
    /// The listener is disabled.
    Disabled,
    /// The listener binds a TCP socket.
    Tcp(String),
    /// The listener binds a UDP socket.
    Udp(String),
    /// The listener binds a Unix-domain socket.
    Unix(String),
}

impl Default for ListenAddress {
    fn default() -> Self {
        Self::Disabled
    }
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
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            storage_path: String::new(),
            max_disk_size_bytes: 0,
            flush_to_disk_mem_ratio: 0.5,
        }
    }
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
    /// Request timeout in milliseconds.
    pub request_timeout_millis: u64,
    /// Whether arbitrary tags are preserved.
    pub allow_arbitrary_tags: bool,
    /// TLS client settings.
    pub tls: TlsClientConfig,
    /// Retry queue settings.
    pub retry: RetryConfig,
}

impl Default for DatadogForwarderConfig {
    fn default() -> Self {
        Self {
            endpoints: vec![EndpointConfig::default()],
            endpoint_concurrency: 1,
            request_timeout_millis: 30_000,
            allow_arbitrary_tags: false,
            tls: TlsClientConfig::default(),
            retry: RetryConfig::default(),
        }
    }
}

/// Datadog metrics encoder runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DatadogMetricsEncoderConfig {
    /// Compression level for outgoing payloads.
    pub compression_level: i32,
}

impl Default for DatadogMetricsEncoderConfig {
    fn default() -> Self {
        Self { compression_level: 6 }
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
    /// Packet buffer size in bytes.
    pub buffer_size: usize,
    /// Context interner size in bytes.
    pub context_string_interner_size_bytes: u64,
    /// Cached context limit.
    pub cached_contexts_limit: usize,
    /// Extra tags added to all received payloads.
    pub additional_tags: Vec<String>,
}

impl Default for DogStatsDConfig {
    fn default() -> Self {
        Self {
            udp_address: ListenAddress::Udp("127.0.0.1:8125".to_string()),
            tcp_address: ListenAddress::Disabled,
            socket_path: None,
            buffer_size: 8192,
            context_string_interner_size_bytes: 2 * 1024 * 1024,
            cached_contexts_limit: 500_000,
            additional_tags: Vec::new(),
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
}

impl Default for AggregateConfig {
    fn default() -> Self {
        Self {
            flush_interval_millis: 10_000,
        }
    }
}

/// DogStatsD debug log runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct DogStatsDDebugLogConfig {
    /// Whether debug logging is enabled.
    pub enabled: bool,
    /// Output file path.
    pub path: String,
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
    /// Whether obfuscation is enabled.
    pub enabled: bool,
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
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OtlpConfig {
    /// GRPC listen endpoint.
    pub grpc_endpoint: String,
    /// HTTP listen endpoint.
    pub http_endpoint: String,
    /// String interner size.
    pub string_interner_size: usize,
    /// Cached contexts limit.
    pub cached_contexts_limit: usize,
}

impl Default for OtlpConfig {
    fn default() -> Self {
        Self {
            grpc_endpoint: "127.0.0.1:4317".to_string(),
            http_endpoint: "127.0.0.1:4318".to_string(),
            string_interner_size: 32_768,
            cached_contexts_limit: 500_000,
        }
    }
}

/// OTLP relay runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Default)]
pub struct OtlpRelayConfig {
    /// GRPC relay endpoint.
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
}
