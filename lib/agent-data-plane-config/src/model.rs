//! The ADP-native runtime configuration model: [`SalukiConfiguration`] and its component groups.
//!
//! [`SalukiConfiguration`] is the single output of configuration translation. It has two top-level
//! groups: [`ControlConfiguration`] (pipeline gates and topology shaping and
//! [`ComponentConfiguration`] (the per-domain component groups, each read by its owning component).
//!
//! Every group wrapper embeds the `saluki-component-config` leaf structs directly. The translator
//! writes witnessed values into these embedded leaf structs.
//!

// TODO: Only the DogStatsD source is modeled; component groups land with rapid strangler-fig PRs.

use crate::control::ControlConfiguration;
use crate::dogstatsd;

/// The complete ADP-native runtime configuration after translation.
///
/// First, configuration not appearing the Datadog config model, [`SalukiOnlyConfiguration`], is
/// used as a seed (since it cannot come from Datadog config). This is done with
/// [`SalukiOnlyConfiguration::seed`](crate::SalukiOnlyConfiguration::seed).
///
/// Then that seeded configuration is mutated by everything found in `DatadogConfiguration` by
/// driving it through the witness trait.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct SalukiConfiguration {
    /// Pipeline gates, topology-shaping decisions and data plane configuration. Consumed by the
    /// orchestration layers (config-system and the topology builder).
    pub control: ControlConfiguration,

    /// Per-domain component configuration groups. Each component receives its own slice from here.
    pub components: ComponentConfiguration,
}

/// The per-domain component configuration groups.
///
/// Each field is a group wrapper that embeds the `saluki-component-config` leaf structs for one
/// ownership domain. A component is handed only its own leaf slice (for example,
/// `&components.dogstatsd.source`), never the whole `ComponentConfiguration` or
/// [`SalukiConfiguration`].
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct ComponentConfiguration {
    /// DogStatsD source, mapper, aggregate, debug-log, and filter configuration.
    pub dogstatsd: dogstatsd::Config,
}
