//! Component-native configuration for DogStatsD-domain components.
//!
//! Covers the DogStatsD source, the DogStatsD mapper transform, the DogStatsD debug-log
//! destination, and the aggregate transform. Mirrors the corresponding structs in
//! `saluki-components` with source key names, aliases, and `Deserialize` impls stripped.
//!
//! Injected/non-config state is excluded: workload providers, capture-entity resolvers, capture and
//! replay control handles, and the retained `Option<GenericConfiguration>`.

use std::num::NonZeroU64;
use std::path::PathBuf;
use std::time::Duration;

use bytesize::ByteSize;
use saluki_context::origin::OriginTagCardinality;
use stringtheory::MetaString;

/// Configuration for the DogStatsD source component.
///
/// Mirrors `DogStatsDConfiguration` in `saluki-components`. The injected `workload_provider`,
/// `capture_entity_resolver`, `capture_control`, and `replay_control` fields are excluded as
/// runtime-injected state.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct DogStatsDConfig {
    /// The size of the receive buffer, in bytes.
    ///
    /// Payloads larger than this are truncated, leading to discarded messages. Defaults to 8192.
    pub buffer_size: usize,

    /// The number of message buffers to allocate overall.
    ///
    /// Defaults to 128.
    pub buffer_count: usize,

    /// The UDP port to listen on. If set to `0`, UDP is not used.
    ///
    /// Defaults to 8125.
    pub port: u16,

    /// The UDP/UDS socket receive buffer size, in bytes. If set to `0`, the OS default is used.
    ///
    /// Defaults to 0.
    pub socket_receive_buffer_size: usize,

    /// The TCP port to listen on. If set to `0`, TCP is not used.
    ///
    /// Defaults to 0.
    pub tcp_port: u16,

    /// The host to forward framed DogStatsD messages to over UDP.
    ///
    /// Forwarding is enabled only when this is non-empty and `statsd_forward_port` is non-zero.
    /// Defaults to unset.
    pub statsd_forward_host: Option<MetaString>,

    /// The port to forward framed DogStatsD messages to over UDP.
    ///
    /// Defaults to 0.
    pub statsd_forward_port: u16,

    /// The Unix domain socket path to listen on, in datagram mode. Defaults to unset.
    pub socket_path: Option<String>,

    /// The Unix domain socket path to listen on, in stream mode. Defaults to unset.
    pub socket_stream_path: Option<String>,

    /// Whether to log oversized DogStatsD stream frames.
    ///
    /// Defaults to `false`.
    pub stream_log_too_big: bool,

    /// Whether to lower DogStatsD parse-failure logs to debug level.
    ///
    /// Defaults to `false`.
    pub disable_verbose_logs: bool,

    /// Listener types that require DogStatsD messages to be newline-terminated.
    ///
    /// Valid values are `udp`, `uds`, and `named_pipe`. Defaults to empty.
    pub eol_required: Vec<String>,

    /// The host address to bind UDP and TCP listeners to.
    ///
    /// Ignored when `non_local_traffic` is `true`. Defaults to unset, which binds to `127.0.0.1`.
    pub bind_host: Option<String>,

    /// Whether to listen for non-local traffic in UDP mode.
    ///
    /// Defaults to `false`.
    pub non_local_traffic: bool,

    /// Whether to autoscale UDP stream handlers using `SO_REUSEPORT`.
    ///
    /// Has no effect on non-Linux platforms. Defaults to `false`.
    pub autoscale_udp_listeners: bool,

    /// Whether to allow heap allocations when resolving contexts.
    ///
    /// Defaults to `true`.
    pub allow_context_heap_allocations: bool,

    /// Whether to enable support for no-aggregation pipelines.
    ///
    /// Defaults to `true`.
    pub no_aggregation_pipeline_support: bool,

    /// Number of entries for the string interner, as interpreted by the Core Agent.
    ///
    /// Multiplied by 512 bytes per entry when `context_string_interner_size_bytes` is unset.
    /// Defaults to 4096 entries.
    pub context_string_interner_entry_count: u64,

    /// Total size of the string interner used for contexts, in bytes.
    ///
    /// When set, takes priority over `context_string_interner_entry_count`. Defaults to unset.
    pub context_string_interner_size_bytes: Option<ByteSize>,

    /// The maximum number of cached contexts to allow.
    ///
    /// Defaults to 500,000.
    pub cached_contexts_limit: usize,

    /// The maximum number of cached tagsets to allow.
    ///
    /// Defaults to 500,000.
    pub cached_tagsets_limit: usize,

    /// The number of seconds after which cached contexts expire.
    ///
    /// Defaults to 20 seconds.
    pub context_expiry_seconds: u64,

    /// Whether to enable permissive mode in the decoder.
    ///
    /// Defaults to `true`.
    pub permissive_decoding: bool,

    /// The minimum sample rate allowed for metrics.
    ///
    /// Sample rates lower than this are clamped. Defaults to `0.000000003845`.
    pub minimum_sample_rate: f64,

    /// Which payload types to forward to the backend.
    pub enable_payloads: EnablePayloadsConfiguration,

    /// Origin detection and enrichment configuration.
    pub origin_enrichment: OriginEnrichmentConfiguration,

    /// Additional tags to add to all metrics. Defaults to empty.
    pub additional_tags: Vec<String>,

    /// The directory where DogStatsD capture files are written by default.
    ///
    /// Defaults to empty.
    pub capture_path: PathBuf,

    /// The maximum number of captured packets that can be queued for persistence.
    ///
    /// Values below 1024 are raised to 1024. Defaults to 1024.
    pub capture_depth: usize,

    /// Provider kind tag appended to all metrics as `provider_kind:<value>`.
    ///
    /// When empty or absent, no tag is added. Defaults to empty.
    pub provider_kind: String,
}

