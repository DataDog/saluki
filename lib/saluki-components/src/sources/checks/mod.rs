/// Checks source.
///
/// Listen to Autodiscovery events, schedule checks and emit results.
use std::collections::HashSet;
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
use saluki_env::HostProvider;
use saluki_error::{generic_error, GenericError};
use serde::Deserialize;
use tokio::select;
use tokio::sync::broadcast::Receiver;
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

mod scheduler;
use self::scheduler::Scheduler;

mod check_metric;

mod check;
use self::check::Check;

mod builder;

#[cfg(feature = "python-checks")]
use self::builder::python::builder::PythonCheckBuilder;
use self::builder::CheckBuilder;

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

    /// Additional checks.d folders.
    #[serde(default)]
    additional_checksd: String,

    /// Autodiscovery provider to use.
    #[serde(skip)]
    autodiscovery_provider: Option<Arc<dyn AutodiscoveryProvider + Send + Sync>>,

    /// Hostname provider to use.
    #[serde(skip)]
    hostname_provider: Option<Arc<dyn HostProvider<Error = GenericError> + Send + Sync>>,

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

    /// Sets the autodiscovery provider to use.
    pub fn with_autodiscovery_provider<A>(mut self, provider: A) -> Self
    where
        A: AutodiscoveryProvider + Send + Sync + 'static,
    {
        self.autodiscovery_provider = Some(Arc::new(provider));
        self
    }

    /// Sets the hostname provider to use.
    pub fn with_hostname_provider<A>(mut self, provider: A) -> Self
    where
        A: HostProvider<Error = GenericError> + Send + Sync + 'static,
    {
        self.hostname_provider = Some(Arc::new(provider));
        self
    }
}

#[async_trait]
impl SourceBuilder for ChecksConfiguration {
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        if let Some(autodiscovery) = &self.autodiscovery_provider {
            if let Some(host) = &self.hostname_provider {
                if let Some(receiver) = autodiscovery.subscribe().await {
                    let hostname = host.get_hostname().await.unwrap_or_else(|e| {
                        warn!("Failed to get hostname: {:?}", e);
                        "".to_string()
                    });

                    Ok(Box::new(ChecksSource {
                        autodiscovery_rx: receiver,

                        check_runners: self.check_runners,
                        custom_checks_dirs: if !self.additional_checksd.is_empty() {
                            Some(self.additional_checksd.split(",").map(|s| s.to_string()).collect())
                        } else {
                            None
                        },

                        configuration: self.full_configuration.clone(),
                        hostname,
                    }))
                } else {
                    Err(generic_error!("No autodiscovery stream configured."))
                }
            } else {
                Err(generic_error!("No hostname provider configured."))
            }
        } else {
            Err(generic_error!("No autodiscovery provider configured."))
        }
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition>> = LazyLock::new(|| {
            vec![
                OutputDefinition::named_output("metrics", EventType::Metric),
                OutputDefinition::named_output("service_checks", EventType::ServiceCheck),
            ]
        });

        &OUTPUTS
    }
}

impl MemoryBounds for ChecksConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<ChecksSource>("component struct");
    }
}

struct ChecksSource {
    hostname: String,
    autodiscovery_rx: Receiver<AutodiscoveryEvent>,
    check_runners: usize,
    custom_checks_dirs: Option<Vec<String>>,
    configuration: Option<GenericConfiguration>,
}

impl ChecksSource {
    /// Builds the check builders for the source.
    fn builders(
        &self, check_events_tx: mpsc::Sender<Event>, configuration: Option<GenericConfiguration>, hostname: String,
    ) -> Vec<Arc<dyn CheckBuilder + Send + Sync>> {
        #[cfg(feature = "python-checks")]
        {
            vec![Arc::new(PythonCheckBuilder::new(
                check_events_tx,
                self.custom_checks_dirs.clone(),
                configuration.clone(),
                hostname,
            ))]
        }
        #[cfg(not(feature = "python-checks"))]
        {
            let _ = check_events_tx; // Suppress unused variable warning
            let _ = &self.custom_checks_dirs; // Suppress unused field warning
            let _ = &configuration; // Suppress unused field warning
            let _ = hostname; // Suppress unused field warning
            vec![]
        }
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
            self.builders(check_events_tx, self.configuration.clone(), self.hostname.clone());

        let mut check_ids = HashSet::new();
        let scheduler = Scheduler::new(self.check_runners);

        let mut listener_shutdown_coordinator = DynamicShutdownCoordinator::default();

        spawn_traced_named(
            "checks-events-listener",
            drain_and_dispatch_check_events(listener_shutdown_coordinator.register(), context, check_event_rx),
        );

        let mut autodiscovery_rx = self.autodiscovery_rx;

        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Checks source received shutdown signal.");
                    scheduler.shutdown().await;
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
                }
            }
        }

        listener_shutdown_coordinator.shutdown().await;

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
                debug!("Checks events listerners received shutdown signal.");
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
                                .expect("checks source should always have default output");

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
                                .expect("checks source should always have default output");

                                if let Err(e) = buffered_dispatcher.push(check_event).await {
                                    error!(error = %e, "Failed to forward check service check.");
                                }

                                if let Err(e) = buffered_dispatcher.flush().await {
                                    error!(error = %e, "Failed to flush check service check.");
                                }
                        },
                        _ => {}
                    }

                }
            }
        }
    }
}
