/// Checks source.
///
/// Listen to Autodiscovery events, schedule checks and emit results.
use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_core::pooling::ObjectPool;
use saluki_core::{
    components::{sources::*, ComponentContext},
    data_model::event::Event,
    data_model::event::EventType,
    topology::OutputDefinition,
};
use saluki_env::autodiscovery::{AutodiscoveryEvent, AutodiscoveryProvider};
use saluki_error::{generic_error, GenericError};
use serde::Deserialize;
use tokio::select;
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

mod scheduler;
use self::scheduler::Scheduler;

mod check_metric;

mod check;
use self::check::Check;

mod builder;
use self::builder::{python::builder::PythonCheckBuilder, CheckBuilder};

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
        static OUTPUTS: [OutputDefinition; 1] = [OutputDefinition::default_output(EventType::Metric)];

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

        let (check_metrics_tx, mut check_metrics_rx) = mpsc::channel(10_000_000);

        let mut event_rx = self.autodiscovery.subscribe().await;
        let mut check_builders: Vec<Arc<dyn CheckBuilder + Send + Sync>> =
            vec![Arc::new(PythonCheckBuilder::new(check_metrics_tx))];
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
                                            if let Some(check) = builder.build_check(&config.name, instance, &config.init_config, &config.source) {
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
                },

                Some(check_metric) = check_metrics_rx.recv() => {
                    trace!("Received check metric: {:?}", check_metric);
                    let mut event_buffer = context.event_buffer_pool().acquire().await;
                    let event: Event = check_metric.try_into().expect("can't convert");
                    if let Some(_unsent) = event_buffer.try_push(event) {
                        error!("Event buffer full, dropping event");
                        continue;
                    }
                    if let Err(e) = context.dispatcher().dispatch_buffer(event_buffer).await {
                        error!(error = %e, "Failed to forward check metrics.");
                    }
                }
            }
        }

        info!("Checks source stopped.");

        Ok(())
    }
}
