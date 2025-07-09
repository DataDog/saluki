use std::num::NonZeroUsize;

use saluki_core::{
    data_model::payload::Payload,
    topology::{interconnect::FixedSizeEventBuffer, TopologyConfiguration},
};

pub struct BasicTopologyConfiguration {
    interconnect_capacity: NonZeroUsize,
}

impl BasicTopologyConfiguration {
    /// Create a new "primary" topology configuration.
    ///
    /// This is used for the topology handling all main data flows in ADP: metrics, etc.
    pub const fn primary() -> Self {
        Self {
            interconnect_capacity: NonZeroUsize::new(128).unwrap(),
        }
    }

    /// Create a new "internal observability" topology configuration.
    ///
    /// This is used for the topology handling internal observability of ADP itself.
    pub const fn internal_observability() -> Self {
        Self {
            interconnect_capacity: NonZeroUsize::new(4).unwrap(),
        }
    }
}

impl TopologyConfiguration for BasicTopologyConfiguration {
    type Events = FixedSizeEventBuffer<1024>;
    type Payloads = Payload;

    fn interconnect_capacity(&self) -> NonZeroUsize {
        self.interconnect_capacity
    }
}
