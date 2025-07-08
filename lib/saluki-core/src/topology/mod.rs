//! Topology building.

use memory_accounting::ComponentRegistry;

mod blueprint;
pub use self::blueprint::{BlueprintError, TopologyBlueprint};

mod built;
pub use self::built::BuiltTopology;

mod config;
pub use self::config::TopologyConfiguration;

mod graph;
mod ids;
pub mod interconnect;

mod running;
pub use self::running::RunningTopology;

pub mod shutdown;

#[cfg(test)]
pub(super) mod test_util;

pub use self::ids::*;

pub(super) struct RegisteredComponent<T> {
    component: T,
    component_registry: ComponentRegistry,
}

impl<T> RegisteredComponent<T> {
    fn new(component: T, component_registry: ComponentRegistry) -> Self {
        Self {
            component,
            component_registry,
        }
    }
    fn into_parts(self) -> (T, ComponentRegistry) {
        (self.component, self.component_registry)
    }
}
