//! DogStatsD-domain translation: the source listener, the mapper transform, the debug-log
//! destination, the aggregate transform's histogram settings, and the prefix/tag/post-aggregate
//! filters.
//!
//! Mirrors the conversions the original `DogStatsDConfiguration`, `DogStatsDMapperConfiguration`,
//! `DogStatsDDebugLogConfiguration`, and the binary-local filter components performed. Leaf structs
//! hold raw resolved values; runtime fixups the original applied at construction (DNS-resolving
//! `bind_host`, the `512 bytes/entry` interner multiplier, the `capture_depth` floor, the trailing
//! `.` on a metric prefix) remain component concerns and are not re-applied here.

use bytesize::ByteSize;
use datadog_agent_config::TranslateError;
use saluki_component_config::dogstatsd::{
    AggregateConfig, DogStatsDConfig, DogStatsDDebugLogConfig, DogStatsDMapperConfig,
    DogStatsDPostAggregateFilterConfig, DogStatsDPrefixFilterConfig, HistogramStatistic, MapperProfileConfigs,
    MappingProfileConfig, MetricMappingConfig, TagFilterlistConfig,
};
use saluki_context::origin::OriginTagCardinality;
use stringtheory::MetaString;

/// Returns `Some(s)` when `s` is non-empty.
fn non_empty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

// ----- source -----

/// `dogstatsd_buffer_size` -> receive buffer size.
pub fn set_buffer_size(config: &mut DogStatsDConfig, value: i64) {
    config.buffer_size = value.max(0) as usize;
}

/// `dogstatsd_port` -> UDP listen port.
pub fn set_port(config: &mut DogStatsDConfig, value: i64) {
    config.port = value.clamp(0, u16::MAX as i64) as u16;
}

/// `dogstatsd_so_rcvbuf` -> socket receive buffer size.
pub fn set_socket_receive_buffer_size(config: &mut DogStatsDConfig, value: i64) {
    config.socket_receive_buffer_size = value.max(0) as usize;
}

/// `dogstatsd_socket` -> datagram-mode UDS path (empty/absent clears).
pub fn set_socket_path(config: &mut DogStatsDConfig, value: Option<String>) {
    config.socket_path = value.and_then(non_empty);
}

/// `dogstatsd_stream_socket` -> stream-mode UDS path (empty clears).
pub fn set_socket_stream_path(config: &mut DogStatsDConfig, value: String) {
    config.socket_stream_path = non_empty(value);
}

/// `dogstatsd_stream_log_too_big` -> whether oversized stream frames are logged.
pub fn set_stream_log_too_big(config: &mut DogStatsDConfig, value: bool) {
    config.stream_log_too_big = value;
}

/// `dogstatsd_eol_required` -> listener types requiring newline-terminated messages.
pub fn set_eol_required(config: &mut DogStatsDConfig, value: Vec<String>) {
    config.eol_required = value;
}

/// `bind_host` -> the host UDP/TCP listeners bind to (empty clears).
pub fn set_bind_host(config: &mut DogStatsDConfig, value: String) {
    config.bind_host = non_empty(value);
}

/// `dogstatsd_non_local_traffic` -> whether to listen for non-local UDP traffic.
pub fn set_non_local_traffic(config: &mut DogStatsDConfig, value: bool) {
    config.non_local_traffic = value;
}

/// `dogstatsd_string_interner_size` -> context interner entry count (the component multiplies by
/// 512 bytes/entry when no explicit byte size is set).
pub fn set_context_string_interner_entry_count(config: &mut DogStatsDConfig, value: i64) {
    config.context_string_interner_entry_count = value.max(0) as u64;
}

/// `dogstatsd_context_expiry_seconds` -> cached-context expiry.
pub fn set_context_expiry_seconds(config: &mut DogStatsDConfig, value: i64) {
    config.context_expiry_seconds = value.max(0) as u64;
}

/// `dogstatsd_tags` -> additional tags appended to all metrics.
pub fn set_additional_tags(config: &mut DogStatsDConfig, value: Vec<String>) {
    config.additional_tags = value;
}

/// `dogstatsd_capture_path` -> capture-file directory.
pub fn set_capture_path(config: &mut DogStatsDConfig, value: String) {
    config.capture_path = std::path::PathBuf::from(value);
}

/// `dogstatsd_capture_depth` -> capture queue depth (the component raises values below 1024).
pub fn set_capture_depth(config: &mut DogStatsDConfig, value: i64) {
    config.capture_depth = value.max(0) as usize;
}

/// `provider_kind` -> provider-kind tag value.
pub fn set_provider_kind(config: &mut DogStatsDConfig, value: String) {
    config.provider_kind = value;
}

