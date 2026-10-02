use super::RemapperRule;

const NO_AGG_SPLIT_COMPONENT_TAG: &str = "component_id:dsd_no_agg_split";
const EVENTS_ENCODER_COMPONENT_TAG: &str = "component_id:dd_events_encode";
const SERVICE_CHECKS_ENCODER_COMPONENT_TAG: &str = "component_id:dd_service_checks_encode";

const FLUSH_HELP_TEXT: &str = "Number of metrics/service checks/events flushed";
const FLUSH_COUNT_HELP_TEXT: &str = "Number of items handled by the last flush, by flush type";
const FLUSH_TIME_HELP_TEXT: &str = "Duration in nanoseconds of the last flush, by flush type";

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
        // Events and service checks don't pass through an aggregator in ADP, so their encoders stand in for it.
        RemapperRule::by_name_and_tags(
            "adp.component_events_received_total",
            &[EVENTS_ENCODER_COMPONENT_TAG],
            "aggregator.processed",
        )
        .with_additional_tags(["data_type:events"])
        .with_help_text("Amount of metrics/services_checks/events processed by the aggregator"),
        RemapperRule::by_name_and_tags(
            "adp.component_events_received_total",
            &[SERVICE_CHECKS_ENCODER_COMPONENT_TAG],
            "aggregator.processed",
        )
        .with_additional_tags(["data_type:service_checks"])
        .with_help_text("Amount of metrics/services_checks/events processed by the aggregator"),
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
        .with_help_text(FLUSH_HELP_TEXT),
        RemapperRule::by_name_and_tags(
            "adp.aggregate_flushes_total",
            &["component_id:dsd_agg"],
            "aggregator.number_of_flush",
        )
        .with_help_text("Number of flushes done by the aggregator"),
        RemapperRule::by_name_and_tags(
            "adp.aggregate_last_flush_count",
            &["component_id:dsd_agg"],
            "aggregator.flush_count",
        )
        .with_remapped_tags([("data_type", "flush_type")])
        .with_help_text(FLUSH_COUNT_HELP_TEXT),
        // There's no ADP equivalent of the Core Agent's separate `metric_sketch` and `checks_metric_sample` flush
        // timings, which include serialization: ADP encodes metrics in a separate component after the aggregator flush.
        RemapperRule::by_name_and_tags(
            "adp.aggregate_last_flush_duration_nanoseconds",
            &["component_id:dsd_agg"],
            "aggregator.flush_time",
        )
        .with_additional_tags(["flush_type:main"])
        .with_help_text(FLUSH_TIME_HELP_TEXT),
        // As with `aggregator.processed`, the events and service checks encoders stand in for the aggregator's
        // event and service check flushes.
        RemapperRule::by_name_and_tags(
            "adp.encoder_flushed_events_total",
            &[EVENTS_ENCODER_COMPONENT_TAG],
            "aggregator.flush",
        )
        .with_additional_tags(["data_type:events"])
        .with_help_text(FLUSH_HELP_TEXT),
        RemapperRule::by_name_and_tags(
            "adp.encoder_flushed_events_total",
            &[SERVICE_CHECKS_ENCODER_COMPONENT_TAG],
            "aggregator.flush",
        )
        .with_additional_tags(["data_type:service_checks"])
        .with_help_text(FLUSH_HELP_TEXT),
        RemapperRule::by_name_and_tags(
            "adp.encoder_last_flush_events",
            &[EVENTS_ENCODER_COMPONENT_TAG],
            "aggregator.flush_count",
        )
        .with_additional_tags(["flush_type:events"])
        .with_help_text(FLUSH_COUNT_HELP_TEXT),
        RemapperRule::by_name_and_tags(
            "adp.encoder_last_flush_events",
            &[SERVICE_CHECKS_ENCODER_COMPONENT_TAG],
            "aggregator.flush_count",
        )
        .with_additional_tags(["flush_type:service_checks"])
        .with_help_text(FLUSH_COUNT_HELP_TEXT),
        RemapperRule::by_name_and_tags(
            "adp.encoder_last_flush_duration_nanoseconds",
            &[EVENTS_ENCODER_COMPONENT_TAG],
            "aggregator.flush_time",
        )
        .with_additional_tags(["flush_type:event"])
        .with_help_text(FLUSH_TIME_HELP_TEXT),
        RemapperRule::by_name_and_tags(
            "adp.encoder_last_flush_duration_nanoseconds",
            &[SERVICE_CHECKS_ENCODER_COMPONENT_TAG],
            "aggregator.flush_time",
        )
        .with_additional_tags(["flush_type:service_check"])
        .with_help_text(FLUSH_TIME_HELP_TEXT),
    ]
}
