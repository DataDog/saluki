//! DogStatsD domain: source listeners, parsing, origin detection, aggregation, mapping, filters
//! (some dynamic-capable), and debug logging.

use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::defaults::{
    DEFAULT_AGGREGATE_CONTEXT_LIMIT, DEFAULT_AGGREGATE_FLUSH_INTERVAL,
    DEFAULT_AGGREGATE_PASSTHROUGH_IDLE_FLUSH_TIMEOUT, DEFAULT_AGGREGATE_WINDOW_DURATION_SECONDS,
};
use crate::Error;

// TODO: better name than Domain? Pipeline? Topology? BlueprintConfig?
/// Resolved DogStatsD configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
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

    /// Per-metric tag value allow-list rules.
    pub tag_value_allowlist: Vec<MetricTagValueAllowlistEntry>,

    /// Extra tags added to every metric.
    pub tags: Vec<String>,

    /// Telemetry emitted by the DogStatsD source.
    pub telemetry: Telemetry,

    /// Debug logging for the DogStatsD source.
    pub debug_log: DebugLog,
}

/// Source listeners and packet-decoding options.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Listeners {
    /// UDP port DogStatsD listens on.
    pub port: u16,

    /// TCP port DogStatsD listens on. (not in Datadog Agent config schema)
    pub tcp_port: u16,

    /// Path of the Unix datagram socket DogStatsD listens on.
    pub socket: Option<String>,

    /// Path of the Unix stream socket DogStatsD listens on.
    pub stream_socket: Option<String>,

    /// Windows named pipe name DogStatsD listens on. Unset when no named pipe is configured.
    pub pipe_name: Option<String>,

    /// SDDL security descriptor applied to the Windows named pipe listener.
    pub windows_pipe_security_descriptor: String,

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

    /// Number of connectionless packet decoder workers.
    pub workers_count: usize,

    /// Whether to bind multiple UDP sockets via `SO_REUSEPORT`. (not in Datadog Agent config
    /// schema)
    pub autoscale_udp_listeners: bool,

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
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
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

impl FromStr for OriginTagCardinality {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "orchestrator" => Ok(Self::Orchestrator),
            "high" => Ok(Self::High),
            "none" => Ok(Self::None),
            other => Err(Error::new_without_source(format!(
                "unknown tag cardinality `{other}`; expected low, orchestrator, high, or none"
            ))),
        }
    }
}

/// Telemetry emitted by the DogStatsD source.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Telemetry {
    /// Whether processed-metric telemetry is broken down by detected origin.
    pub origin_breakdown: bool,
}

/// Context cache sizing and sample-rate floor.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
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
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Aggregation {
    /// Length, in seconds, of each aggregation window. (not in Datadog Agent config schema)
    pub window_duration_seconds: NonZeroU64,

    /// Maximum number of contexts held per aggregation window. (not in Datadog Agent config schema)
    pub context_limit: usize,

    /// How often aggregated metrics are flushed. (not in Datadog Agent config schema)
    pub flush_interval: Duration,

    /// Whether windows that are still open are flushed on shutdown.
    ///
    /// Set by the Datadog `dogstatsd_flush_incomplete_buckets` key.
    pub flush_open_windows: bool,

    /// How long the no-aggregation passthrough waits before flushing while idle. (not in Datadog
    /// Agent config schema)
    pub passthrough_idle_flush_timeout: Duration,

    /// How long, in seconds, a counter value is retained after its last update before expiring.
    ///
    /// Set by the Datadog `dogstatsd_expiry_seconds` key. A value of `0` disables zero-value counter
    /// emission.
    pub counter_expiry_seconds: Option<u64>,

    /// How long, in seconds, a context is retained after its last update before expiring.
    pub context_expiry_seconds: u64,

    /// Whether metrics bypass aggregation and are forwarded directly.
    pub no_aggregation_pipeline: bool,

    /// Capacity of the aggregator's tag-filter result cache.
    pub aggregator_tag_filter_cache_capacity: usize,
}

impl Default for Aggregation {
    fn default() -> Self {
        Self {
            // Saluki-schema-only knobs: the Datadog Agent schema does not publish these, so they are
            // seeded only when set; absent that, these defaults stand.
            window_duration_seconds: DEFAULT_AGGREGATE_WINDOW_DURATION_SECONDS,
            context_limit: DEFAULT_AGGREGATE_CONTEXT_LIMIT,
            flush_interval: DEFAULT_AGGREGATE_FLUSH_INTERVAL,
            passthrough_idle_flush_timeout: DEFAULT_AGGREGATE_PASSTHROUGH_IDLE_FLUSH_TIMEOUT,
            // Datadog-schema knobs: always written by the witness driver, so these values are
            // placeholders that never survive translation.
            flush_open_windows: false,
            counter_expiry_seconds: None,
            context_expiry_seconds: 0,
            no_aggregation_pipeline: false,
            aggregator_tag_filter_cache_capacity: 0,
        }
    }
}

/// DogStatsD metric mapper.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Mapper {
    /// Mapper profiles that rewrite matching metric names and tags.
    pub profiles: Vec<MapperProfile>,

    /// Number of mapper match results cached.
    pub cache_size: usize,

    /// Number of entries the mapper's string interner holds. (not in Datadog Agent config schema)
    pub string_interner_size: u64,
}

/// One mapper profile: a name, a metric prefix, and the mappings under it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct MapperProfile {
    /// Profile name, for diagnostics.
    pub name: String,

    /// Metric-name prefix the profile's mappings apply to.
    pub prefix: String,

    /// The name/tag mappings under this profile.
    pub mappings: Vec<MetricMapping>,
}

