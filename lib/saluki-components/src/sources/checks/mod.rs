/// Checks source.
///
/// Listen to Autodiscovery events, schedule checks and emit results.
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::task::spawn_traced_named;
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{sources::*, ComponentContext},
    data_model::event::{Event, EventType},
    topology::{
        shutdown::{ComponentShutdownHandle, DynamicShutdownCoordinator, DynamicShutdownHandle},
        OutputDefinition,
    },
};
use saluki_env::autodiscovery::{AutodiscoveryEvent, AutodiscoveryProvider};
use saluki_env::{EnvironmentProvider, HostProvider};
use saluki_error::{generic_error, GenericError};
use serde::Deserialize;
use tokio::select;
use tokio::sync::broadcast::Receiver;
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace};

mod scheduler;
use scheduler::{Runner, Scheduler};

mod check;
use check::Check;

mod builder;
mod check_metric;
mod execution_context;

use self::builder::native::builder::NativeCheckBuilder;
#[cfg(feature = "python-checks")]
use self::builder::python::builder::PythonCheckBuilder;
use self::builder::CheckBuilder;
use self::execution_context::ExecutionContext;

const fn default_check_runners() -> usize {
    4
}

/// Configuration for the checks source.
#[derive(Deserialize)]
pub struct ChecksConfiguration<E = ()> {
    /// The number of check runners to use.
    ///
    /// Defaults to 4.
    #[serde(default = "default_check_runners")]
    check_runners: usize,

    /// Additional checks.d folders.
    #[serde(default)]
    additional_checksd: String,

    #[serde(skip)]
    environment_provider: Option<E>,

    #[serde(skip)]
    full_configuration: Option<GenericConfiguration>,
}

impl ChecksConfiguration {
    /// Creates a new `ChecksConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let mut checks_config = config.as_typed::<Self>()?;
        checks_config.full_configuration = Some(config.clone());
        Ok(checks_config)
    }
}

impl<E> ChecksConfiguration<E> {
    /// Sets the environment provider to use.
    pub fn with_environment_provider<E2>(self, environment_provider: E2) -> ChecksConfiguration<E2> {
        ChecksConfiguration {
            check_runners: self.check_runners,
            additional_checksd: self.additional_checksd,
            environment_provider: Some(environment_provider),
            full_configuration: self.full_configuration,
        }
    }
}

#[async_trait]
impl<E> SourceBuilder for ChecksConfiguration<E>
where
    E: EnvironmentProvider + Send + Sync + 'static,
    <E::Host as HostProvider>::Error: Into<GenericError> + std::fmt::Debug,
{
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        let environment_provider = self
            .environment_provider
            .as_ref()
            .ok_or_else(|| generic_error!("No environment provider configured."))?;

        let receiver = environment_provider
            .autodiscovery()
            .subscribe()
            .await
            .ok_or_else(|| generic_error!("No autodiscovery stream configured."))?;

        let configuration = self
            .full_configuration
            .as_ref()
            .ok_or_else(|| generic_error!("No configuration configured."))?;

        let execution_context =
            ExecutionContext::from_environment_provider(configuration.clone(), environment_provider).await;

        Ok(Box::new(ChecksSource {
            autodiscovery_rx: receiver,
            check_runners: self.check_runners,
            custom_checks_dirs: if !self.additional_checksd.is_empty() {
                Some(self.additional_checksd.split(",").map(|s| s.to_string()).collect())
            } else {
                None
            },
            execution_context: Arc::new(execution_context),
        }))
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition<EventType>>> = LazyLock::new(|| {
            vec![
                OutputDefinition::named_output("metrics", EventType::Metric),
                OutputDefinition::named_output("service_checks", EventType::ServiceCheck),
                OutputDefinition::named_output("events", EventType::EventD),
            ]
        });
        &OUTPUTS
    }
}

impl<E> MemoryBounds for ChecksConfiguration<E> {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<ChecksSource>("component struct");
    }
}

struct ChecksSource {
    autodiscovery_rx: Receiver<AutodiscoveryEvent>,
    check_runners: usize,
    custom_checks_dirs: Option<Vec<String>>,
    execution_context: Arc<ExecutionContext>,
}

impl ChecksSource {
    /// Builds the check builders for the source.
    fn builders(
        &self, check_events_tx: mpsc::Sender<Event>, execution_context: Arc<ExecutionContext>,
    ) -> Vec<Arc<dyn CheckBuilder + Send + Sync>> {
        let mut builders = Vec::<Arc<dyn CheckBuilder + Send + Sync>>::new();
        builders.push(Arc::new(NativeCheckBuilder::new(
            check_events_tx.clone(),
            self.custom_checks_dirs.clone(),
            execution_context.clone(),
        )));
        #[cfg(feature = "python-checks")]
        {
            builders.push(Arc::new(PythonCheckBuilder::new(
                check_events_tx.clone(),
                self.custom_checks_dirs.clone(),
                execution_context.clone(),
            )));
        }
        builders
    }
}

