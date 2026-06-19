//! DogStatsD-domain component configuration group.

/// DogStatsD-domain component configuration: source, mapper, aggregate, debug-log, and filter.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct Config {
    /// DogStatsD source configuration (listeners, parser/decoding options).
    pub source: saluki_component_config::dogstatsd::SourceConfig,
}
