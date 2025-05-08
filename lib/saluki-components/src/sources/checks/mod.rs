/// Checks source.
///
/// Listen to Autodiscovery events, schedule checks and emit results.
use std::sync::Arc;

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{sources::*, ComponentContext},
    topology::{shutdown::DynamicShutdownCoordinator, OutputDefinition},
};
use saluki_env::autodiscovery::{AutodiscoveryEvent, AutodiscoveryProvider};
use saluki_error::{generic_error, GenericError};
use saluki_event::DataType;
use serde::Deserialize;
use tokio::select;
use tokio::sync::broadcast::Receiver;
use tracing::{debug, info};

/// Configuration for the checks source.
#[derive(Deserialize)]
pub struct ChecksConfiguration {
    /// Autodiscovery provider to use.
    #[serde(skip)]
    autodiscovery_provider: Option<Arc<dyn AutodiscoveryProvider + Send + Sync>>,
}

impl ChecksConfiguration {
    /// Creates a new `ChecksConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }

    /// Sets the autodiscovery provider to use.
    pub fn with_autodiscovery_provider<A>(mut self, provider: A) -> Self
    where
        A: AutodiscoveryProvider + Send + Sync + 'static,
    {
        self.autodiscovery_provider = Some(Arc::new(provider));
        self
    }
}

#[async_trait]
impl SourceBuilder for ChecksConfiguration {
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        match &self.autodiscovery_provider {
            Some(autodiscovery) => {
                let event_rx = autodiscovery.subscribe().await;
                Ok(Box::new(ChecksSource { event_rx }))
            }
            None => Err(generic_error!("No autodiscovery provider configured.")),
        }
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: [OutputDefinition; 1] = [OutputDefinition::default_output(DataType::Metric)];

        &OUTPUTS
    }
}

impl MemoryBounds for ChecksConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<ChecksSource>("component struct");
    }
}

struct ChecksSource {
    event_rx: Receiver<AutodiscoveryEvent>,
}

#[async_trait]
impl Source for ChecksSource {
    async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let mut global_shutdown = context.take_shutdown_handle();
        let mut health = context.take_health_handle();

        let listener_shutdown_coordinator = DynamicShutdownCoordinator::default();

        let mut event_rx = self.event_rx;

        tokio::spawn(async move {
            loop {
                while let Ok(event) = event_rx.recv().await {
                    match event {
                        AutodiscoveryEvent::Schedule { config } => {
                            info!("Received schedule check config: {:?}", config);
                        }
                        AutodiscoveryEvent::Unscheduled { config_id } => {
                            info!("Received unscheduled check config: {:?}", config_id);
                        }
                    }
                }
            }
        });

        health.mark_ready();
        info!("Checks source started.");

        // Wait for the global shutdown signal, then notify listeners to shutdown.
        //
        // We also handle liveness here, which doesn't really matter for _this_ task, since the real work is happening
        // in the listeners, but we need to satisfy the health checker.
        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break
                },
                _ = health.live() => continue,
            }
        }

        info!("Stopping Checks source...");

        listener_shutdown_coordinator.shutdown().await;

        info!("Checks source stopped.");

        Ok(())
    }
}
