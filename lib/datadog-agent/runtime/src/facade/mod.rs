//! A staged facade that drives the whole startup sequence of a remote agent.
//!
//! [`RemoteAgent`] walks through the stages in [`stage`], each of which hands control back to the binary at the point
//! where it has work of its own to do:
//!
//! 1. [`RemoteAgent::start`] registers with the Datadog Agent, waits for the initial configuration, and reloads logging
//!    to match it. In the [`Started`] stage, the binary can check its configuration before anything else starts.
//! 2. [`RemoteAgent::initialize`] creates the registries and the environment provider, which opens the connections for
//!    the workload provider. In the [`Initialized`] stage, the binary builds its topology and its control plane
//!    supervisor, and adds the supervisor from [`RemoteAgent::create_remote_agent_supervisor`] to its control plane.
//!    The binary can also subscribe to Remote Configuration products through [`RemoteAgent::remote_config_client`].
//! 3. [`RemoteAgent::run`] creates the internal supervisor around the binary's control plane, validates the memory
//!    bounds of everything registered so far, and runs the supervision tree until the process is asked to shut down.
//!
//! The resulting supervision tree looks like this, where `{identifier}` is the identifier from the application details
//! that the binary registered with `saluki_metadata`:
//!
//! ```text
//! {identifier}-root
//! ├── bootstrap supervisor (from the binary)
//! ├── internal-sup
//! │   ├── control plane (from the binary)
//! │   │   └── remote-agent
//! │   ├── resource registry
//! │   ├── config-updates (connected mode only)
//! │   ├── env-provider (connected mode only)
//! │   └── remote-config (connected mode only, once the binary asks for a Remote Configuration client)
//! └── the topology
//! ```

use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use datadog_agent_commons::ipc::config::RemoteAgentClientConfiguration;
use datadog_agent_remote_config::RemoteConfigurationClient;
use saluki_app::{
    accounting::initialize_memory_bounds, bootstrap::BootstrapGuard, metrics::emit_startup_metrics,
    util::wait_for_shutdown_signal,
};
use saluki_core::{
    accounting::ComponentRegistry,
    health::HealthRegistry,
    observability::metrics::RemapperRule,
    runtime::{self, state::ResourceRegistry, RestartMode, RestartStrategy, Supervisor, SupervisorError},
    topology::TopologyBlueprint,
};
use saluki_error::{ErrorContext as _, GenericError};
use tracing::{info, warn};

use crate::{
    control_plane::ConfigUpdatesWorker,
    env::RemoteAgentEnvironmentProvider,
    logging::LoggingTranslator,
    remote_agent::{RemoteAgentBootstrap, StatusSectionProvider},
};

mod config;
pub use self::config::{AuthoritativeConfiguration, LocalConfiguration, RemoteAgentSpecification};

mod remote_config;

pub mod stage;
pub use self::stage::{Initialized, Started, Unstarted};
use self::stage::{InitializedData, Stage, StartedData, UnstartedData};

/// The remote agent startup sequence.
///
/// `R` is the binary's [`RemoteAgentSpecification`], and `S` is the current [stage], which determines the methods
/// available.
pub struct RemoteAgent<R, S = Unstarted>
where
    R: RemoteAgentSpecification,
    S: Stage,
{
    inner: S::Data<R>,
}

