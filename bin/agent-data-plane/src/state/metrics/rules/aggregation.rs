use super::RemapperRule;

const NO_AGG_SPLIT_COMPONENT_TAG: &str = "component_id:dsd_no_agg_split";

pub fn get_aggregation_remappings() -> Vec<RemapperRule> {
    vec![
        RemapperRule::by_name_and_tags(
            "adp.context_resolver_active_contexts",
            &["resolver_id:dsd_in/dsd/primary"],
            "aggregator.dogstatsd_contexts",
        )
        .with_help_text("Count the number of dogstatsd contexts in the aggregator"),
        RemapperRule::by_name_and_tags(
            "adp.aggregate_active_contexts_by_type",
            &["component_id:dsd_agg"],
            "aggregator.dogstatsd_contexts_by_mtype",
        )
        .with_original_tags(["metric_type"])
        .with_help_text("Count the number of dogstatsd contexts in the aggregator, by metric type"),
        RemapperRule::by_name_and_tags(
            "adp.aggregate_active_contexts_bytes_by_type",
            &["component_id:dsd_agg"],
            "aggregator.dogstatsd_contexts_bytes_by_mtype",
        )
        .with_original_tags(["metric_type"])
        .with_help_text("Estimated count of bytes taken by contexts in the aggregator, by metric type"),
        RemapperRule::by_name_and_tags(
            "adp.component_events_received_total",
            &["component_id:dsd_agg"],
            "aggregator.processed",
        )
        .with_additional_tags(["data_type:dogstatsd_metrics"])
        .with_help_text("Amount of metrics/services_checks/events processed by the aggregator"),
        // The passthrough counters are emitted by `dsd_no_agg_split`, not `dsd_agg`, since the timestamp-based
        // split moved out of the aggregate transform.
        RemapperRule::by_name_and_tags(
            "adp.aggregate_passthrough_metrics_total",
            &[NO_AGG_SPLIT_COMPONENT_TAG],
            "no_aggregation.processed",
        )
        .with_additional_tags(["state:ok"])
        .with_help_text("Count the number of samples processed by the no-aggregation pipeline worker"),
        RemapperRule::by_name_and_tags(
            "adp.aggregate_passthrough_flushes_total",
            &[NO_AGG_SPLIT_COMPONENT_TAG],
            "no_aggregation.flush",
        )
        .with_help_text("Count the number of flushes done by the no-aggregation pipeline worker"),
        RemapperRule::by_name_and_tags(
            "adp.aggregate_flushed_total",
            &["component_id:dsd_agg"],
            "aggregator.flush",
        )
        .with_original_tags(["data_type"])
        .with_help_text("Number of metrics/service checks/events flushed"),
    ]
}
