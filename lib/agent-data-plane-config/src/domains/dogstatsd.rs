//! DogStatsD domain: source listeners, parsing, origin detection, aggregation, mapping, filters
//! (some dynamic-capable), and debug logging.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;

// TODO: better name than Domain? Pipeline? Topology? BlueprintConfig?
/// Resolved DogStatsD configuration.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Domain {
    /// Source listeners and packet-decoding options.
    pub listeners: Listeners,

    /// Origin detection and tag cardinality.
    pub origin: OriginDetection,

    /// Context cache sizing and the sample-rate floor.
    pub contexts: Contexts,

    /// Metric aggregation window and flush behavior.
    pub aggregation: Aggregation,

    /// Metric-name mapper.
    pub mapper: Mapper,

    /// Which payload types are emitted.
    pub enable_payloads: EnablePayloads,

    /// Metric-name prefix filtering.
    pub prefix_filter: PrefixFilter,

    /// Per-metric tag include/exclude rules.
    pub tag_filterlist: Vec<MetricTagFilterEntry>,

    /// Extra tags added to every metric.
    pub tags: Vec<String>,

    /// Telemetry emitted by the DogStatsD source.
    pub telemetry: Telemetry,

    /// Debug logging for the DogStatsD source.
    pub debug_log: DebugLog,
}

/// Source listeners and packet-decoding options.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Listeners {
    /// UDP port DogStatsD listens on.
    pub port: u16,

    /// TCP port DogStatsD listens on. (not in Datadog Agent config schema)
    pub tcp_port: u16,

    /// Path of the Unix datagram socket DogStatsD listens on.
    pub socket: Option<String>,

    /// Path of the Unix stream socket DogStatsD listens on.
    pub stream_socket: Option<String>,

    /// Whether the UDP listener accepts traffic from non-local addresses.
    pub non_local_traffic: bool,

    /// Host the UDP listener binds to.
    pub bind_host: Option<String>,

    /// Size, in bytes, requested for the socket receive buffer.
    pub so_rcvbuf: usize,

    /// Size, in bytes, of each packet receive buffer.
    pub buffer_size: usize,

    /// Number of receive buffers allocated. (not in Datadog Agent config schema)
    pub buffer_count: usize,

    /// Maximum number of receive buffers. (not in Datadog Agent config schema)
    pub buffer_count_max: usize,

    /// Whether to bind multiple UDP sockets via `SO_REUSEPORT`. (not in Datadog Agent config
    /// schema)
    pub autoscale_udp_listeners: bool,

    /// Which listener implementation provides packets.
    pub provider_kind: String,

    /// Path a traffic capture is written to or replayed from.
    pub capture_path: PathBuf,

    /// Maximum recursion depth when replaying a traffic capture.
    pub capture_depth: usize,

    /// End-of-line markers required to terminate a stream-socket message.
    pub eol_required: Vec<String>,

    /// Whether to log stream messages that exceed the buffer size.
    pub stream_log_too_big: bool,

    /// Whether to relax decoder strictness on malformed packets. (not in Datadog Agent config
    /// schema)
    pub permissive_decoding: bool,

    /// Host that received metrics are additionally forwarded to.
    pub forward_host: Option<String>,

    /// Port that received metrics are additionally forwarded to.
    pub forward_port: u16,
}

/// Origin detection and tag cardinality.
#[derive(Clone, Debug, Default, Serialize)]
pub struct OriginDetection {
    /// Whether origin detection tags metrics with their source workload.
    pub detection: bool,

    /// Whether client-supplied origin information is honored.
    pub detection_client: bool,

    /// Whether the unified origin-detection scheme is used.
    pub unified: bool,

    /// Whether a client may opt out of origin detection per metric.
    pub optout_enabled: bool,

    /// Whether a client-supplied entity ID takes precedence over the detected origin.
    pub entity_id_precedence: bool,

    /// Tag cardinality applied to origin-detected tags.
    pub tag_cardinality: OriginTagCardinality,
}

/// Tag cardinality applied during origin detection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum OriginTagCardinality {
    #[default]
    Low,
    Orchestrator,
    High,
    None,
}

/// Telemetry emitted by the DogStatsD source.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Telemetry {
    /// Whether processed-metric telemetry is broken down by detected origin.
    pub origin_breakdown: bool,
}

/// Context cache sizing and sample-rate floor.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Contexts {
    /// Maximum number of metric contexts held in the cache. (not in Datadog Agent config schema)
    pub cached_contexts_limit: usize,

    /// Maximum number of tagsets held in the cache. (not in Datadog Agent config schema)
    pub cached_tagsets_limit: usize,

    /// Number of entries the context string interner holds.
    pub string_interner_size: u64,

    /// Byte budget for the context string interner, overriding the entry count when set. (not in
    /// Datadog Agent config schema)
    pub string_interner_size_bytes: Option<u64>,

    /// Whether contexts may be heap-allocated when the interner is full. (not in Datadog Agent
    /// config schema)
    pub allow_context_heap_allocs: bool,

    /// Lowest sample rate accepted before a metric is rejected. (not in Datadog Agent config
    /// schema)
    pub minimum_sample_rate: f64,
}