#[async_trait]
impl Source for ChecksSource {
    async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let mut global_shutdown: ComponentShutdownHandle = context.take_shutdown_handle();
        let mut health = context.take_health_handle();
        health.mark_ready();

        info!("Checks source started.");

        let (check_events_tx, check_event_rx) = mpsc::channel(128);

        let mut check_builders: Vec<Arc<dyn CheckBuilder + Send + Sync>> =
            self.builders(check_events_tx, self.execution_context.clone());

        let (scheduler, checks_rx) = Scheduler::new();
        let scheduler = Arc::new(scheduler);
        let workers_count = if self.check_runners != 0 {
            Some(self.check_runners)
        } else {
            None
        };
        let runner = Runner::new(checks_rx, workers_count).await;
        let runner = Arc::new(runner);

        let mut listener_shutdown_coordinator = DynamicShutdownCoordinator::default();

        spawn_traced_named(
            "checks-events-listener",
            drain_and_dispatch_check_events(listener_shutdown_coordinator.register(), context, check_event_rx),
        );

        let mut autodiscovery_rx = self.autodiscovery_rx;
        runner.clone().run().await;

        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Checks source received shutdown signal.");
                    scheduler.clone().shutdown().await;
                    break
                },

                _ = health.live() => continue,

                event = autodiscovery_rx.recv() => match event {
                        Ok(event) => {
                            match event {
                                AutodiscoveryEvent::CheckSchedule { config } => {
                                    let mut runnable_checks: Vec<Arc<dyn Check + Send + Sync>> = vec![];
                                    for instance in &config.instances {
                                        let check_id = instance.id();
                                        if scheduler.clone().is_check_scheduled(check_id) {
                                            println!("already scheduled!");
                                            continue;
                                        }

                                        for builder in check_builders.iter_mut() {
                                            if let Some(check) = builder.build_check(&config.name, instance, &config.init_config, &config.source).await {
                                                let check_id = check.id().to_string();
                                                let check_version = check.version().to_string();
                                                let check_source = check.source().to_string();

                                                debug!(
                                                    check_id = check_id,
                                                    check_name = config.name.as_ref(),
                                                    check_version = check_version,
                                                    check_source = check_source,
                                                    "Built new check instance."
                                                );
                                                runnable_checks.push(check);
                                                break;
                                            }
                                        }
                                    }

                                    for check in runnable_checks {
                                        scheduler.clone().schedule(check).await;
                                    }
                                }
                                AutodiscoveryEvent::CheckUnscheduled { config } => {
                                    for instance in &config.instances {
                                        let check_id = instance.id();
                                        scheduler.clone().unschedule(check_id).await;
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

        listener_shutdown_coordinator.shutdown().await;
        scheduler.shutdown().await;

        info!("Checks source stopped.");

        Ok(())
    }
}

async fn drain_and_dispatch_check_events(
    shutdown_handle: DynamicShutdownHandle, context: SourceContext, mut check_event_rx: mpsc::Receiver<Event>,
) {
    tokio::pin!(shutdown_handle);

    loop {
        select! {
            _ = &mut shutdown_handle => {
                debug!("Checks events listeners received shutdown signal.");
                break;
            }
            result = check_event_rx.recv() => match result {
                None => break,
                Some(check_event) => {
                    trace!("Received check event: {:?}", check_event);

                    match check_event.event_type() {
                       EventType::Metric => {
                            let mut buffered_dispatcher = context
                                .dispatcher()
                                .buffered_named("metrics")
                                .expect("checks source metrics output should always exist");

                                if let Err(e) = buffered_dispatcher.push(check_event).await {
                                    error!(error = %e, "Failed to forward check metric.");
                                }

                                if let Err(e) = buffered_dispatcher.flush().await {
                                    error!(error = %e, "Failed to flush check metric.");
                                }
                        },
                        EventType::ServiceCheck => {
                            let mut buffered_dispatcher = context
                                .dispatcher()
                                .buffered_named("service_checks")
                                .expect("checks source service checks output should always exist");

                                if let Err(e) = buffered_dispatcher.push(check_event).await {
                                    error!(error = %e, "Failed to forward check service check.");
                                }

                                if let Err(e) = buffered_dispatcher.flush().await {
                                    error!(error = %e, "Failed to flush check service check.");
                                }
                        },
                        EventType::EventD => {
                            let mut buffered_dispatcher = context
                                .dispatcher()
                                .buffered_named("events")
                                .expect("checks source events output should always exist");

                                if let Err(e) = buffered_dispatcher.push(check_event).await {
                                    error!(error = %e, "Failed to forward check event");
                                }

                                if let Err(e) = buffered_dispatcher.flush().await {
                                    error!(error = %e, "Failed to flush check event.");
                                }
                        },
                        _ => {},
                    }

                }
            }
        }
    }
}
