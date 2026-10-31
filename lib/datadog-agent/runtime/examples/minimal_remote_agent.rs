//! A minimal remote agent built on `datadog-agent-runtime`.
//!
//! This example shows everything a remote agent supplies to the runtime: its configuration, its logging rules, its
//! topology, and its control plane. Its configuration "system" is a single settings struct behind an `ArcSwap`, which
//! takes the initial snapshot from the Datadog Agent and then follows updates to `log_level`.
//!
//! In connected mode, which is the default, the example registers with a Datadog Agent on `localhost:5001`, using the
//! default Agent IPC authentication token and certificate. To run it without an Agent, set
//! `MINIMAL_REMOTE_AGENT_STANDALONE` to any value. Either way, the privileged API endpoint needs the Agent IPC
//! certificate to exist.

use std::{future::Future, path::PathBuf, sync::Arc, time::Instant};

use arc_swap::ArcSwap;
use datadog_agent_commons::ipc::{
    config::{IpcAuthConfiguration, RemoteAgentClientConfiguration},
    tls::build_ipc_server_tls_config,
};
use datadog_agent_runtime::{
    control_plane::{ConfigRuntimeWorker, ConfigUpdatesRunner, RuntimeConfigSnapshot},
    env::{HostSettings, WorkloadSettings},
    facade::{AuthoritativeConfiguration, Initialized, LocalConfiguration, RemoteAgent, RemoteAgentSpecification},
    logging::{LoggingSettings, LoggingTranslator},
    remote_agent::{StatusBuilder, StatusSectionProvider},
};
use futures::{stream, Stream};
use saluki_api::EndpointType;
use saluki_app::{
    accounting::{MemoryBoundsConfiguration, MemoryMode, ResourceTelemetryWorker},
    api::APIBuilder,
    bootstrap::{AppBootstrapper, Bootstrap},
    config::{ConfigSnapshotFn, ConfigWorker},
    logging::{DynamicLogLevelWorker, LoggingOverrideController},
};
use saluki_config::dynamic::ConfigUpdate;
use saluki_core::{
    observability::metrics::InternalTelemetryAPIWorker, runtime::Supervisor, topology::TopologyBlueprint,
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::ListenAddress;
use saluki_metadata::AppDetails;
use serde::Serialize;
use tokio::sync::{mpsc, watch, Mutex};

const APP_DETAILS: AppDetails = saluki_metadata::declare_app_details!(
    full_name = "Minimal Remote Agent",
    short_name = "minimal-remote-agent",
    identifier = "minimal",
);

/// The settings of the example remote agent.
#[derive(Clone, Serialize)]
struct Settings {
    standalone: bool,
    hostname: String,
    log_level: String,
    cmd_port: u16,
    api_listen_address: String,
    secure_api_listen_address: String,
}

impl Settings {
    /// Applies a setting from the Datadog Agent. Only `log_level` is followed; everything else stays local.
    fn apply(&mut self, key: &str, value: &serde_json::Value) {
        if key == "log_level" {
            if let Some(level) = value.as_str() {
                self.log_level = level.to_string();
            }
        }
    }

    fn client_configuration(&self) -> Result<RemoteAgentClientConfiguration, GenericError> {
        let auth = IpcAuthConfiguration::new(PathBuf::new(), PathBuf::new());
        RemoteAgentClientConfiguration::from_parts(self.cmd_port, auth, 4 * 1024 * 1024, "")
    }

    fn secure_api_listen_address(&self) -> Result<ListenAddress, GenericError> {
        ListenAddress::try_from(self.secure_api_listen_address.as_str())
            .map_err(|e| generic_error!("Invalid secure API listen address: {}", e))
    }
}

/// The example as a remote agent.
enum MinimalRemoteAgent {}

impl RemoteAgentSpecification for MinimalRemoteAgent {
    type LocalConfiguration = LocalConfig;
    type AuthoritativeConfiguration = Config;
    type ConfigurationUpdates = ConfigUpdates;
}

/// The local configuration, before the configuration authority is selected.
struct LocalConfig {
    settings: Settings,
}

impl LocalConfig {
    fn load() -> Self {
        Self {
            settings: Settings {
                standalone: std::env::var_os("MINIMAL_REMOTE_AGENT_STANDALONE").is_some(),
                hostname: "minimal-remote-agent-host".to_string(),
                log_level: "info".to_string(),
                cmd_port: 5001,
                api_listen_address: "tcp://127.0.0.1:5200".to_string(),
                secure_api_listen_address: "tcp://127.0.0.1:5201".to_string(),
            },
        }
    }

    fn logging(&self) -> LoggingSettings {
        let mut logging = LoggingSettings::default();
        logging.level = self.settings.log_level.clone();
        logging.disable_file_logging = true;
        logging
    }
}

impl LocalConfiguration<MinimalRemoteAgent> for LocalConfig {
    fn is_standalone(&self) -> bool {
        self.settings.standalone
    }

    fn client_configuration(&self) -> Result<RemoteAgentClientConfiguration, GenericError> {
        self.settings.client_configuration()
    }

    fn advertised_secure_api_address(&self) -> Result<ListenAddress, GenericError> {
        self.settings.secure_api_listen_address()
    }

    async fn connect(
        mut self, mut config_stream: mpsc::Receiver<ConfigUpdate>,
    ) -> Result<(Config, ConfigUpdates), GenericError> {
        // Wait for the initial snapshot, ignoring any partial update that arrives before it.
        loop {
            match config_stream.recv().await {
                Some(ConfigUpdate::Snapshot(settings)) => {
                    for setting in &settings {
                        self.settings.apply(&setting.key, &setting.value);
                    }
                    break;
                }
                Some(_) => continue,
                None => {
                    return Err(generic_error!(
                        "Configuration stream closed before the initial snapshot."
                    ))
                }
            }
        }

        let config = Config::new(self.settings);
        let updates = ConfigUpdates {
            stream: Arc::new(Mutex::new(config_stream)),
            current: Arc::clone(&config.current),
            changed: Arc::clone(&config.changed),
        };
        Ok((config, updates))
    }

    async fn into_standalone(self) -> Result<Config, GenericError> {
        Ok(Config::new(self.settings))
    }
}

/// The authoritative configuration.
struct Config {
    current: Arc<ArcSwap<Settings>>,
    changed: Arc<watch::Sender<()>>,
}

impl Config {
    fn new(settings: Settings) -> Self {
        let (changed, _) = watch::channel(());
        Self {
            current: Arc::new(ArcSwap::from_pointee(settings)),
            changed: Arc::new(changed),
        }
    }

    fn api_listen_address(&self) -> Result<ListenAddress, GenericError> {
        ListenAddress::try_from(self.current.load().api_listen_address.as_str())
            .map_err(|e| generic_error!("Invalid API listen address: {}", e))
    }

    /// Returns each change to `log_level`, starting with the first change rather than the current level.
    fn log_levels(&self) -> impl Stream<Item = String> + Send + 'static {
        let current = Arc::clone(&self.current);
        let last = current.load().log_level.clone();
        stream::unfold(
            (current, self.changed.subscribe(), last),
            |(current, mut changed, mut last)| async move {
                // Once the configuration can no longer change, end the stream.
                while changed.changed().await.is_ok() {
                    let level = current.load().log_level.clone();
                    if level != last {
                        last = level.clone();
                        return Some((level, (current, changed, last)));
                    }
                }
                None
            },
        )
    }

    fn config_snapshot(&self) -> ConfigSnapshotFn {
        let current = Arc::clone(&self.current);
        Arc::new(move || serde_json::to_value(&**current.load()).unwrap_or_default())
    }

    fn runtime_config(&self) -> RuntimeConfigSnapshot {
        RuntimeConfigSnapshot::from_shared(Arc::clone(&self.current))
    }
}