/// `statsd_forward_host` -> framed-message UDP forward host (empty clears).
pub fn set_statsd_forward_host(config: &mut DogStatsDConfig, value: String) {
    config.statsd_forward_host = non_empty(value).map(MetaString::from);
}

/// `statsd_forward_port` -> framed-message UDP forward port.
pub fn set_statsd_forward_port(config: &mut DogStatsDConfig, value: i64) {
    config.statsd_forward_port = value.clamp(0, u16::MAX as i64) as u16;
}

/// `dogstatsd_no_aggregation_pipeline` -> no-aggregation pipeline support.
pub fn set_no_aggregation_pipeline_support(config: &mut DogStatsDConfig, value: bool) {
    config.no_aggregation_pipeline_support = value;
}

// enable_payloads: dogstatsd source mirrors the top-level enable_payloads.* keys.

/// `enable_payloads.series` -> whether series payloads are forwarded.
pub fn set_enable_payloads_series(config: &mut DogStatsDConfig, value: bool) {
    config.enable_payloads.series = value;
}

/// `enable_payloads.sketches` -> whether sketch payloads are forwarded.
pub fn set_enable_payloads_sketches(config: &mut DogStatsDConfig, value: bool) {
    config.enable_payloads.sketches = value;
}

/// `enable_payloads.events` -> whether event payloads are forwarded.
pub fn set_enable_payloads_events(config: &mut DogStatsDConfig, value: bool) {
    config.enable_payloads.events = value;
}

/// `enable_payloads.service_checks` -> whether service-check payloads are forwarded.
pub fn set_enable_payloads_service_checks(config: &mut DogStatsDConfig, value: bool) {
    config.enable_payloads.service_checks = value;
}

// origin enrichment

/// `dogstatsd_origin_detection` -> origin detection enable flag.
pub fn set_origin_detection(config: &mut DogStatsDConfig, value: bool) {
    config.origin_enrichment.enabled = value;
}

/// `dogstatsd_entity_id_precedence` -> whether a client entity ID outranks detected origin.
pub fn set_entity_id_precedence(config: &mut DogStatsDConfig, value: bool) {
    config.origin_enrichment.entity_id_precedence = value;
}

/// `origin_detection_unified` -> unified origin-detection behavior flag.
pub fn set_origin_detection_unified(config: &mut DogStatsDConfig, value: bool) {
    config.origin_enrichment.origin_detection_unified = value;
}

/// `dogstatsd_origin_optout_enabled` -> origin-detection opt-out for `none`-cardinality metrics.
pub fn set_origin_detection_optout(config: &mut DogStatsDConfig, value: bool) {
    config.origin_enrichment.origin_detection_optout = value;
}

/// `dogstatsd_origin_detection_client` -> whether client-provided origin fields are parsed.
pub fn set_origin_detection_client(config: &mut DogStatsDConfig, value: bool) {
    config.origin_enrichment.origin_detection_client = value;
}

/// `dogstatsd_tag_cardinality` -> default origin tag cardinality.
///
/// Mirrors the original case-insensitive parse (`low`/`orchestrator`/`high`/`none`). On an unknown
/// value, records an error against the key and leaves the seeded/default cardinality in place.
pub fn set_tag_cardinality(config: &mut DogStatsDConfig, value: String) -> Result<(), TranslateError> {
    match OriginTagCardinality::try_from(value.as_str()) {
        Ok(c) => {
            config.origin_enrichment.tag_cardinality = c;
            Ok(())
        }
        Err(reason) => Err(TranslateError::for_key("dogstatsd_tag_cardinality", reason)),
    }
}

// ----- mapper -----

/// `dogstatsd_mapper_cache_size` -> mapper result cache size.
pub fn set_mapper_cache_size(mapper: &mut DogStatsDMapperConfig, value: i64) {
    mapper.cache_size = value.max(0) as usize;
}

/// `dogstatsd_mapper_profiles` -> typed mapper profiles.
///
/// The witnessed value is the raw JSON profile array. Each element is parsed into a typed
/// [`MappingProfileConfig`] mirroring the original `serde` shape (`match` -> `metric_match`,
/// `match_type`, `name`, `tags`). On a malformed element, records an error against the key and
/// leaves the seeded/default (empty) profile set in place.
pub fn set_mapper_profiles(
    mapper: &mut DogStatsDMapperConfig, value: Vec<serde_json::Value>,
) -> Result<(), TranslateError> {
    let mut profiles = Vec::with_capacity(value.len());
    for raw in value {
        match parse_mapper_profile(raw) {
            Ok(p) => profiles.push(p),
            Err(reason) => {
                return Err(TranslateError::for_key("dogstatsd_mapper_profiles", reason));
            }
        }
    }
    mapper.dogstatsd_mapper_profiles = MapperProfileConfigs(profiles);
    Ok(())
}

