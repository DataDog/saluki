/// Checks source.
///
/// Listen to Autodiscovery events, schedule checks and emit results.
use std::sync::Arc;

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::task::spawn_traced_named;
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{sources::*, ComponentContext},
    topology::{
        shutdown::{DynamicShutdownCoordinator, DynamicShutdownHandle},
        OutputDefinition,
    },
};
use saluki_env::autodiscovery::{AutodiscoveryEvent, AutodiscoveryProvider, Config};
use saluki_error::{generic_error, GenericError};
use saluki_event::DataType;
use serde::Deserialize;
use tokio::select;
use tokio::sync::broadcast::Receiver;
use tracing::{debug, error, info};

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
            Some(autodiscovery) => Ok(Box::new(ChecksSource {
                autodiscovery: Arc::clone(autodiscovery),
            })),
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
    autodiscovery: Arc<dyn AutodiscoveryProvider + Send + Sync>,
}

trait CheckRunner {
    fn can_run_check(&self, check_request: &Config) -> bool;
    fn run_check(&self, check_request: &Config) -> Result<(), GenericError>;
    fn stop_check(&self, check_name: &String);
}

/// A no-op implementation of CheckRunner that logs but doesn't execute checks
struct NoOpCheckRunner;

impl CheckRunner for NoOpCheckRunner {
    fn can_run_check(&self, _check_request: &Config) -> bool {
        true
    }

    fn run_check(&self, check_request: &Config) -> Result<(), GenericError> {
        info!("NoOpCheckRunner: Would run check {:?}", check_request.name);
        Ok(())
    }

    fn stop_check(&self, check_name: &String) {
        info!("NoOpCheckRunner: Would stop check {}", check_name);
    }
}

struct ChecksDispatcher {
    shutdown_handle: DynamicShutdownHandle,
    event_rx: Receiver<AutodiscoveryEvent>,
    runners: Vec<Arc<dyn CheckRunner + Send + Sync>>,
}

impl ChecksDispatcher {
    pub fn new(
        shutdown_handle: DynamicShutdownHandle, event_rx: Receiver<AutodiscoveryEvent>,
        runners: Vec<Arc<dyn CheckRunner + Send + Sync>>,
    ) -> Self {
        Self {
            shutdown_handle,
            event_rx,
            runners,
        }
    }

    pub async fn run(mut self) {
        let check_dispatcher_shutdown_coordinator = DynamicShutdownCoordinator::default();

        loop {
            select! {
                _ = &mut self.shutdown_handle => {
                    debug!("Received shutdown signal.");
                    break
                },
                event = self.event_rx.recv() => {
                    match event {
                        Ok(event) => {
                            match event {
                                AutodiscoveryEvent::Schedule { config } => {
                                    for runner in self.runners.iter_mut() {
                                        if runner.can_run_check(&config) {
                                            if let Err(e) = runner.run_check(&config) {
                                                error!("Error running check: {:?}", e);
                                            }
                                            continue;
                                        }
                                    }
                                }
                                AutodiscoveryEvent::Unscheduled { config_id } => {
                                    for runner in self.runners.iter_mut() {
                                        runner.stop_check(&config_id);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            error!("Error receiving event: {:?}", e);
                        }
                    }
                }
            }
        }

        check_dispatcher_shutdown_coordinator.shutdown().await;

        info!("Checks dispatcher stopped.");
    }
}

#[async_trait]
impl Source for ChecksSource {
    async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let mut global_shutdown = context.take_shutdown_handle();
        let mut health = context.take_health_handle();

        let mut listener_shutdown_coordinator = DynamicShutdownCoordinator::default();

        let _check_dispatcher_health = context.health_registry().register_component("check_dispatcher");
        let event_rx = self.autodiscovery.subscribe().await;

        let runners: Vec<Arc<dyn CheckRunner + Send + Sync>> = vec![Arc::new(NoOpCheckRunner)];

        let dispatcher = ChecksDispatcher::new(listener_shutdown_coordinator.register(), event_rx, runners);

        spawn_traced_named("checks-dispatcher", async move {
            dispatcher.run().await;
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