impl AuthoritativeConfiguration for Config {
    fn is_standalone(&self) -> bool {
        self.current.load().standalone
    }

    fn client_configuration(&self) -> Result<RemoteAgentClientConfiguration, GenericError> {
        self.current.load().client_configuration()
    }

    fn logging(&self) -> LoggingSettings {
        let mut logging = LoggingSettings::default();
        logging.level = self.current.load().log_level.clone();
        logging.disable_file_logging = true;
        logging
    }

    fn workload(&self) -> WorkloadSettings {
        WorkloadSettings::default()
    }

    fn host(&self) -> HostSettings {
        let mut host = HostSettings::default();
        host.hostname = Some(self.current.load().hostname.clone());
        host
    }

    fn remote_config_enabled(&self) -> bool {
        false
    }

    fn memory_bounds(&self) -> MemoryBoundsConfiguration {
        MemoryBoundsConfiguration {
            memory_limit: None,
            memory_slop_factor: 0.25,
            enable_global_limiter: false,
            memory_mode: MemoryMode::Disabled,
        }
    }
}

/// Applies the updates that the Datadog Agent sends after its initial snapshot.
struct ConfigUpdates {
    stream: Arc<Mutex<mpsc::Receiver<ConfigUpdate>>>,
    current: Arc<ArcSwap<Settings>>,
    changed: Arc<watch::Sender<()>>,
}