/// Parses one mapper-profile JSON object into a [`MappingProfileConfig`].
fn parse_mapper_profile(raw: serde_json::Value) -> Result<MappingProfileConfig, String> {
    #[derive(serde::Deserialize)]
    struct RawMapping {
        #[serde(rename = "match")]
        metric_match: String,
        #[serde(default)]
        match_type: String,
        name: String,
        #[serde(default)]
        tags: std::collections::HashMap<String, String>,
    }
    #[derive(serde::Deserialize)]
    struct RawProfile {
        name: String,
        prefix: String,
        #[serde(default)]
        mappings: Vec<RawMapping>,
    }

    let parsed: RawProfile = serde_json::from_value(raw).map_err(|e| format!("invalid mapper profile: {e}"))?;
    Ok(MappingProfileConfig {
        name: parsed.name,
        prefix: parsed.prefix,
        mappings: parsed
            .mappings
            .into_iter()
            .map(|m| MetricMappingConfig {
                metric_match: m.metric_match,
                match_type: m.match_type,
                name: m.name,
                tags: m.tags,
            })
            .collect(),
    })
}

// ----- debug log -----

/// `dogstatsd_metrics_stats_enable` -> whether metric-level stats are enabled.
pub fn set_metrics_stats_enabled(debug: &mut DogStatsDDebugLogConfig, value: bool) {
    debug.metrics_stats_enabled = value;
}

/// `dogstatsd_logging_enabled` -> whether stats are written to a log file.
pub fn set_logging_enabled(debug: &mut DogStatsDDebugLogConfig, value: bool) {
    debug.logging_enabled = value;
}

/// `dogstatsd_log_file` -> debug-log file path (the component falls back to a default when empty).
pub fn set_log_file(debug: &mut DogStatsDDebugLogConfig, value: String) {
    debug.log_file = std::path::PathBuf::from(value);
}

/// `dogstatsd_log_file_max_size` -> debug-log rotation size.
///
/// Mirrors the original `ByteSize` parse (accepting either a size string like `10MB` or a raw byte
/// count). On a malformed value, records an error against the key and leaves the default in place.
pub fn set_log_file_max_size(debug: &mut DogStatsDDebugLogConfig, value: String) -> Result<(), TranslateError> {
    match value.parse::<ByteSize>() {
        Ok(size) => {
            debug.log_file_max_size = size;
            Ok(())
        }
        Err(reason) => Err(TranslateError::for_key("dogstatsd_log_file_max_size", reason)),
    }
}

/// `dogstatsd_log_file_max_rolls` -> number of rotated debug-log files retained.
pub fn set_log_file_max_rolls(debug: &mut DogStatsDDebugLogConfig, value: i64) {
    debug.log_file_max_rolls = value.max(0) as usize;
}

// ----- aggregate (histogram settings) -----

/// `histogram_aggregates` -> the histogram aggregate statistics to compute.
///
/// Mirrors the original parse: `count`/`sum`/`min`/`max`/`avg`/`median` map to the named variants.
/// The original keeps the configured percentiles separately; the aggregate transform here tracks
/// only the named aggregates, so percentile entries are ignored at this site.
pub fn set_histogram_aggregates(aggregate: &mut AggregateConfig, value: Vec<String>) {
    let mut statistics = Vec::with_capacity(value.len());
    for entry in value {
        match entry.as_str() {
            "count" => statistics.push(HistogramStatistic::Count),
            "sum" => statistics.push(HistogramStatistic::Sum),
            "min" => statistics.push(HistogramStatistic::Minimum),
            "max" => statistics.push(HistogramStatistic::Maximum),
            "avg" => statistics.push(HistogramStatistic::Average),
            "median" => statistics.push(HistogramStatistic::Median),
            _ => {}
        }
    }
    aggregate.hist_config.statistics = statistics;
}

/// `dogstatsd_flush_incomplete_buckets` -> whether open aggregation buckets are flushed on stop.
///
/// Mirrors the Agent semantic: flushing incomplete buckets corresponds to flushing the open
/// aggregation windows.
pub fn set_flush_incomplete_buckets(aggregate: &mut AggregateConfig, value: bool) {
    aggregate.flush_open_windows = value;
}

/// `histogram_copy_to_distribution` -> whether histograms are copied to distributions.
pub fn set_histogram_copy_to_distribution(aggregate: &mut AggregateConfig, value: bool) {
    aggregate.hist_config.copy_to_distribution = value;
}

