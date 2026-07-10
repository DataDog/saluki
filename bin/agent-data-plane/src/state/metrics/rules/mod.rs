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

/// Returns the `emitter` tag applied to all COAT telemetry that ADP exposes to the Datadog Agent.
///
/// The Datadog Agent's Remote Agent Registry attributes forwarded telemetry to its source via an
/// `emitter` label, falling back to the sanitized display name only when the metric doesn't already
/// carry one. Setting it explicitly here gives ADP telemetry a stable, self-declared identity. The
/// value mirrors the Agent-side sanitization (`sanitizeString`: lowercased, spaces replaced with
/// hyphens) of the app's full name so it matches the fallback the Agent would otherwise apply.
pub fn emitter_tag() -> String {
    let full_name = saluki_metadata::get_app_details().full_name();
    format!("emitter:{}", full_name.to_lowercase().replace(' ', "-"))
}