/// A single metric-name mapping within a [`MapperProfile`].
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
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
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
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
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
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
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct MetricTagFilterEntry {
    /// Metric name the entry applies to.
    pub metric_name: String,

    /// Whether the listed tags are included or excluded.
    pub action: FilterAction,

    /// Tags the action applies to.
    pub tags: Vec<String>,
}

/// One tag value allow-list entry.
///
/// Rules apply to counters and sketch-backed metrics after mapper rewrites and metric namespace prefixing. Distinct
/// prefixes must not overlap. Multiple rules may use the same prefix when they target different tags.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct MetricTagValueAllowlistEntry {
    /// Non-empty metric-name prefix the entry applies to.
    ///
    /// Matching is exact and case-sensitive, including any whitespace. Empty prefixes and overlapping distinct
    /// prefixes are invalid. Multiple rules may use the same prefix when they target different tags.
    pub metric_prefix: String,

    /// Non-empty tag key whose values are constrained.
    ///
    /// Bare tags have no value and are not changed. Key/value tags with an empty value are processed normally. Empty
    /// names and names containing `:` are invalid. Matching is exact and preserves whitespace.
    pub tag_name: String,

    /// Tag values retained unchanged.
    ///
    /// The default is an empty list, which treats every key/value tag as a mismatch. The empty string is a valid list
    /// member and retains tags with an empty value. Matching is exact and preserves whitespace.
    #[serde(default)]
    pub values: Vec<String>,

    /// Action applied when a tag value is absent from [`values`][Self::values].
    ///
    /// The default is [`Remove`][TagValueMismatchAction::Remove].
    #[serde(default)]
    pub on_miss: TagValueMismatchAction,

    /// Replacement value used when `on_miss` is [`Replace`][TagValueMismatchAction::Replace].
    ///
    /// The default is `other`. This field has no effect when `on_miss` is
    /// [`Remove`][TagValueMismatchAction::Remove]. The replacement is emitted exactly as configured, including
    /// whitespace.
    #[serde(default = "default_tag_value_replacement")]
    pub replacement: String,
}

fn default_tag_value_replacement() -> String {
    "other".to_string()
}

impl Default for MetricTagValueAllowlistEntry {
    fn default() -> Self {
        Self {
            metric_prefix: String::new(),
            tag_name: String::new(),
            values: Vec::new(),
            on_miss: TagValueMismatchAction::Remove,
            replacement: default_tag_value_replacement(),
        }
    }
}

/// Reports why a metric tag value allow-list cannot be represented.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidMetricTagValueAllowlist(String);

impl fmt::Display for InvalidMetricTagValueAllowlist {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InvalidMetricTagValueAllowlist {}

/// Validates metric tag value allow-list entries.
///
/// # Errors
///
/// Returns an error for empty prefixes or tag names, tag names containing `:`, overlapping distinct prefixes, or a
/// duplicate prefix and tag pair.
pub fn validate_metric_tag_value_allowlists(
    entries: &[MetricTagValueAllowlistEntry],
) -> Result<(), InvalidMetricTagValueAllowlist> {
    for (index, entry) in entries.iter().enumerate() {
        let rule = index + 1;
        if entry.metric_prefix.is_empty() {
            return Err(InvalidMetricTagValueAllowlist(format!(
                "metric tag value allow-list rule {rule} has an empty `metric_prefix`; configure a non-empty metric-name prefix"
            )));
        }
        if entry.tag_name.is_empty() {
            return Err(InvalidMetricTagValueAllowlist(format!(
                "metric tag value allow-list rule {rule} for prefix '{}' has an empty `tag_name`; configure a non-empty tag name",
                entry.metric_prefix
            )));
        }
        if entry.tag_name.contains(':') {
            return Err(InvalidMetricTagValueAllowlist(format!(
                "metric tag value allow-list tag name '{}' contains ':'; configure only the tag key, without a colon or value",
                entry.tag_name
            )));
        }
    }

    let mut sorted_entries = entries.iter().collect::<Vec<_>>();
    sorted_entries.sort_unstable_by(|left, right| {
        left.metric_prefix
            .cmp(&right.metric_prefix)
            .then_with(|| left.tag_name.cmp(&right.tag_name))
    });

    // After sorting by prefix and then tag, duplicate prefix/tag pairs are adjacent. Any distinct prefix that extends
    // another prefix follows the complete group for the shorter prefix, so one adjacent pair also exposes that overlap.
    for pair in sorted_entries.windows(2) {
        let [left, right] = pair else {
            unreachable!("a two-entry window must contain two entries");
        };
        if left.metric_prefix == right.metric_prefix {
            if left.tag_name == right.tag_name {
                return Err(InvalidMetricTagValueAllowlist(format!(
                    "metric prefix '{}' is configured more than once for tag '{}'; configure each prefix and tag pair only once",
                    left.metric_prefix, left.tag_name
                )));
            }
        } else if right.metric_prefix.starts_with(&left.metric_prefix) {
            return Err(InvalidMetricTagValueAllowlist(format!(
                "overlapping metric prefixes '{}' and '{}' are configured; configure distinct prefixes that do not overlap",
                left.metric_prefix, right.metric_prefix
            )));
        }
    }

    Ok(())
}

/// Action applied when a tag value is absent from its allow-list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TagValueMismatchAction {
    /// Removes the tag.
    #[default]
    Remove,
    /// Replaces the tag value with the configured sentinel.
    Replace,
}

/// Whether a tag-filterlist entry includes or excludes the listed tags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum FilterAction {
    Include,
    #[default]
    Exclude,
}

/// DogStatsD debug logging (dynamic-capable).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
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
