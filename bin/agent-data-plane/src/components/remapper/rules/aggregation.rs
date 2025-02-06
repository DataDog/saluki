use crate::components::remapper::RemapperRule;

pub fn get_aggregation_remappings() -> Vec<RemapperRule> {
    vec![
        RemapperRule::by_name_and_tags(
            "adp.context_resolver_active_contexts",
            &["resolver_id:dogstatsd"],
            "aggregator.dogstatsd_contexts",
        ),
        RemapperRule::by_name_and_tags(
            "adp.aggregate_active_contexts_by_type",
            &["component_id:dsd_agg"],
            "aggregator.dogstatsd_contexts_by_mtype",
        )
        .with_original_tags(["metric_type"]),
        RemapperRule::by_name_and_tags(
            "adp.aggregate_active_contexts_bytes_by_type",
            &["component_id:dsd_agg"],
            "aggregator.dogstatsd_contexts_bytes_by_mtype",
        )
        .with_original_tags(["metric_type"]),
        RemapperRule::by_name_and_tags(
            "adp.component_events_received_total",
            &["component_id:dsd_agg"],
            "aggregator.processed",
        )
        .with_additional_tags(["data_type:dogstatsd_metrics"]),
        RemapperRule::by_name_and_tags(
            "adp.aggregate_passthrough_metrics_total",
            &["component_id:dsd_agg"],
            "no_aggregation.processed",
        )
        .with_additional_tags(["state:ok"]),
    ]
}
