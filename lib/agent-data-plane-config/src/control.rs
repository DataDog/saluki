//! [`ControlConfiguration`]: pipeline gates, topology-shaping, and data plane decisions.
//!
//! Control config is read by the config system and the data plane, not by components.

/// Pipeline gates and topology-shaping decisions for ADP.
///
/// Read first, before [`ComponentConfiguration`](crate::ComponentConfiguration): it decides which
/// pipelines and topology to build. Consumed by the orchestration layer only.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct ControlConfiguration {
    /// Whether the data plane runs at all.
    pub enabled: bool,
}
