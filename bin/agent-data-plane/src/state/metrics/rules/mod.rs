use saluki_core::observability::metrics::RemapperRule;

mod aggregation;
mod dogstatsd;
mod serializer;
mod transaction;

/// The tag the API key validation HTTP client puts on all of its telemetry.
///
/// Validation requests aren't forwarder transactions, and the Core Agent doesn't count them as such. The client emits
/// the same HTTP metrics as the forwarder's client, distinguished only by this tag, so everything that reads those
/// metrics must reject it. The value is owned by `saluki-components`, which applies it.
pub(crate) const API_KEY_VALIDATION_TAG: &str = saluki_components::forwarders::API_KEY_VALIDATION_PURPOSE_TAG;

/// Returns the list of remapper rules relevant to metrics we send to the Datadog Agent via Remote Agent Registry (RAR).
///
/// These metrics are used specifically to ensure continuity of metrics that are emitted via COAT, regardless of whether
/// or not ADP is enabled.
pub fn get_datadog_agent_remappings() -> Vec<RemapperRule> {
    let mut rules = Vec::new();
    rules.extend(self::dogstatsd::get_dogstatsd_remappings());
    rules.extend(self::aggregation::get_aggregation_remappings());
    rules.extend(self::transaction::get_transaction_remappings());
    rules.extend(self::serializer::get_serializer_remappings());
    rules
}
