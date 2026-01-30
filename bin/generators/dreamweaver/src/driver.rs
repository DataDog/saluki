//! Driver for orchestrating the simulation.

use saluki_error::{ErrorContext as _, GenericError};
use tracing::info;

use crate::config::Config;
use crate::simulation::SimulationEngine;

/// The driver that orchestrates the simulation.
pub struct Driver {
    config: Config,
}

impl Driver {
    /// Creates a new driver with the given configuration.
    pub fn new(config: Config) -> Result<Self, GenericError> {
        info!(
            "Loaded configuration with {} templates and {} services",
            config.templates.len(),
            config.services.len()
        );

        Ok(Self { config })
    }

    /// Runs the simulation.
    pub async fn run(self) -> Result<(), GenericError> {
        info!("Initializing simulation engine...");

        let engine = SimulationEngine::new(&self.config)
            .await
            .error_context("Failed to initialize simulation engine")?;

        info!("Starting workload generation...");

        engine
            .run(&self.config.workload)
            .await
            .error_context("Simulation failed")?;

        info!("Workload generation complete.");
        Ok(())
    }
}
