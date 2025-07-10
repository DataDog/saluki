use std::num::NonZeroUsize;

use super::interconnect::Dispatchable;

/// Fundamental configuration for a topology.
pub trait TopologyConfiguration {
    /// Type of events that are dispatched from event-based components.
    type Events: Dispatchable;

    /// Type of payloads that are dispatched from payload-based components.
    type Payloads: Dispatchable;

    /// Returns the capacity of the interconnect between components.
    fn interconnect_capacity(&self) -> NonZeroUsize;
}
