use super::RemapperRule;

pub fn get_dogstatsd_remappings() -> Vec<RemapperRule> {
    vec![
        // DogStatsD metrics.
        RemapperRule::by_name_and_tags(
            "adp.metric_filterlist_size",
            &["component_id:dsd_prefix_filter"],
            "datadog.agent.filterlist.size",
        )
        .with_help_text("Metric filter list size"),
        RemapperRule::by_name_and_tags(
            "adp.metric_filterlist_updates_total",
            &["component_id:dsd_prefix_filter"],
            "datadog.agent.filterlist.updates",
        )
        .with_help_text("Incremented when a reconfiguration of the metric filterlist happened"),
        RemapperRule::by_name_and_tags(
            "adp.dogstatsd_listener_filtered_points_total",
            &["component_id:dsd_prefix_filter"],
            "datadog.agent.dogstatsd.listener_filtered_points",
        )
        .with_help_text("How many points were filtered out"),
        RemapperRule::by_name_and_tags(
            "adp.dogstatsd_post_aggregate_filtered_metrics_total",
            &["component_id:dsd_post_agg_filter"],
            "datadog.agent.aggregator.dogstatsd_filtered_metrics",
        )
        .with_help_text("How many metrics were filtered in the time samplers"),
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
        .with_original_tags(["message_type"])
        .with_additional_tags(["state:ok"])
        .with_help_text("Count of service checks/events/metrics processed by dogstatsd"),
        RemapperRule::by_name_and_tags(
            "adp.component_errors_total",
            &["component_id:dsd_in", "error_type:decode"],
            "dogstatsd.processed",
        )
        .with_original_tags(["message_type"])
        .with_additional_tags(["state:error"])
        .with_help_text("Count of service checks/events/metrics processed by dogstatsd"),
    ]
}
