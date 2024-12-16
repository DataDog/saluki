use crate::components::remapper::RemapperRule;

pub fn get_aggregation_remappings() -> Vec<RemapperRule> {
    vec![
        RemapperRule::by_name_and_tags(
            "adp.context_resolver_active_contexts",
            &["resolver_id:dogstatsd"],
            "aggregator.dogstatsd_contexts",
        ),
        RemapperRule::by_name_and_tags(
            "adp.component_events_received_total",
            &["component_id:dsd_agg"],
            "aggregator.processed",
        )
        .with_additional_tags(["data_type:dogstatsd_metrics"])
    ]
}