impl Default for DogStatsDConfig {
    fn default() -> Self {
        Self {
            buffer_size: 8192,
            buffer_count: 128,
            port: 8125,
            socket_receive_buffer_size: 0,
            tcp_port: 0,
            statsd_forward_host: None,
            statsd_forward_port: 0,
            socket_path: None,
            socket_stream_path: None,
            stream_log_too_big: false,
            disable_verbose_logs: false,
            eol_required: Vec::new(),
            bind_host: None,
            non_local_traffic: false,
            autoscale_udp_listeners: false,
            allow_context_heap_allocations: true,
            no_aggregation_pipeline_support: true,
            context_string_interner_entry_count: 4096,
            context_string_interner_size_bytes: None,
            cached_contexts_limit: 500_000,
            cached_tagsets_limit: 500_000,
            context_expiry_seconds: 20,
            permissive_decoding: true,
            minimum_sample_rate: 0.000000003845,
            enable_payloads: EnablePayloadsConfiguration::default(),
            origin_enrichment: OriginEnrichmentConfiguration::default(),
            additional_tags: Vec::new(),
            capture_path: PathBuf::new(),
            capture_depth: 1024,
            provider_kind: String::new(),
        }
    }
}

/// Which DogStatsD payload types to forward to the backend.
///
/// Mirrors `EnablePayloadsConfiguration` in `saluki-components`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct EnablePayloadsConfiguration {
    /// Whether to enable sending series (counter/gauge/rate) payloads.
    ///
    /// Defaults to `true`.
    pub series: bool,

    /// Whether to enable sending sketch (distribution) payloads.
    ///
    /// Defaults to `true`.
    pub sketches: bool,

    /// Whether to enable sending event payloads.
    ///
    /// Defaults to `true`.
    pub events: bool,

    /// Whether to enable sending service check payloads.
    ///
    /// Defaults to `true`.
    pub service_checks: bool,
}

impl Default for EnablePayloadsConfiguration {
    fn default() -> Self {
        Self {
            series: true,
            sketches: true,
            events: true,
            service_checks: true,
        }
    }
}

