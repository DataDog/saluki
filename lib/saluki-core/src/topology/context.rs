use memory_accounting::MemoryLimiter;
use saluki_health::HealthRegistry;
use tokio::runtime::Handle;

/// Topology context.
#[derive(Clone)]
pub struct TopologyContext {
    memory_limiter: MemoryLimiter,
    health_registry: HealthRegistry,
    global_thread_pool: Handle,
}

impl TopologyContext {
    /// Creates a new `TopologyContext`.
    pub fn new(memory_limiter: MemoryLimiter, health_registry: HealthRegistry, global_thread_pool: Handle) -> Self {
        Self {
            memory_limiter,
            health_registry,
            global_thread_pool,
        }
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
}
