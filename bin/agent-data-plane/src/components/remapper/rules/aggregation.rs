use crate::components::remapper::RemapperRule;

pub fn get_aggregation_remappings() -> Vec<RemapperRule> {
    vec![
        // Aggregation metrics. (placeholder so we can have the below TODO)
        //
        // TODO: We need to add two new metrics to the DD Metrics destination to properly capture the intent of
        // `datadog.agent.aggregator.flush`, which is the number of series/sketches that we managed to successfully
        // serialize. The metric doesn't concern itself with whether or not those payloads actually make it anywhere,
        // just if they were sent out of the aggregator and serialized correctly.
    ]
}