impl ConfigUpdatesRunner for ConfigUpdates {
    fn run(&self) -> impl Future<Output = Result<(), GenericError>> + Send + 'static {
        let stream = Arc::clone(&self.stream);
        let current = Arc::clone(&self.current);
        let changed = Arc::clone(&self.changed);

        async move {
            // Holding the stream across calls lets a restarted worker continue where the previous one stopped.
            let mut stream = stream.lock().await;
            while let Some(update) = stream.recv().await {
                let mut settings = Settings::clone(&current.load());
                match update {
                    ConfigUpdate::Snapshot(snapshot) => {
                        for setting in &snapshot {
                            settings.apply(&setting.key, &setting.value);
                        }
                    }
                    ConfigUpdate::Partial(setting) => settings.apply(&setting.key, &setting.value),
                }
                current.store(Arc::new(settings));
                changed.send_replace(());
            }

            Err(generic_error!("Configuration stream from the Datadog Agent closed."))
        }
    }
}

/// A status section that the Datadog Agent shows in its status output.
struct ExampleStatusSection;

impl StatusSectionProvider for ExampleStatusSection {
    fn write_status(&self, builder: &mut StatusBuilder) {
        builder
            .named_section("Minimal Remote Agent")
            .set_field("Mode", "example");
    }
}

/// Creates the control plane supervisor, which serves the internal APIs and the remote agent supervisor from the facade.
async fn create_control_plane(
    agent: &RemoteAgent<MinimalRemoteAgent, Initialized>, translator: &LoggingTranslator,
    logging_controller: LoggingOverrideController,
) -> Result<Supervisor, GenericError> {
    let config = agent.config();
    let mut supervisor = Supervisor::new("ctrl-pln")?;

    supervisor.add_worker(agent.health_registry().worker());
    supervisor.add_worker(ResourceTelemetryWorker::new(agent.component_registry()));
    supervisor.add_worker(InternalTelemetryAPIWorker::new(None));
    supervisor.add_worker(DynamicLogLevelWorker::new(
        translator.translate_log_levels(config.log_levels()),
        logging_controller,
    ));
    supervisor.add_worker(ConfigWorker::new(config.config_snapshot()));
    supervisor.add_worker(ConfigRuntimeWorker::new(config.runtime_config()));
    supervisor.add_worker(APIBuilder::new(EndpointType::Unprivileged, config.api_listen_address()?).into_supervisor());
    supervisor.add_worker(agent.create_remote_agent_supervisor(
        vec![Arc::new(ExampleStatusSection)],
        Vec::new(),
        |routes, _| routes,
    )?);

    // Every client of the privileged API endpoint must present the Agent IPC certificate.
    let ipc_auth = IpcAuthConfiguration::new(PathBuf::new(), PathBuf::new());
    let tls_config = build_ipc_server_tls_config(ipc_auth.ipc_cert_file_path()).await?;
    let secure_api_listen_address = config.current.load().secure_api_listen_address()?;
    supervisor.add_worker(
        APIBuilder::new(EndpointType::Privileged, secure_api_listen_address)
            .with_tls_config(tls_config)
            .into_supervisor(),
    );

    Ok(supervisor)
}

fn main() -> Result<(), GenericError> {
    let started = Instant::now();
    saluki_metadata::set_app_details(APP_DETAILS);

    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .error_context("Failed to build the runtime.")?
        .block_on(run(started))
}

async fn run(started: Instant) -> Result<(), GenericError> {
    let local = LocalConfig::load();

    // A plain log level also applies to this binary's own crate.
    let translator = LoggingTranslator::new("/var/log/datadog/minimal-remote-agent.log")
        .with_first_party_targets(&["minimal_remote_agent"]);

    let Bootstrap {
        supervisor: bootstrap_supervisor,
        guard: mut bootstrap_guard,
    } = AppBootstrapper::new()
        .with_metrics_prefix("minimal")
        .with_logging_configuration(translator.translate(local.logging())?)
        .bootstrap()
        .await?;

    let agent = RemoteAgent::<MinimalRemoteAgent>::new(local, translator.clone())
        .with_start_time(started)
        .start(&mut bootstrap_guard, bootstrap_supervisor)
        .await?;

    // The binary can check its configuration here, before anything else starts.

    let agent = agent.initialize().await?;

    // Build the topology here, using `agent.env_provider()` for hostnames and workload metadata, and registering
    // components with `agent.component_registry()`.
    let blueprint = TopologyBlueprint::new("primary", agent.component_registry());

    let control_plane = create_control_plane(&agent, &translator, bootstrap_guard.logging().controller()).await?;
    agent.run(blueprint, control_plane).await
}
