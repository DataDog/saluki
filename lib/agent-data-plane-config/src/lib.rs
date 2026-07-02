//! ADP-native configuration model: the typed target of configuration translation.
//!
//! This crate owns the domain-shaped model types that translation produces and that ADP runtime
//! code consumes: `SalukiConfiguration { control, shared, domains }`, `ControlConfiguration`,
//! `SharedConfiguration`, `DomainConfiguration` and its per-domain structs.
//!
//! It does not embed component config structs (those stay in `saluki-components`, built from this
//! model). It depends on neither the raw configuration map nor the Datadog source model, so a
//! consumer can depend on it without inheriting either.
//!
//! Every field is plain, source-agnostic data. There are no source key names in identifiers and no
//! source serde (these structs are serialized for the `/config/internal` view but never
//! deserialized from a source language; that is the source adapter's job).

use serde::Serialize;

pub mod control;
pub mod domains;
pub mod live;
pub mod shared;

pub use control::{ControlConfiguration, ListenAddress, Logging};
pub use domains::DomainConfiguration;
pub use live::Live;
pub use shared::SharedConfiguration;

/// The complete ADP-native runtime configuration after translation.
///
/// Two writers fill it: the Datadog witness `drive` (schema fields) and `seed` (Saluki-only
/// fields). They write disjoint fields. It is read by the orchestration layer (`control`) and by
/// components at topology assembly (`shared` and `domains`).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SalukiConfiguration {
    /// Read first: decides which pipelines/topology to build. Orchestration layer only.
    pub control: ControlConfiguration,
    /// Cross-cutting values consumed by more than one domain, each with a single home.
    pub shared: SharedConfiguration,
    /// Per-domain resolved config, grouped by ownership domain.
    pub domains: DomainConfiguration,
}
