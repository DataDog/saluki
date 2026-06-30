use saluki_core::observability::metrics::RemapperRule;

mod aggregation;
mod compat;
mod dogstatsd;
mod serializer;
mod transaction;

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

/// Returns remapper rules that expose ADP telemetry under Core Agent `go_expvar`-compatible names.
pub fn get_compat_remappings() -> Vec<RemapperRule> {
    self::compat::get_compat_remappings()
}
