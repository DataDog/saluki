use std::sync::Arc;

use tokio::runtime::Handle;

use crate::accounting::MemoryLimiter;
use crate::health::HealthRegistry;
use crate::runtime::state::DataspaceRegistry;

/// Topology context.
#[derive(Clone)]
pub struct TopologyContext {
    topology_name: Arc<str>,
    memory_limiter: MemoryLimiter,
    health_registry: HealthRegistry,
    global_thread_pool: Handle,
    dataspace: DataspaceRegistry,
}

impl TopologyContext {
    /// Creates a new `TopologyContext`.
    pub fn new(
        topology_name: Arc<str>, memory_limiter: MemoryLimiter, health_registry: HealthRegistry,
        global_thread_pool: Handle, dataspace: DataspaceRegistry,
    ) -> Self {
        Self {
            topology_name,
            memory_limiter,
            health_registry,
            global_thread_pool,
            dataspace,
        }
    }

    /// Gets the name of the topology this context belongs to.
    pub fn topology_name(&self) -> &str {
        &self.topology_name
    }

    /// Gets a reference to the memory limiter.
    pub fn memory_limiter(&self) -> &MemoryLimiter {
        &self.memory_limiter
    }

    /// Gets a reference to the health registry.
    pub fn health_registry(&self) -> &HealthRegistry {
        &self.health_registry
    }

    /// Gets a reference to the global thread pool.
    pub fn global_thread_pool(&self) -> &Handle {
        &self.global_thread_pool
    }

    /// Gets a reference to the dataspace registry.
    pub fn dataspace(&self) -> &DataspaceRegistry {
        &self.dataspace
    }
}