/// Metric aggregation window and flush behavior.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Aggregation {
    /// Length, in seconds, of each aggregation window; must be non-zero. (not in Datadog Agent
    /// config schema)
    pub window_duration_seconds: u64,

    /// Maximum number of contexts held per aggregation window. (not in Datadog Agent config schema)
    pub context_limit: usize,

    /// How often aggregated metrics are flushed. (not in Datadog Agent config schema)
    pub flush_interval: Duration,

    /// Whether windows that are still open are flushed on shutdown. (not in Datadog Agent config
    /// schema)
    pub flush_open_windows: bool,

    /// How long the no-aggregation passthrough waits before flushing while idle. (not in Datadog
    /// Agent config schema)
    pub passthrough_idle_flush_timeout: Duration,

    /// How long, in seconds, a counter value is retained after its last update before expiring.
    pub counter_expiry_seconds: Option<u64>,

    /// How long, in seconds, a context is retained after its last update before expiring.
    pub context_expiry_seconds: u64,

    /// Whether incomplete aggregation buckets are flushed rather than discarded.
    pub flush_incomplete_buckets: bool,

    /// Whether metrics bypass aggregation and are forwarded directly.
    pub no_aggregation_pipeline: bool,

    /// Capacity of the aggregator's tag-filter result cache.
    pub aggregator_tag_filter_cache_capacity: usize,
}

/// DogStatsD metric mapper.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Mapper {
    /// Mapper profiles that rewrite matching metric names and tags.
    pub profiles: Vec<MapperProfile>,

    /// Number of mapper match results cached.
    pub cache_size: usize,

    /// Number of entries the mapper's string interner holds. (not in Datadog Agent config schema)
    pub string_interner_size: u64,
}

/// One mapper profile: a name, a metric prefix, and the mappings under it.
#[derive(Clone, Debug, Default, Serialize)]
pub struct MapperProfile {
    /// Profile name, for diagnostics.
    pub name: String,

    /// Metric-name prefix the profile's mappings apply to.
    pub prefix: String,

    /// The name/tag mappings under this profile.
    pub mappings: Vec<MetricMapping>,
}

/// A single metric-name mapping within a [`MapperProfile`].
#[derive(Clone, Debug, Default, Serialize)]
pub struct MetricMapping {
    /// Pattern a metric name must match.
    pub metric_match: String,

    /// How `metric_match` is interpreted (for example, `wildcard` or `regex`).
    pub match_type: String,

    /// Replacement name emitted for a matching metric.
    pub name: String,

    /// Tags added to a matching metric, with values captured from the match.
    pub tags: HashMap<String, String>,
}

/// Which payload types are emitted.
#[derive(Clone, Debug, Default, Serialize)]
pub struct EnablePayloads {
    /// Whether event payloads are emitted.
    pub events: bool,

    /// Whether series (metric) payloads are emitted.
    pub series: bool,

    /// Whether service-check payloads are emitted.
    pub service_checks: bool,

    /// Whether sketch (distribution) payloads are emitted.
    pub sketches: bool,
}

/// Metric-name prefix filtering (dynamic-capable).
#[derive(Clone, Debug, Default, Serialize)]
pub struct PrefixFilter {
    /// Metric names (or prefixes) that are allowed through; others are dropped.
    pub metric_filterlist: Vec<String>,

    /// Whether filterlist entries match by prefix rather than exact name.
    pub metric_filterlist_match_prefix: bool,

    /// Metric names (or prefixes) that are blocked.
    pub metric_blocklist: Vec<String>,

    /// Whether blocklist entries match by prefix rather than exact name.
    pub metric_blocklist_match_prefix: bool,

    /// Namespace prepended to every metric name.
    pub metric_namespace: String,

    /// Namespaces excluded from the metric-namespace prefixing.
    pub metric_namespace_blocklist: Vec<String>,
}

/// One tag-filterlist entry (dynamic-capable).
#[derive(Clone, Debug, Default, Serialize)]
pub struct MetricTagFilterEntry {
    /// Metric name the entry applies to.
    pub metric_name: String,

    /// Whether the listed tags are included or excluded.
    pub action: FilterAction,

    /// Tags the action applies to.
    pub tags: Vec<String>,
}

/// Whether a tag-filterlist entry includes or excludes the listed tags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum FilterAction {
    Include,
    #[default]
    Exclude,
}

/// DogStatsD debug logging (dynamic-capable).
#[derive(Clone, Debug, Default, Serialize)]
pub struct DebugLog {
    /// Whether DogStatsD debug logging is enabled.
    pub logging_enabled: bool,

    /// Path of the DogStatsD debug log file.
    pub log_file: PathBuf,

    /// Number of rotated debug log files retained.
    pub log_file_max_rolls: usize,

    /// Maximum size, in bytes, a debug log file reaches before it is rotated.
    pub log_file_max_size: u64,

    /// Whether per-metric processing statistics are collected.
    pub metrics_stats_enable: bool,

    /// Whether verbose per-packet log lines are suppressed.
    pub disable_verbose_logs: bool,
}