/// Origin detection and enrichment configuration for the DogStatsD source.
///
/// Mirrors `OriginEnrichmentConfiguration` in `saluki-components`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct OriginEnrichmentConfiguration {
    /// Whether origin detection is enabled.
    ///
    /// Defaults to `false`.
    pub enabled: bool,

    /// Whether a client-provided entity ID takes precedence over detected origin metadata.
    ///
    /// Defaults to `false`.
    pub entity_id_precedence: bool,

    /// The default cardinality of tags to enrich metrics with.
    ///
    /// Defaults to [`OriginTagCardinality::Low`].
    pub tag_cardinality: OriginTagCardinality,

    /// Whether to use the unified origin detection behavior.
    ///
    /// Defaults to `false`.
    pub origin_detection_unified: bool,

    /// Whether to opt out of origin detection for metrics that explicitly request `none` cardinality.
    ///
    /// Defaults to `true`.
    pub origin_detection_optout: bool,

    /// Whether to parse client-provided origin fields from DogStatsD payloads.
    ///
    /// Defaults to `false`.
    pub origin_detection_client: bool,
}

impl Default for OriginEnrichmentConfiguration {
    fn default() -> Self {
        Self {
            enabled: false,
            entity_id_precedence: false,
            tag_cardinality: OriginTagCardinality::Low,
            origin_detection_unified: false,
            origin_detection_optout: true,
            origin_detection_client: false,
        }
    }
}

/// Configuration for the DogStatsD mapper transform component.
///
/// Mirrors `DogStatsDMapperConfiguration` in `saluki-components`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct DogStatsDMapperConfig {
    /// Total size of the string interner used for mapped contexts, in bytes.
    ///
    /// Defaults to 64 KiB.
    pub context_string_interner_bytes: ByteSize,

    /// Maximum number of mapped results to cache.
    ///
    /// When set to `0`, the cache is disabled. Defaults to 1000.
    pub cache_size: usize,

    /// The configured mapper profiles.
    pub dogstatsd_mapper_profiles: MapperProfileConfigs,
}

impl Default for DogStatsDMapperConfig {
    fn default() -> Self {
        Self {
            context_string_interner_bytes: ByteSize::kib(64),
            cache_size: 1000,
            dogstatsd_mapper_profiles: MapperProfileConfigs::default(),
        }
    }
}

/// The set of configured DogStatsD mapper profiles.
///
/// Mirrors `MapperProfileConfigs` in `saluki-components`.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct MapperProfileConfigs(pub Vec<MappingProfileConfig>);

/// A single DogStatsD mapper profile.
///
/// Mirrors `MappingProfileConfig` in `saluki-components`.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct MappingProfileConfig {
    /// The profile name.
    pub name: String,

    /// The metric name prefix this profile applies to.
    pub prefix: String,

    /// The metric mappings within this profile.
    pub mappings: Vec<MetricMappingConfig>,
}

/// A single DogStatsD metric mapping rule.
///
/// Mirrors `MetricMappingConfig` in `saluki-components`.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct MetricMappingConfig {
    /// The metric name to extract groups from, using wildcard or regex matching.
    pub metric_match: String,

    /// The type of match to apply: `wildcard` or `regex`.
    pub match_type: String,

    /// The new metric name, with tags inlined from the matched groups.
    pub name: String,

    /// The tags to apply, keyed by tag name with values drawn from the matched groups.
    pub tags: std::collections::HashMap<String, String>,
}

/// Configuration for the DogStatsD debug-log destination component.
///
/// Mirrors `DogStatsDDebugLogConfiguration` in `saluki-components`. The injected
/// `Option<GenericConfiguration>` field is excluded. The original component falls back to an
/// injected `default_log_file_path` when `log_file` is empty; here `log_file` simply carries the
/// resolved path.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct DogStatsDDebugLogConfig {
    /// Whether DogStatsD metric-level statistics are enabled.
    ///
    /// Defaults to `false`.
    pub metrics_stats_enabled: bool,

    /// Whether DogStatsD metric-level statistics are written to a log file.
    ///
    /// Controls whether the destination is added to the topology. Defaults to `true`.
    pub logging_enabled: bool,

    /// Path to the DogStatsD debug log file.
    pub log_file: PathBuf,

    /// Maximum size of the active debug log file before rotation.
    ///
    /// Defaults to 10 MB.
    pub log_file_max_size: ByteSize,

    /// Number of rotated debug log files to keep.
    ///
    /// Defaults to 3.
    pub log_file_max_rolls: usize,
}

