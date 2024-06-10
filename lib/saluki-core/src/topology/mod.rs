//! Topology building.

mod blueprint;
pub use self::blueprint::{BlueprintError, TopologyBlueprint};

mod built;
pub use self::built::BuiltTopology;

mod graph;
mod ids;
pub mod interconnect;

mod running;
pub use self::running::RunningTopology;

pub mod shutdown;

#[cfg(test)]
pub(super) mod test_util;

pub use self::ids::*;
