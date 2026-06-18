use saluki_component_config::ListenAddress;
use serde_json::Value;

use super::*;

pub(super) fn consume_key(translator: &mut Translator, key: &str, value: Value) -> Option<Value> {
    match key {
        "bind_host" => {
            translator.dogstatsd_bind_host = optional_string_value(value);

            None
        }
        "dogstatsd_buffer_size" => {
            translator.native.components.dogstatsd.source.buffer_size = usize_value(value, 8192);

            None
        }
        "dogstatsd_capture_depth" => {
            translator.native.components.dogstatsd.source.capture_depth = usize_value(value, 1024);

            None
        }
        "dogstatsd_capture_path" => {
            translator.native.components.dogstatsd.source.capture_path = string_value(value);

            None
        }
        "dogstatsd_context_expiry_seconds" => {
            translator.native.components.dogstatsd.source.context_expiry_seconds = u64_value(value, 20);

            None
        }
        "dogstatsd_entity_id_precedence" => {
            translator
                .native
                .components
                .dogstatsd
                .source
                .origin
                .entity_id_precedence = bool_value(value);

            None
        }
        "dogstatsd_eol_required" => {
            translator.native.components.dogstatsd.source.eol_required = string_vec_value(value);

            None
        }
        "dogstatsd_flush_incomplete_buckets" => {
            translator.native.components.dogstatsd.aggregate.flush_open_windows = bool_value(value);

            None
        }
        "dogstatsd_no_aggregation_pipeline" => {
            let enabled = bool_value(value);
            translator
                .native
                .components
                .dogstatsd
                .source
                .no_aggregation_pipeline_support = enabled;
            translator
                .native
                .components
                .dogstatsd
                .aggregate
                .passthrough_timestamped_metrics = enabled;

            None
        }
        "dogstatsd_non_local_traffic" => {
            let enabled = bool_value(value);
            translator.native.components.dogstatsd.source.non_local_traffic = enabled;
            translator.dogstatsd_non_local_traffic = enabled;

            None
        }
        "dogstatsd_origin_detection" => {
            translator.native.components.dogstatsd.source.origin.enabled = bool_value(value);

            None
        }
        "dogstatsd_origin_detection_client" => {
            translator.native.components.dogstatsd.source.origin.client_detection = bool_value(value);

            None
        }
        "dogstatsd_origin_optout_enabled" => {
            translator.native.components.dogstatsd.source.origin.optout_enabled = bool_value(value);

            None
        }
        "dogstatsd_so_rcvbuf" => {
            translator.native.components.dogstatsd.source.socket_receive_buffer_size = usize_value(value, 0);

            None
        }
        "dogstatsd_stream_log_too_big" => {
            translator.native.components.dogstatsd.source.stream_log_too_big = bool_value(value);

            None
        }
        "dogstatsd_stream_socket" => {
            translator.native.components.dogstatsd.source.socket_stream_path = optional_string_value(value);

            None
        }
        "dogstatsd_string_interner_size" => {
            translator
                .native
                .components
                .dogstatsd
                .source
                .context_string_interner_entry_count = u64_value(value, 4096);

            None
        }
        "dogstatsd_tag_cardinality" => {
            translator.native.components.dogstatsd.source.origin.tag_cardinality = string_value(value);

            None
        }
        "enable_payloads.events" => {
            translator.native.components.dogstatsd.source.enable_payloads.events = bool_value(value);

            None
        }
        "enable_payloads.series" => {
            translator.native.components.dogstatsd.source.enable_payloads.series = bool_value(value);

            None
        }
        "enable_payloads.service_checks" => {
            translator
                .native
                .components
                .dogstatsd
                .source
                .enable_payloads
                .service_checks = bool_value(value);

            None
        }
        "enable_payloads.sketches" => {
            translator.native.components.dogstatsd.source.enable_payloads.sketches = bool_value(value);

            None
        }
        "origin_detection_unified" => {
            translator.native.components.dogstatsd.source.origin.unified_detection = bool_value(value);

            None
        }
        "provider_kind" => {
            translator.native.components.dogstatsd.source.provider_kind = string_value(value);

            None
        }
        "statsd_forward_host" => {
            translator.native.components.dogstatsd.source.statsd_forward_host = optional_string_value(value);

            None
        }
        "statsd_forward_port" => {
            translator.native.components.dogstatsd.source.statsd_forward_port = u16_value(value, 0);

            None
        }
        "dogstatsd_mapper_cache_size" => {
            translator.native.components.dogstatsd.mapper.cache_size = usize_value(value, 1000);

            None
        }
        "dogstatsd_mapper_profiles" => {
            translator.native.components.dogstatsd.mapper.profiles = array_value(value);

            None
        }
        "dogstatsd_logging_enabled" => {
            translator.native.components.dogstatsd.debug_log.logging_enabled = bool_value(value);

            None
        }
        "dogstatsd_log_file" => {
            translator.native.components.dogstatsd.debug_log.log_file = string_value(value);

            None
        }
        "dogstatsd_log_file_max_size" => {
            translator.native.components.dogstatsd.debug_log.log_file_max_size_bytes =
                u64_value(value, 10 * 1024 * 1024);

            None
        }
        "dogstatsd_log_file_max_rolls" => {
            translator.native.components.dogstatsd.debug_log.log_file_max_rolls = usize_value(value, 3);

            None
        }
        "dogstatsd_metrics_stats_enable" => {
            translator.native.components.dogstatsd.debug_log.metrics_stats_enabled = bool_value(value);

            None
        }
        "dogstatsd_port" => {
            let port = u16_value(value, 8125);
            translator.native.components.dogstatsd.source.udp_address = ListenAddress::Udp(format!("127.0.0.1:{port}"));

            None
        }
        "dogstatsd_socket" => {
            let socket = string_value(value);
            translator.native.components.dogstatsd.source.socket_path = (!socket.is_empty()).then_some(socket);

            None
        }
        "dogstatsd_tags" => {
            translator.native.components.dogstatsd.source.additional_tags = string_vec_value(value);
            None
        }
        "dogstatsd_string_interner_size_bytes" => {
            translator
                .native
                .components
                .dogstatsd
                .source
                .context_string_interner_size_bytes = u64_value(value, 2 * 1024 * 1024);

            None
        }
        "dogstatsd_cached_contexts_limit" => {
            translator.native.components.dogstatsd.source.cached_contexts_limit = usize_value(value, 500_000);

            None
        }
        "metric_filterlist" => {
            let values = string_vec_value(value);
            translator.native.components.dogstatsd.prefix_filter.metric_filterlist = values.clone();
            translator
                .native
                .components
                .dogstatsd
                .post_aggregate_filter
                .metric_filterlist = values;

            None
        }
        "metric_filterlist_match_prefix" => {
            let value = bool_value(value);
            translator
                .native
                .components
                .dogstatsd
                .prefix_filter
                .metric_filterlist_match_prefix = value;
            translator
                .native
                .components
                .dogstatsd
                .post_aggregate_filter
                .metric_filterlist_match_prefix = value;

            None
        }
        "statsd_metric_blocklist" => {
            let values = string_vec_value(value);
            translator.native.components.dogstatsd.prefix_filter.metric_blocklist = values.clone();
            translator
                .native
                .components
                .dogstatsd
                .post_aggregate_filter
                .metric_blocklist = values;

            None
        }
        "statsd_metric_blocklist_match_prefix" => {
            let value = bool_value(value);
            translator
                .native
                .components
                .dogstatsd
                .prefix_filter
                .metric_blocklist_match_prefix = value;
            translator
                .native
                .components
                .dogstatsd
                .post_aggregate_filter
                .metric_blocklist_match_prefix = value;

            None
        }
        "metric_tag_filterlist" => {
            translator.native.components.dogstatsd.tag_filterlist.entries = metric_tag_filter_entries(value);

            None
        }
        "data_plane.dogstatsd.aggregator_tag_filter_cache_capacity" => {
            translator.native.components.dogstatsd.tag_filterlist.cache_capacity = usize_value(value, 100_000);

            None
        }
        "histogram_aggregates" => {
            translator
                .native
                .components
                .dogstatsd
                .post_aggregate_filter
                .histogram_aggregates = string_vec_value(value);

            None
        }
        "histogram_copy_to_distribution" => {
            translator
                .native
                .components
                .dogstatsd
                .aggregate
                .histogram_copy_to_distribution = bool_value(value);

            None
        }
        "histogram_copy_to_distribution_prefix" => {
            translator
                .native
                .components
                .dogstatsd
                .aggregate
                .histogram_copy_to_distribution_prefix = string_value(value);

            None
        }
        "histogram_percentiles" => {
            translator
                .native
                .components
                .dogstatsd
                .post_aggregate_filter
                .histogram_percentiles = string_vec_value(value);

            None
        }
        "statsd_metric_namespace" => {
            translator.native.components.dogstatsd.prefix_filter.metric_prefix = string_value(value);

            None
        }
        "statsd_metric_namespace_blacklist" => {
            translator
                .native
                .components
                .dogstatsd
                .prefix_filter
                .metric_prefix_blocklist = string_vec_value(value);

            None
        }
        "statsd_metric_namespace_blocklist" => {
            translator
                .native
                .components
                .dogstatsd
                .prefix_filter
                .metric_prefix_blocklist = string_vec_value(value);

            None
        }
        _ => Some(value),
    }
}
