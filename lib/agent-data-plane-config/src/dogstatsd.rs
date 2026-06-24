//! DogStatsD-domain component configuration group.

/// Configuration for the DogStatsD *domain*: the family of components whose Datadog config keys
/// share the `dogstatsd_*` namespace (source, mapper, aggregate, debug-log, and filter).
///
/// This is an umbrella holding one config slice per family member, not the config of any single
/// component. It currently carries only `source`, because the source is the only member cut over to
/// the translated-config system so far; the mapper, aggregate, debug-log, and filter slices are
/// added here as those components are cut over.
///
/// "DogStatsD" names both this domain family and the source component specifically. This `Config` is
/// the family. The source component's own config is `SourceConfig` (embedded below, defined in
/// `saluki-component-config`), which the source builder `DogStatsDConfiguration` (in
/// `saluki-components`) also embeds by value.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct Config {
    /// DogStatsD source configuration (listeners, parser/decoding options).
    pub source: saluki_component_config::dogstatsd::SourceConfig,

    /// DogStatsD prefix and listener-side metric filter configuration.
    pub prefix_filter: saluki_component_config::dogstatsd::PrefixFilterConfig,
}