/// `histogram_copy_to_distribution_prefix` -> prefix for copied distributions.
pub fn set_histogram_copy_to_distribution_prefix(aggregate: &mut AggregateConfig, value: String) {
    aggregate.hist_config.copy_to_distribution_prefix = value;
}

// ----- tag filterlist -----

/// `data_plane.dogstatsd.aggregator_tag_filter_cache_capacity` -> tag-filter context cache capacity.
pub fn set_tag_filter_cache_capacity(tag_filterlist: &mut TagFilterlistConfig, value: i64) {
    tag_filterlist.context_cache_capacity = value.max(0) as usize;
}

// ----- prefix filter -----

/// `statsd_metric_namespace` -> the metric prefix the filter scopes to.
pub fn set_prefix_metric_prefix(filter: &mut DogStatsDPrefixFilterConfig, value: String) {
    filter.metric_prefix = value;
}

/// `statsd_metric_namespace_blacklist` -> unconditionally blocked metric prefixes.
pub fn set_prefix_metric_prefix_blocklist(filter: &mut DogStatsDPrefixFilterConfig, value: Vec<String>) {
    filter.metric_prefix_blocklist = value;
}

/// `metric_filterlist` -> the prefix filter's allowed metric names/prefixes.
pub fn set_prefix_metric_filterlist(filter: &mut DogStatsDPrefixFilterConfig, value: Vec<String>) {
    filter.metric_filterlist = value;
}

/// `metric_filterlist_match_prefix` -> whether the prefix filter's filterlist matches as prefixes.
pub fn set_prefix_metric_filterlist_match_prefix(filter: &mut DogStatsDPrefixFilterConfig, value: bool) {
    filter.metric_filterlist_match_prefix = value;
}

/// `statsd_metric_blocklist` -> the prefix filter's blocked metric names/prefixes.
pub fn set_prefix_metric_blocklist(filter: &mut DogStatsDPrefixFilterConfig, value: Vec<String>) {
    filter.metric_blocklist = value;
}

/// `statsd_metric_blocklist_match_prefix` -> whether the prefix filter's blocklist matches as prefixes.
pub fn set_prefix_metric_blocklist_match_prefix(filter: &mut DogStatsDPrefixFilterConfig, value: bool) {
    filter.metric_blocklist_match_prefix = value;
}

// ----- post-aggregate filter -----

/// `metric_filterlist` -> the post-aggregate filter's allowed metric names/prefixes.
pub fn set_post_metric_filterlist(filter: &mut DogStatsDPostAggregateFilterConfig, value: Vec<String>) {
    filter.metric_filterlist = value;
}

/// `metric_filterlist_match_prefix` -> whether the post-aggregate filterlist matches as prefixes.
pub fn set_post_metric_filterlist_match_prefix(filter: &mut DogStatsDPostAggregateFilterConfig, value: bool) {
    filter.metric_filterlist_match_prefix = value;
}

/// `statsd_metric_blocklist` -> the post-aggregate filter's blocked metric names/prefixes.
pub fn set_post_metric_blocklist(filter: &mut DogStatsDPostAggregateFilterConfig, value: Vec<String>) {
    filter.metric_blocklist = value;
}

/// `statsd_metric_blocklist_match_prefix` -> whether the post-aggregate blocklist matches as prefixes.
pub fn set_post_metric_blocklist_match_prefix(filter: &mut DogStatsDPostAggregateFilterConfig, value: bool) {
    filter.metric_blocklist_match_prefix = value;
}

/// `histogram_aggregates` -> the post-aggregate filter's histogram aggregate names (raw strings).
pub fn set_post_histogram_aggregates(filter: &mut DogStatsDPostAggregateFilterConfig, value: Vec<String>) {
    filter.histogram_aggregates = value;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_cardinality_parses_and_errors() {
        let mut c = DogStatsDConfig::default();
        set_tag_cardinality(&mut c, "high".to_string()).expect("known cardinality");
        assert_eq!(c.origin_enrichment.tag_cardinality, OriginTagCardinality::High);
        assert!(set_tag_cardinality(&mut c, "bogus".to_string()).is_err());
    }

    #[test]
    fn mapper_profiles_parse_from_json() {
        let mut mapper = DogStatsDMapperConfig::default();
        let raw = vec![serde_json::json!({
            "name": "p1",
            "prefix": "foo",
            "mappings": [{ "match": "foo.*", "match_type": "wildcard", "name": "foo.bar", "tags": { "k": "$1" } }]
        })];
        set_mapper_profiles(&mut mapper, raw).expect("valid profiles");
        let profiles = &mapper.dogstatsd_mapper_profiles.0;
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "p1");
        assert_eq!(profiles[0].mappings[0].metric_match, "foo.*");
        assert_eq!(profiles[0].mappings[0].name, "foo.bar");
    }
}