impl<R> RemoteAgent<R, Unstarted>
where
    R: RemoteAgentSpecification,
{
    /// Creates a new `RemoteAgent` from its local configuration.
    ///
    /// `translator` translates the authoritative logging settings once they're known.
    pub fn new(local: R::LocalConfiguration, translator: LoggingTranslator) -> Self {
        Self {
            inner: UnstartedData {
                local,
                translator,
                start_time: Instant::now(),
            },
        }
    }

    /// Sets the time the process started, which is used to report how long the topology took to become healthy.
    ///
    /// Defaults to the time the `RemoteAgent` was created.
    pub fn with_start_time(mut self, start_time: Instant) -> Self {
        self.inner.start_time = start_time;
        self
    }

    /// Selects the configuration authority and reloads logging to match it.
    ///
    /// In connected mode, this registers with the Datadog Agent and waits for its initial configuration. In standalone
    /// mode, the local configuration becomes authoritative.
    ///
    /// `bootstrap_supervisor` is the supervisor returned by the process bootstrap, which runs as the first child of the
    /// root supervisor.
    ///
    /// # Errors
    ///
    /// If registration fails, or the configuration can't be loaded, an error is returned.
    pub async fn start(
        self, bootstrap_guard: &mut BootstrapGuard, bootstrap_supervisor: Supervisor,
    ) -> Result<RemoteAgent<R, Started>, GenericError> {
        let UnstartedData {
            local,
            translator,
            start_time,
        } = self.inner;
        let app_details = saluki_metadata::get_app_details();
        info!(
            version = app_details.version().raw(),
            git_hash = app_details.git_hash(),
            target_arch = app_details.target_arch(),
            build_time = app_details.build_time(),
            process_id = std::process::id(),
            "{} starting...",
            app_details.full_name()
        );

        // Use local sources for decisions required before the Agent configuration stream is available.
        let standalone = local.is_standalone();

        // Resolve the authoritative configuration. Connected mode registers as a remote agent and takes the Core
        // Agent's config stream as authority; standalone mode treats the local sources as authoritative. Both block
        // until the first configuration is received and validated, failing the boot if it cannot be -- the strict
        // startup gate.
        let (config, ra_bootstrap, config_updates) = if standalone {
            let config = local
                .into_standalone()
                .await
                .error_context("Failed to load configuration.")?;
            (config, None, None)
        } else {
            // Blocks until the Core Agent acknowledges registration.
            let client_config = local.client_configuration()?;
            let ra_bootstrap = async {
                let secure_api_listen_address = local.advertised_secure_api_address()?;
                RemoteAgentBootstrap::new(&client_config, &secure_api_listen_address).await
            }
            .await
            .error_context("Failed to bootstrap remote agent state.")?;

            // The configuration system owns the config stream. `ra_bootstrap` is kept to build the remote agent workers
            // in `create_remote_agent_supervisor`.
            let config_stream = ra_bootstrap.create_config_stream();

            info!("Waiting for initial configuration from Datadog Agent...");
            let (config, config_updates) = local
                .connect(config_stream)
                .await
                .error_context("Failed to load initial configuration from the Datadog Agent.")?;
            info!("Initial configuration received.");

            (config, Some(ra_bootstrap), Some(config_updates))
        };

        // Connected mode may have replaced the bootstrap-phase settings with the Agent's authoritative config, so
        // reload logging to match. Standalone resolves the same local sources seen at bootstrap, making a reload
        // redundant.
        if !standalone {
            match translator.translate(config.logging()) {
                Ok(logging_config) => {
                    if let Err(e) = bootstrap_guard.logging_mut().reload(logging_config).await {
                        warn!(
                            error = %e,
                            "Failed to reload logging from Agent configuration; continuing with bootstrap logging settings."
                        );
                    }
                }
                Err(e) => warn!(
                    error = %e,
                    "Failed to translate logging configuration from Agent; continuing with bootstrap logging settings."
                ),
            }
        }

        Ok(RemoteAgent {
            inner: StartedData {
                config,
                ra_bootstrap,
                config_updates,
                bootstrap_supervisor,
                start_time,
            },
        })
    }
}

impl<R> RemoteAgent<R, Started>
where
    R: RemoteAgentSpecification,
{
    /// Returns the authoritative configuration.
    pub fn config(&self) -> &R::AuthoritativeConfiguration {
        &self.inner.config
    }

    /// Returns `true` if the remote agent runs without the Datadog Agent.
    pub fn is_standalone(&self) -> bool {
        self.inner.config.is_standalone()
    }

    /// Creates the registries and the environment provider.
    ///
    /// In connected mode, the environment provider connects to the Datadog Agent for the hostname, autodiscovery, and
    /// workload metadata. In standalone mode, it reports the configured hostname, and nothing else.
    ///
    /// # Errors
    ///
    /// If the environment provider can't be created, an error is returned.
    pub async fn initialize(self) -> Result<RemoteAgent<R, Initialized>, GenericError> {
        // Create the root supervisor up front so that the binary's control plane can serve a handle to the whole
        // supervision tree. Nothing is added to it until the rest of the tree is built.
        //
        // The name can't be a bare `root`: it would collide with the built-in `root` resource group.
        let root_supervisor_name = format!("{}-root", saluki_metadata::get_app_details().identifier());
        let root_restart_strategy = RestartStrategy::new(RestartMode::OneForOne, 0, Duration::from_secs(5));
        let root_supervisor = Supervisor::new(root_supervisor_name)?.with_restart_strategy(root_restart_strategy);

        // Set up all of the building blocks for building topologies and launching internal processes.
        let component_registry = ComponentRegistry::default();
        let health_registry = HealthRegistry::new();
        let resource_registry = ResourceRegistry::new();
        let (env_provider, env_supervisor) = RemoteAgentEnvironmentProvider::from_authoritative(
            &self.inner.config,
            &component_registry,
            &health_registry,
        )
        .await?;

        Ok(RemoteAgent {
            inner: InitializedData {
                started: self.inner,
                root_supervisor,
                component_registry,
                health_registry,
                resource_registry,
                env_provider,
                env_supervisor,
                remote_config: None,
            },
        })
    }
}

