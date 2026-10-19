//! ADP's configuration, plugged into the shared remote agent runtime.
//!
//! These are thin wrappers: the runtime defines the traits, and ADP's configuration types live in other crates, so
//! the orphan rule requires a local type to implement one for the other.

use std::future::Future;

use agent_data_plane_config_system::ConfigurationUpdates;
use datadog_agent_runtime::control_plane::ConfigUpdatesRunner;
use saluki_error::GenericError;

/// Applies the configuration updates that ADP's configuration system receives from the Datadog Agent.
pub struct AdpConfigUpdates(ConfigurationUpdates);

impl AdpConfigUpdates {
    /// Wraps the given configuration updates.
    pub fn new(updates: ConfigurationUpdates) -> Self {
        Self(updates)
    }
}

impl ConfigUpdatesRunner for AdpConfigUpdates {
    fn run(&self) -> impl Future<Output = Result<(), GenericError>> + Send + 'static {
        let updates = self.0.run();
        async move { updates.await.map_err(GenericError::from) }
    }
}
