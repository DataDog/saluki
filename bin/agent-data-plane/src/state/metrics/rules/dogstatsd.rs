use super::RemapperRule;

pub fn get_dogstatsd_remappings() -> Vec<RemapperRule> {
    vec![
        // DogStatsD metrics.
        RemapperRule::by_name_and_tags(
            "adp.metric_filterlist_size",
            &["component_id:dsd_prefix_filter"],
            "filterlist.size",
        ),
        RemapperRule::by_name_and_tags(
            "adp.metric_filterlist_updates_total",
            &["component_id:dsd_prefix_filter"],
            "filterlist.updates",
        ),
        RemapperRule::by_name_and_tags(
            "adp.dogstatsd_listener_filtered_points_total",
            &["component_id:dsd_prefix_filter"],
            "dogstatsd.listener_filtered_points",
        ),
        RemapperRule::by_name_and_tags(
            "adp.dogstatsd_post_aggregate_filtered_metrics_total",
            &["component_id:dsd_post_agg_filter"],
            "aggregator.dogstatsd_filtered_metrics",
        ),
        RemapperRule::by_name_and_tags(
            "adp.tag_filterlist_size",
            &["component_id:dsd_tag_filterlist"],
            "tag_filterlist.size",
        ),
        RemapperRule::by_name_and_tags(
            "adp.tag_filterlist_updates_total",
            &["component_id:dsd_tag_filterlist"],
            "tag_filterlist.updates",
        ),
        RemapperRule::by_name_and_tags(
            "adp.object_pool_acquired",
            &["pool_name:dsd_packet_bufs"],
            "dogstatsd.packet_pool_get",
        ),
        RemapperRule::by_name_and_tags(
            "adp.object_pool_released",
            &["pool_name:dsd_packet_bufs"],
            "dogstatsd.packet_pool_put",
        ),
        RemapperRule::by_name_and_tags(
            "adp.object_pool_in_use",
            &["pool_name:dsd_packet_bufs"],
            "dogstatsd.packet_pool",
        ),
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
        .with_additional_tags(["state:ok"]),
        RemapperRule::by_name_and_tags(
            "adp.component_errors_total",
            &["component_id:dsd_in", "error_type:decode"],
            "dogstatsd.processed",
        )
        .with_original_tags(["message_type"])
        .with_additional_tags(["state:error"]),
    ]
}