impl<R> RemoteAgent<R, Initialized>
where
    R: RemoteAgentSpecification,
{
    /// Returns the authoritative configuration.
    pub fn config(&self) -> &R::AuthoritativeConfiguration {
        &self.inner.started.config
    }

    /// Returns `true` if the remote agent runs without the Datadog Agent.
    pub fn is_standalone(&self) -> bool {
        self.inner.started.config.is_standalone()
    }

    /// Returns the environment provider.
    pub fn env_provider(&self) -> &RemoteAgentEnvironmentProvider {
        &self.inner.env_provider
    }

    /// Returns the component registry, which the topology registers its components and memory bounds with.
    pub fn component_registry(&self) -> &ComponentRegistry {
        &self.inner.component_registry
    }

    /// Returns the health registry.
    pub fn health_registry(&self) -> &HealthRegistry {
        &self.inner.health_registry
    }

    /// Returns the configuration for connecting to the Datadog Agent, or `None` in standalone mode.
    pub fn client_configuration(&self) -> Option<&RemoteAgentClientConfiguration> {
        self.inner.env_provider.client_configuration()
    }

    /// Returns the Remote Configuration client, connecting to the Datadog Agent on the first call.
    ///
    /// The polling worker runs under the internal supervisor once the remote agent runs, so subscriptions made before
    /// then are all included in its first poll. Binaries should only call this when they have products to subscribe
    /// to, since connecting starts the worker polling.
    ///
    /// Returns `None` in standalone mode, or if Remote Configuration is turned off.
    ///
    /// # Errors
    ///
    /// If the client can't connect to the Datadog Agent, an error is returned.
    pub async fn remote_config_client(&mut self) -> Result<Option<RemoteConfigurationClient>, GenericError> {
        if let Some((client, _)) = &self.inner.remote_config {
            return Ok(Some(client.clone()));
        }

        let Some(client_config) = self.inner.env_provider.client_configuration() else {
            return Ok(None);
        };
        if !self.inner.started.config.remote_config_enabled() {
            return Ok(None);
        }

        let (client, worker) = remote_config::connect(client_config).await?;
        self.inner.remote_config = Some((client.clone(), worker));
        Ok(Some(client))
    }

    /// Creates the supervisor for the parts of the control plane that only the facade can build.
    ///
    /// The supervisor is named `remote-agent`, and must be added to the binary's control plane supervisor. It serves the
    /// supervision tree on the privileged API endpoint and, in connected mode, runs the remote agent workers from
    /// [`RemoteAgentBootstrap`]: it serves the status, flare, and telemetry services, and reports diagnostic events to
    /// the Datadog Agent.
    ///
    /// `status_sections` and `telemetry_rules` are passed to [`RemoteAgentBootstrap::create_services_worker`].
    ///
    /// # Errors
    ///
    /// If the supervisor can't be created, an error is returned.
    pub fn create_remote_agent_supervisor(
        &self, status_sections: Vec<Arc<dyn StatusSectionProvider>>, telemetry_rules: Vec<RemapperRule>,
    ) -> Result<Supervisor, GenericError> {
        let mut supervisor = Supervisor::new("remote-agent")?.with_restart_strategy(RestartStrategy::one_to_one());

        supervisor.add_worker(self.inner.root_supervisor.tree_handle().worker());
        if let Some(ra_bootstrap) = &self.inner.started.ra_bootstrap {
            supervisor.add_worker(ra_bootstrap.create_dataspace_anchor());
            supervisor.add_worker(ra_bootstrap.create_event_reporter());
            supervisor.add_worker(ra_bootstrap.create_services_worker(status_sections, telemetry_rules));
        }

        Ok(supervisor)
    }

    /// Runs the remote agent until the process receives a shutdown signal.
    ///
    /// See [`run_until`](Self::run_until) for the parameters.
    ///
    /// # Errors
    ///
    /// If the remote agent can't be assembled, or the supervision tree fails, an error is returned.
    pub async fn run(self, blueprint: TopologyBlueprint, control_plane: Supervisor) -> Result<(), GenericError> {
        self.run_until(blueprint, control_plane, wait_for_shutdown_signal())
            .await
    }

    /// Runs the remote agent until `shutdown` resolves.
    ///
    /// This creates the internal supervisor around `control_plane`, validates the memory bounds, and then runs the
    /// supervision tree with `blueprint` as the topology.
    ///
    /// `control_plane` is the binary's control plane supervisor, which must include the supervisor from
    /// [`create_remote_agent_supervisor`](Self::create_remote_agent_supervisor). The memory bounds cover every
    /// component registered with the component registry, so `blueprint` must be fully built.
    ///
    /// A shutdown in which some components had to be stopped forcefully, after exceeding their shutdown timeout, is
    /// logged as a warning rather than returned as an error, since the process did stop.
    ///
    /// # Errors
    ///
    /// If the internal supervisor can't be created, the memory bounds can't be validated, or the supervision tree
    /// fails, an error is returned.
    pub async fn run_until<F>(
        self, mut blueprint: TopologyBlueprint, control_plane: Supervisor, shutdown: F,
    ) -> Result<(), GenericError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let InitializedData {
            started,
            mut root_supervisor,
            component_registry,
            health_registry,
            resource_registry,
            env_provider,
            env_supervisor,
            remote_config,
        } = self.inner;
        let StartedData {
            config,
            ra_bootstrap: _,
            config_updates,
            bootstrap_supervisor,
            start_time,
        } = started;
        let full_name = saluki_metadata::get_app_details().full_name();

        root_supervisor.add_worker(bootstrap_supervisor);

        // Create the internal supervisor which drives the control plane and internal observability.
        //
        // The internal supervisor runs in ambient mode (the caller's runtime). The default restart strategy applies to
        // its children as units.
        let mut internal_supervisor =
            Supervisor::new("internal-sup").error_context("Failed to create internal supervisor.")?;
        internal_supervisor.add_worker(control_plane);
        internal_supervisor.add_worker(resource_registry.worker());

        if let Some(env_supervisor) = env_supervisor {
            internal_supervisor.add_worker(env_supervisor);
        }

        if let Some(config_updates) = config_updates {
            internal_supervisor.add_worker(ConfigUpdatesWorker::new(config_updates));
        }

        // The worker shares the client's subscriptions, so dropping the client here ends none of them.
        if let Some((_, worker)) = remote_config {
            internal_supervisor.add_worker(remote_config::remote_config_child(worker)?);
        }

        root_supervisor.add_worker(internal_supervisor);

        // Run memory bounds validation to ensure that the topology can launch within the configured memory limit, if
        // any.
        let memory_limiter = initialize_memory_bounds(config.memory_bounds(), component_registry.root())?;

        // Configure our topology blueprint with all of the relevant registries and ambient capabilities, which
        // ultimately get used when creating components, and sometimes exposed to them at runtime.
        blueprint
            .with_health_registry(health_registry.clone())
            .with_memory_limiter(memory_limiter)
            .with_resource_registry(resource_registry.clone())
            .with_environment_readiness(env_provider.wait_for_ready());

        // Spawn a simple task to report when the topology is healthy, which we have to do here before spawning
        // the topology as it requires altering the blueprint state to attach itself to the ready signal.
        spawn_topology_healthy_check(&mut blueprint, start_time);

        root_supervisor.add_worker(runtime::supervisable(blueprint).needed().build());

        info!("{full_name} running.");
        match root_supervisor.run_with_shutdown(shutdown).await {
            Ok(()) => {
                info!("{full_name} shut down successfully.");
                Ok(())
            }
            // The shutdown ran to completion, but one or more components ignored graceful shutdown and had to be
            // forcefully aborted after exceeding the shutdown timeout. Surface that plainly instead of reporting
            // success -- but don't treat it as a fatal error: the process did stop, and mapping it through the error
            // path would mislabel it as a boot or setup failure.
            Err(SupervisorError::ShutdownTimedOut { aborted }) => {
                warn!(
                    aborted,
                    "{full_name} shut down uncleanly: {aborted} component(s) had to be forcefully stopped after \
                     exceeding the shutdown timeout. Check the preceding warnings for which components were affected."
                );
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }
}

fn spawn_topology_healthy_check(blueprint: &mut TopologyBlueprint, started: Instant) {
    let topology_ready = blueprint.topology_ready();
    tokio::spawn(async move {
        // If the topology is torn down before it ever becomes ready (for example, shutdown during startup), `wait`
        // returns `false` and we skip reporting readiness.
        if topology_ready.wait().await {
            info!(
                topology_ready_ms = started.elapsed().as_millis(),
                "Topology healthy. Waiting for interrupt..."
            );

            emit_startup_metrics();
        }
    });
}