impl Default for DogStatsDDebugLogConfig {
    fn default() -> Self {
        Self {
            metrics_stats_enabled: false,
            logging_enabled: true,
            log_file: PathBuf::new(),
            log_file_max_size: ByteSize::mb(10),
            log_file_max_rolls: 3,
        }
    }
}

/// Configuration for the aggregate transform component.
///
/// Mirrors `AggregateConfiguration` in `saluki-components`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct AggregateConfig {
    /// Size of the aggregation window, in seconds.
    ///
    /// Cannot be zero. Defaults to 10 seconds.
    pub window_duration_seconds: NonZeroU64,

    /// How often to flush buckets.
    ///
    /// Defaults to 15 seconds.
    pub primary_flush_interval: Duration,

    /// Maximum number of contexts to aggregate per window.
    ///
    /// Defaults to 1,000,000.
    pub context_limit: usize,

    /// Whether to flush open buckets when stopping the transform.
    ///
    /// Defaults to `false`.
    pub flush_open_windows: bool,

    /// How long to keep idle counters alive after they have been flushed, in seconds.
    ///
    /// `Some(0)` or `None` disables idle counter keep-alive. Defaults to `Some(300)`.
    pub counter_expiry_seconds: Option<u64>,

    /// Whether to immediately forward metrics that carry pre-defined timestamps.
    ///
    /// Defaults to `true`.
    pub passthrough_timestamped_metrics: bool,

    /// How often to flush buffered passthrough metrics.
    ///
    /// Defaults to 1 second.
    pub passthrough_idle_flush_timeout: Duration,

    /// Histogram aggregation configuration.
    pub hist_config: HistogramConfiguration,
}

impl Default for AggregateConfig {
    fn default() -> Self {
        Self {
            window_duration_seconds: NonZeroU64::new(10).expect("10 is non-zero"),
            primary_flush_interval: Duration::from_secs(15),
            context_limit: 1_000_000,
            flush_open_windows: false,
            counter_expiry_seconds: Some(300),
            passthrough_timestamped_metrics: true,
            passthrough_idle_flush_timeout: Duration::from_secs(1),
            hist_config: HistogramConfiguration::default(),
        }
    }
}

/// Histogram aggregation configuration for the aggregate transform.
///
/// Mirrors the resolved form of `HistogramConfiguration` in `saluki-components` (the parsed result
/// of `histogram_aggregates` and `histogram_percentiles`, not the raw string lists).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct HistogramConfiguration {
    /// The aggregate statistics to calculate over histograms.
    pub statistics: Vec<HistogramStatistic>,

    /// Whether to copy histograms to distributions.
    ///
    /// Defaults to `false`.
    pub copy_to_distribution: bool,

    /// Prefix to append to the names of distributions copied from histograms.
    ///
    /// Defaults to empty.
    pub copy_to_distribution_prefix: String,
}

impl Default for HistogramConfiguration {
    fn default() -> Self {
        Self {
            statistics: vec![
                HistogramStatistic::Maximum,
                HistogramStatistic::Median,
                HistogramStatistic::Average,
                HistogramStatistic::Count,
                HistogramStatistic::Percentile {
                    q: 0.95,
                    suffix: MetaString::from_static("95percentile"),
                },
            ],
            copy_to_distribution: false,
            copy_to_distribution_prefix: String::new(),
        }
    }
}

/// A histogram statistic to calculate.
///
/// Mirrors `HistogramStatistic` in `saluki-components`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub enum HistogramStatistic {
    /// Total count of values in the histogram.
    Count,

    /// Sum of all values in the histogram.
    Sum,

    /// Minimum value in the histogram.
    Minimum,

    /// Maximum value in the histogram.
    Maximum,

    /// Average value in the histogram.
    Average,

    /// Median value in the histogram.
    Median,

    /// A given percentile calculated from the histogram.
    Percentile {
        /// Quantile value to calculate, in the range [0.0, 1.0].
        q: f64,

        /// Suffix to append to the metric name, representing the percentile (for example,
        /// `95percentile`).
        suffix: MetaString,
    },
}

