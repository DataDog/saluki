use crate::components::remapper::RemapperRule;

pub fn get_dogstatsd_remappings() -> Vec<RemapperRule> {
    vec![
        // DogStatsD metrics.
        //
        // TODO: We need to add `datadog.agent.dogstatsd.processed`, but with `state:error`, which the Agent captures by
        // metric type... but it's weird because it checks the prefix of the metric payload to determine metric vs event
        // vs service check, and anything that isn't a service check or metric it just assumes is a "metric"... so you
        // might have a bunch of "metric" type errors for straight up invalid payloads... and I guess it just feels
        // weird to me to categorize pure gibberish as a "metrics"-related decode error instead of just "hey, we got an
        // invalid payload". :shrug:
        RemapperRule::by_name_and_tags(
            "datadog.agent.adp.object_pool_acquired",
            &["pool_name:dsd_packet_bufs"],
            "datadog.agent.dogstatsd.packet_pool_get",
        ),
        RemapperRule::by_name_and_tags(
            "datadog.agent.adp.object_pool_released",
            &["pool_name:dsd_packet_bufs"],
            "datadog.agent.dogstatsd.packet_pool_put",
        ),
        RemapperRule::by_name_and_tags(
            "datadog.agent.adp.object_pool_in_use",
            &["pool_name:dsd_packet_bufs"],
            "datadog.agent.dogstatsd.packet_pool",
        ),
        RemapperRule::by_name_and_tags(
            "datadog.agent.adp.component_packets_received_total",
            &["component_id:dsd_in", "listener_type:udp"],
            "datadog.agent.dogstatsd.udp_packets",
        )
        .with_original_tags(["state"]),
        RemapperRule::by_name_and_tags(
            "datadog.agent.adp.component_bytes_received_total",
            &["component_id:dsd_in", "listener_type:udp"],
            "datadog.agent.dogstatsd.udp_packets_bytes",
        ),
        RemapperRule::by_name_and_tags(
            "datadog.agent.adp.component_packets_received_total",
            &["component_id:dsd_in", "listener_type:unixgram"],
            "datadog.agent.dogstatsd.uds_packets",
        )
        .with_remapped_tags([("listener_type", "transport")])
        .with_original_tags(["state"]),
        RemapperRule::by_name_and_tags(
            "datadog.agent.adp.component_bytes_received_total",
            &["component_id:dsd_in", "listener_type:unixgram"],
            "datadog.agent.dogstatsd.uds_packets_bytes",
        )
        .with_remapped_tags([("listener_type", "transport")]),
        RemapperRule::by_name_and_tags(
            "datadog.agent.adp.component_packets_received_total",
            &["component_id:dsd_in", "listener_type:unix"],
            "datadog.agent.dogstatsd.uds_packets",
        )
        .with_remapped_tags([("listener_type", "transport")])
        .with_original_tags(["state"]),
        RemapperRule::by_name_and_tags(
            "datadog.agent.adp.component_bytes_received_total",
            &["component_id:dsd_in", "listener_type:unix"],
            "datadog.agent.dogstatsd.uds_packets_bytes",
        )
        .with_remapped_tags([("listener_type", "transport")]),
        RemapperRule::by_name_and_tags(
            "datadog.agent.adp.component_connections_active",
            &["component_id:dsd_in", "listener_type:unix"],
            "datadog.agent.dogstatsd.uds_connections",
        )
        .with_remapped_tags([("listener_type", "transport")]),
        RemapperRule::by_name_and_tags(
            "datadog.agent.adp.component_events_received_total",
            &["component_id:dsd_in"],
            "datadog.agent.dogstatsd.processed",
        )
        .with_original_tags(["message_type"])
        .with_additional_tags(["state:ok"]),
    ]
}
