use super::RemapperRule;

pub fn get_dogstatsd_remappings() -> Vec<RemapperRule> {
    vec![
        // DogStatsD metrics.
        RemapperRule::by_name_and_tags(
            "adp.metric_filterlist_size",
            &["component_id:dsd_prefix_filter"],
            "filterlist.size",
        )
        .with_help_text("Metric filter list size"),
        RemapperRule::by_name_and_tags(
            "adp.metric_filterlist_updates_total",
            &["component_id:dsd_prefix_filter"],
            "filterlist.updates",
        )
        .with_help_text("Incremented when a reconfiguration of the metric filterlist happened"),
        RemapperRule::by_name_and_tags(
            "adp.dogstatsd_listener_filtered_points_total",
            &["component_id:dsd_prefix_filter"],
            "dogstatsd.listener_filtered_points",
        )
        .with_help_text("How many points were filtered out"),
        RemapperRule::by_name_and_tags(
            "adp.dogstatsd_post_aggregate_filtered_metrics_total",
            &["component_id:dsd_post_agg_filter"],
            "aggregator.dogstatsd_filtered_metrics",
        )
        .with_help_text("How many metrics were filtered in the time samplers"),
        RemapperRule::by_name_and_tags(
            "adp.tag_filterlist_size",
            &["component_id:dsd_tag_filterlist"],
            "tag_filterlist.size",
        )
        .with_help_text("Tag filter list size"),
        RemapperRule::by_name_and_tags(
            "adp.tag_filterlist_updates_total",
            &["component_id:dsd_tag_filterlist"],
            "tag_filterlist.updates",
        )
        .with_help_text("Incremented when a reconfiguration of the tag filterlist happened"),
        RemapperRule::by_name_and_tags(
            "adp.tag_filterlist_tags_filtered_total",
            &["component_id:dsd_tag_filterlist"],
            "aggregator.filtered_tags",
        )
        .with_help_text("How many tags were filtered from a metric sample"),
        RemapperRule::by_name_and_tags(
            "adp.cache_hits_total",
            &["cache_id:tag_filterlist/context_cache"],
            "aggregator.filtered_tags_cache_hit",
        )
        .with_help_text("How many times we hit the cache on filtering tags"),
        RemapperRule::by_name_and_tags(
            "adp.cache_misses_total",
            &["cache_id:tag_filterlist/context_cache"],
            "aggregator.filtered_tags_cache_miss",
        )
        .with_help_text("How many times we missed the cache on filtering tags"),
        RemapperRule::by_name_and_tags(
            "adp.cache_items_evicted_total",
            &["cache_id:tag_filterlist/context_cache"],
            "aggregator.filtered_tags_cache_evict",
        )
        .with_help_text("How many times an entry was evicted from the tag filter cache"),
        RemapperRule::by_name_and_tags(
            "adp.object_pool_acquired",
            &["pool_name:dsd_packet_bufs"],
            "dogstatsd.packet_pool_get",
        )
        .with_help_text("Count of get done in the packet pool"),
        RemapperRule::by_name_and_tags(
            "adp.object_pool_released",
            &["pool_name:dsd_packet_bufs"],
            "dogstatsd.packet_pool_put",
        )
        .with_help_text("Count of put done in the packet pool"),
        RemapperRule::by_name_and_tags(
            "adp.object_pool_in_use",
            &["pool_name:dsd_packet_bufs"],
            "dogstatsd.packet_pool",
        )
        .with_help_text("Usage of the packet pool in dogstatsd"),
        RemapperRule::by_name_and_tags(
            "adp.component_packets_received_total",
            &["component_id:dsd_in", "listener_type:udp"],
            "dogstatsd.udp_packets",
        )
        .with_original_tags(["state"]),
        RemapperRule::by_name_and_tags(
            "adp.component_bytes_received_total",
            &["component_id:dsd_in", "listener_type:udp"],
            "dogstatsd.udp_packets_bytes",
        ),
        RemapperRule::by_name_and_tags(
            "adp.component_packets_received_total",
            &["component_id:dsd_in", "listener_type:unixgram"],
            "dogstatsd.uds_packets",
        )
        .with_remapped_tags([("listener_type", "transport")])
        .with_original_tags(["state"]),
        RemapperRule::by_name_and_tags(
            "adp.component_bytes_received_total",
            &["component_id:dsd_in", "listener_type:unixgram"],
            "dogstatsd.uds_packets_bytes",
        )
        .with_remapped_tags([("listener_type", "transport")]),
        RemapperRule::by_name_and_tags(
            "adp.component_packets_received_total",
            &["component_id:dsd_in", "listener_type:unix"],
            "dogstatsd.uds_packets",
        )
        .with_remapped_tags([("listener_type", "transport")])
        .with_original_tags(["state"]),
        RemapperRule::by_name_and_tags(
            "adp.component_bytes_received_total",
            &["component_id:dsd_in", "listener_type:unix"],
            "dogstatsd.uds_packets_bytes",
        )
        .with_remapped_tags([("listener_type", "transport")]),
        RemapperRule::by_name_and_tags(
            "adp.component_connections_active",
            &["component_id:dsd_in", "listener_type:unix"],
            "dogstatsd.uds_connections",
        )
        .with_remapped_tags([("listener_type", "transport")]),
        RemapperRule::by_name_and_tags(
            "adp.component_events_received_total",
            &["component_id:dsd_in"],
            "dogstatsd.processed",
        )
        .with_original_tags(["message_type", "origin"])
        .with_additional_tags(["state:ok"])
        .with_help_text("Count of service checks/events/metrics processed by dogstatsd"),
        RemapperRule::by_name_and_tags(
            "adp.component_errors_total",
            &["component_id:dsd_in", "error_type:decode"],
            "dogstatsd.processed",
        )
        .with_original_tags(["message_type", "origin"])
        .with_additional_tags(["state:error"])
        .with_help_text("Count of service checks/events/metrics processed by dogstatsd"),
        // DogStatsD client telemetry. These gauges mirror the post-aggregation metric stream without client-provided
        // dimensions, so COAT receives the same values that ADP sends to customer intake while retaining bounded
        // cardinality.
        RemapperRule::by_name(
            "adp.dogstatsd_client_telemetry_metrics",
            "datadog.dogstatsd.client.metrics",
        ),
        RemapperRule::by_name(
            "adp.dogstatsd_client_telemetry_metrics_by_type",
            "datadog.dogstatsd.client.metrics_by_type",
        ),
        RemapperRule::by_name(
            "adp.dogstatsd_client_telemetry_events",
            "datadog.dogstatsd.client.events",
        ),
        RemapperRule::by_name(
            "adp.dogstatsd_client_telemetry_service_checks",
            "datadog.dogstatsd.client.service_checks",
        ),
        RemapperRule::by_name(
            "adp.dogstatsd_client_telemetry_metric_dropped_on_receive",
            "datadog.dogstatsd.client.metric_dropped_on_receive",
        ),
        RemapperRule::by_name(
            "adp.dogstatsd_client_telemetry_bytes_sent",
            "datadog.dogstatsd.client.bytes_sent",
        ),
        RemapperRule::by_name(
            "adp.dogstatsd_client_telemetry_bytes_dropped",
            "datadog.dogstatsd.client.bytes_dropped",
        ),
        RemapperRule::by_name(
            "adp.dogstatsd_client_telemetry_bytes_dropped_queue",
            "datadog.dogstatsd.client.bytes_dropped_queue",
        ),
        RemapperRule::by_name(
            "adp.dogstatsd_client_telemetry_bytes_dropped_writer",
            "datadog.dogstatsd.client.bytes_dropped_writer",
        ),
        RemapperRule::by_name(
            "adp.dogstatsd_client_telemetry_packets_sent",
            "datadog.dogstatsd.client.packets_sent",
        ),
        RemapperRule::by_name(
            "adp.dogstatsd_client_telemetry_packets_dropped",
            "datadog.dogstatsd.client.packets_dropped",
        ),
        RemapperRule::by_name(
            "adp.dogstatsd_client_telemetry_packets_dropped_queue",
            "datadog.dogstatsd.client.packets_dropped_queue",
        ),
        RemapperRule::by_name(
            "adp.dogstatsd_client_telemetry_packets_dropped_writer",
            "datadog.dogstatsd.client.packets_dropped_writer",
        ),
        RemapperRule::by_name(
            "adp.dogstatsd_client_telemetry_aggregated_context",
            "datadog.dogstatsd.client.aggregated_context",
        ),
        RemapperRule::by_name(
            "adp.dogstatsd_client_telemetry_aggregated_context_by_type",
            "datadog.dogstatsd.client.aggregated_context_by_type",
        ),
    ]
}