/// Configuration for the DogStatsD metric-tag filterlist transform.
///
/// Mirrors the resolved form of the DogStatsD metric-tag filterlist in `saluki-components`. This is
/// a dynamic-capable slice: at runtime the metric-tag filterlist can be retranslated and pushed to
/// the component via a [`ScopedConfig`]. The injected `Option<GenericConfiguration>` used by the
/// original component for `watch_for_updates` is excluded.
///
/// [`ScopedConfig`]: crate::ScopedConfig
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct TagFilterlistConfig {
    /// The per-metric tag filter entries.
    ///
    /// Each entry names a metric and either includes or excludes a set of tags on it. Defaults to
    /// empty (no filtering).
    pub entries: Vec<MetricTagFilterEntry>,

    /// The capacity of the per-context filter result cache.
    ///
    /// Bounds how many distinct contexts have their computed filter decision cached. Defaults to 0.
    pub context_cache_capacity: usize,
}

/// A single metric-tag filter entry within a [`TagFilterlistConfig`].
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct MetricTagFilterEntry {
    /// The metric name this entry applies to.
    pub metric_name: String,

    /// Whether the listed tags are an include list or an exclude list.
    ///
    /// Defaults to [`FilterAction::Exclude`].
    pub action: FilterAction,

    /// The tags this entry includes or excludes, depending on `action`.
    ///
    /// Defaults to empty.
    pub tags: Vec<String>,
}

/// Whether a metric-tag filter entry includes or excludes the listed tags.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub enum FilterAction {
    /// Keep only the listed tags; drop all others.
    Include,

    /// Drop the listed tags; keep all others.
    ///
    /// This is the default.
    #[default]
    Exclude,
}

/// Configuration for the DogStatsD metric prefix/name filter.
///
/// Mirrors the resolved form of the DogStatsD prefix filter in `saluki-components`. This is a
/// dynamic-capable slice; the injected `Option<GenericConfiguration>` used by the original component
/// for runtime watching is excluded.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct DogStatsDPrefixFilterConfig {
    /// The metric name prefix this filter scopes itself to.
    ///
    /// Only metrics whose names start with this prefix are considered. Defaults to empty.
    pub metric_prefix: String,

    /// Metric prefixes that are unconditionally blocked.
    ///
    /// Defaults to empty.
    pub metric_prefix_blocklist: Vec<String>,

    /// The metric names (or prefixes) that are allowed through the filter.
    ///
    /// Defaults to empty.
    pub metric_filterlist: Vec<String>,

    /// Whether `metric_filterlist` entries match as prefixes rather than exact names.
    ///
    /// Defaults to `false`.
    pub metric_filterlist_match_prefix: bool,

    /// The metric names (or prefixes) that are blocked by the filter.
    ///
    /// Defaults to empty.
    pub metric_blocklist: Vec<String>,

    /// Whether `metric_blocklist` entries match as prefixes rather than exact names.
    ///
    /// Defaults to `false`.
    pub metric_blocklist_match_prefix: bool,
}

/// Configuration for the DogStatsD post-aggregation metric filter.
///
/// Mirrors the resolved form of the DogStatsD post-aggregate filter in `saluki-components`. This is
/// a dynamic-capable slice; the injected `Option<GenericConfiguration>` used by the original
/// component for runtime watching is excluded.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct DogStatsDPostAggregateFilterConfig {
    /// The metric names (or prefixes) that are allowed through the post-aggregation filter.
    ///
    /// Defaults to empty.
    pub metric_filterlist: Vec<String>,

    /// Whether `metric_filterlist` entries match as prefixes rather than exact names.
    ///
    /// Defaults to `false`.
    pub metric_filterlist_match_prefix: bool,

    /// The metric names (or prefixes) that are blocked by the post-aggregation filter.
    ///
    /// Defaults to empty.
    pub metric_blocklist: Vec<String>,

    /// Whether `metric_blocklist` entries match as prefixes rather than exact names.
    ///
    /// Defaults to `false`.
    pub metric_blocklist_match_prefix: bool,

    /// The set of histogram aggregate statistics to compute post-aggregation.
    ///
    /// Defaults to empty.
    pub histogram_aggregates: Vec<String>,

    /// The set of histogram percentiles to compute post-aggregation.
    ///
    /// Defaults to empty.
    pub histogram_percentiles: Vec<String>,
}
