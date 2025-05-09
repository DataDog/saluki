/// Checks source.
///
/// Listen to Autodiscovery events, schedule checks and emit results.
use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{sources::*, ComponentContext},
    data_model::event::DataType,
    topology::OutputDefinition,
};
use saluki_env::autodiscovery::{AutodiscoveryEvent, AutodiscoveryProvider};
use saluki_error::{generic_error, GenericError};
use serde::Deserialize;
use tokio::select;
use tracing::{debug, error, info, warn};

mod scheduler;
use self::scheduler::Scheduler;

mod check;
use self::check::Check;

mod builder;
use self::builder::{CheckBuilder, NoopCheckBuilder};

const fn default_check_runners() -> usize {
    4
}

/// Configuration for the checks source.
#[derive(Deserialize)]
pub struct ChecksConfiguration {
    /// The number of check runners to use.
    ///
    /// Defaults to 4.
    #[serde(default = "default_check_runners")]
    check_runners: usize,

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
                check_runners: self.check_runners,
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
    check_runners: usize,
}

#[async_trait]
impl Source for ChecksSource {
    async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let mut global_shutdown: saluki_core::topology::shutdown::ComponentShutdownHandle =
            context.take_shutdown_handle();
        let mut health = context.take_health_handle();
        health.mark_ready();

        info!("Checks source started.");

        let mut event_rx = self.autodiscovery.subscribe().await;
        let mut check_builders: Vec<Arc<dyn CheckBuilder + Send + Sync>> = vec![Arc::new(NoopCheckBuilder)];
        let mut check_ids = HashSet::new();
        let scheduler = Scheduler::new(self.check_runners);

        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    scheduler.shutdown().await;
                    break
                },

                _ = health.live() => continue,

                event = event_rx.recv() => match event {
                        Ok(event) => {
                            match event {
                                AutodiscoveryEvent::CheckSchedule { config } => {
                                    let mut runnable_checks: Vec<Arc<dyn Check + Send + Sync>> = vec![];
                                    for instance in &config.instances {
                                        let check_id = instance.id();
                                        if check_ids.contains(check_id) {
                                            continue;
                                        }

                                        for builder in check_builders.iter_mut() {
                                            if let Some(check) = builder.build_check(check_id, &config) {
                                                runnable_checks.push(check);
                                                check_ids.insert(check_id.clone());
                                                break;
                                            }
                                        }
                                    }

                                    for check in runnable_checks {
                                        scheduler.schedule(check);
                                    }
                                }
                                AutodiscoveryEvent::CheckUnscheduled { config } => {
                                    for instance in &config.instances {
                                        let check_id = instance.id();
                                        if !check_ids.contains(check_id) {
                                            warn!("Unscheduling check {} not found, skipping.", check_id);
                                            continue;
                                        }

                                        scheduler.unschedule(check_id);
                                    }
                                }
                                // We only care about CheckSchedule and CheckUnscheduled events
                                _ => {}
                            }
                        }
                        Err(e) => {
                            error!("Error receiving event: {:?}", e);
                        }
                }
            }
        }

        info!("Checks source stopped.");

        Ok(())
    }
}
