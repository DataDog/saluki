//! Builds component inputs from the configuration model.
//!
//! Components define their own input types and never name the model. The functions here copy model values into those
//! types, calling model methods where the model resolves a value, such as an endpoint chosen by provenance. They add
//! no defaults, parsing, or validation: those belong to the configuration layer.
//!
//! Neither the model types nor the component types are local to the binary, so the orphan rule forbids `From`
//! implementations between them. Conversions are free functions.

mod datadog;
mod dogstatsd;
mod otlp;
mod static_tags;
mod traces;

use agent_data_plane_config::domains::dogstatsd::OriginTagCardinality as ModelOriginTagCardinality;
use saluki_core::data_model::origin::OriginTagCardinality;

pub(crate) use self::datadog::{datadog_metrics_configuration, forwarder_configuration, mrf_configuration};
pub(crate) use self::dogstatsd::{metric_mirroring, metric_name_filter, tag_filter_entries, tag_value_allowlist};
pub(crate) use self::otlp::{otlp_context_settings, otlp_metrics_settings, otlp_receiver, otlp_traces_settings};
pub(crate) use self::static_tags::resolve_static_tags;
pub(crate) use self::traces::{
    apm_stats_configuration, obfuscation_configuration, ottl_error_mode, trace_configuration,
    trace_sampler_configuration,
};

/// Maps the configured tag cardinality onto the cardinality the context resolver understands.
pub(crate) fn origin_tag_cardinality(cardinality: ModelOriginTagCardinality) -> OriginTagCardinality {
    match cardinality {
        ModelOriginTagCardinality::None => OriginTagCardinality::None,
        ModelOriginTagCardinality::Low => OriginTagCardinality::Low,
        ModelOriginTagCardinality::Orchestrator => OriginTagCardinality::Orchestrator,
        ModelOriginTagCardinality::High => OriginTagCardinality::High,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_tag_cardinality_maps_every_variant() {
        for (configured, expected) in [
            (ModelOriginTagCardinality::None, OriginTagCardinality::None),
            (ModelOriginTagCardinality::Low, OriginTagCardinality::Low),
            (
                ModelOriginTagCardinality::Orchestrator,
                OriginTagCardinality::Orchestrator,
            ),
            (ModelOriginTagCardinality::High, OriginTagCardinality::High),
        ] {
            assert_eq!(expected, origin_tag_cardinality(configured), "{configured:?}");
        }
    }
}
