use std::{
    sync::{Arc, Mutex, Once, OnceLock},
    time::Duration,
};

use async_trait::async_trait;
use bytesize::ByteSize;
use saluki_app::{
    accounting::{MemoryBoundsConfiguration, MemoryMode},
    bootstrap::{AppBootstrapper, Bootstrap},
};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_config::dynamic::ConfigUpdate;
use saluki_core::{
    runtime::{
        InitializationError, NodeSnapshot, NodeState, Supervisable, SupervisionTreeHandle, SupervisorFuture,
        TreeSnapshot,
    },
    support::SubsystemIdentifier,
};
use saluki_env::{EnvironmentProvider as _, HostProvider as _};
use saluki_error::generic_error;
use saluki_io::net::ListenAddress;
use saluki_metadata::AppDetails;
use tokio::{
    sync::{mpsc, oneshot, MutexGuard},
    task::JoinHandle,
};
use tracing::Level;

use super::*;
use crate::{
    control_plane::ConfigUpdatesRunner,
    env::{HostSettings, WorkloadSettings},
    logging::LoggingSettings,
};

mod fake_agent;
use self::fake_agent::{FakeAgent, FAKE_AGENT_HOSTNAME};

const APP_DETAILS: AppDetails = saluki_metadata::declare_app_details!(
    full_name = "Facade Test Agent",
    short_name = "facade-test",
    identifier = "facade-test",
);

const ADVERTISED_SECURE_API_ADDRESS: &str = "tcp://127.0.0.1:5101";
const STANDALONE_HOSTNAME: &str = "facade-test-host";
const WAIT_TIMEOUT: Duration = Duration::from_secs(30);
// Below the default graceful shutdown timeout of 5s, so a child that ignores shutdown fails the test instead of being
// aborted, which `run_until` still reports as success.
const CLEAN_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(4);

/// Locks the process-wide bootstrap, creating it on first use.
///
/// The application details can only be registered once per process, and the process can only be bootstrapped once, so
/// every test that drives the facade shares one bootstrap and holds this lock while it runs. The bootstrap supervisor
/// runs on a thread of its own, which keeps the logging override worker applying `LoggingGuard::reload` for every test,
/// whichever runtime it runs on.
async fn lock_bootstrap() -> MutexGuard<'static, BootstrapGuard> {
    static APP_DETAILS_REGISTERED: Once = Once::new();
    static BOOTSTRAP: OnceLock<tokio::sync::Mutex<BootstrapGuard>> = OnceLock::new();

    BOOTSTRAP
        .get_or_init(|| {
            // A failed bootstrap leaves `BOOTSTRAP` empty, so later tests retry it and report the same error.
            APP_DETAILS_REGISTERED.call_once(|| saluki_metadata::set_app_details(APP_DETAILS));

            let (guard_tx, guard_rx) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("facade-test-bootstrap".to_string())
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("bootstrap runtime should be built");
                    runtime.block_on(async move {
                        let Bootstrap { mut supervisor, guard } = AppBootstrapper::new()
                            .bootstrap()
                            .await
                            .expect("process should bootstrap");
                        guard_tx.send(guard).expect("bootstrap guard should be received");
                        let _ = supervisor.run_with_shutdown(std::future::pending::<()>()).await;
                    });
                })
                .expect("bootstrap thread should be spawned");

            tokio::sync::Mutex::new(guard_rx.recv().expect("process should bootstrap"))
        })
        .lock()
        .await
}

enum TestAgent {}

impl RemoteAgentSpecification for TestAgent {
    type LocalConfiguration = TestLocalConfig;
    type AuthoritativeConfiguration = TestConfig;
    type ConfigurationUpdates = TestConfigUpdates;
}

#[derive(Clone)]
struct Settings {
    standalone: bool,
    agent: Option<RemoteAgentClientConfiguration>,
    hostname: Option<String>,
    log_level: String,
    remote_config_enabled: bool,
    memory_limit: Option<ByteSize>,
    reject: bool,
}

impl Settings {
    fn standalone() -> Self {
        Self {
            standalone: true,
            agent: None,
            hostname: Some(STANDALONE_HOSTNAME.to_string()),
            log_level: "info".to_string(),
            remote_config_enabled: false,
            memory_limit: None,
            reject: false,
        }
    }

    fn connected(agent: &FakeAgent) -> Self {
        Self {
            standalone: false,
            agent: Some(agent.client_configuration()),
            hostname: None,
            ..Self::standalone()
        }
    }

    fn client_configuration(&self) -> Result<RemoteAgentClientConfiguration, GenericError> {
        self.agent
            .clone()
            .ok_or_else(|| generic_error!("No Datadog Agent is configured."))
    }
}

/// The configuration methods that the facade called, in order.
#[derive(Clone, Default)]
struct Calls(Arc<Mutex<Vec<&'static str>>>);

impl Calls {
    fn record(&self, call: &'static str) {
        self.0.lock().unwrap().push(call);
    }

    fn get(&self) -> Vec<&'static str> {
        self.0.lock().unwrap().clone()
    }
}

struct TestLocalConfig {
    settings: Settings,
    calls: Calls,
}

impl TestLocalConfig {
    fn into_authoritative(self) -> TestConfig {
        TestConfig {
            settings: self.settings,
            calls: self.calls,
        }
    }
}

impl LocalConfiguration<TestAgent> for TestLocalConfig {
    fn is_standalone(&self) -> bool {
        self.calls.record("is_standalone");
        self.settings.standalone
    }

    fn client_configuration(&self) -> Result<RemoteAgentClientConfiguration, GenericError> {
        self.calls.record("client_configuration");
        self.settings.client_configuration()
    }

    fn advertised_secure_api_address(&self) -> Result<ListenAddress, GenericError> {
        self.calls.record("advertised_secure_api_address");
        ListenAddress::try_from(ADVERTISED_SECURE_API_ADDRESS).map_err(|e| generic_error!("{}", e))
    }

    async fn connect(
        mut self, mut config_stream: mpsc::Receiver<ConfigUpdate>,
    ) -> Result<(TestConfig, TestConfigUpdates), GenericError> {
        self.calls.record("connect");
        if self.settings.reject {
            return Err(generic_error!("Configuration rejected."));
        }

        let Some(ConfigUpdate::Snapshot(snapshot)) = config_stream.recv().await else {
            return Err(generic_error!("Expected an initial snapshot."));
        };
        for setting in snapshot {
            if let ("log_level", serde_json::Value::String(level)) = (setting.key.as_str(), setting.value) {
                self.settings.log_level = level;
            }
        }

        let updates = TestConfigUpdates {
            stream: Arc::new(tokio::sync::Mutex::new(config_stream)),
        };
        Ok((self.into_authoritative(), updates))
    }

    async fn into_standalone(self) -> Result<TestConfig, GenericError> {
        self.calls.record("into_standalone");
        if self.settings.reject {
            return Err(generic_error!("Configuration rejected."));
        }

        Ok(self.into_authoritative())
    }
}

struct TestConfig {
    settings: Settings,
    calls: Calls,
}

impl AuthoritativeConfiguration for TestConfig {
    fn is_standalone(&self) -> bool {
        self.settings.standalone
    }

    fn client_configuration(&self) -> Result<RemoteAgentClientConfiguration, GenericError> {
        self.settings.client_configuration()
    }

    fn logging(&self) -> LoggingSettings {
        self.calls.record("logging");
        LoggingSettings {
            level: self.settings.log_level.clone(),
            disable_file_logging: true,
            ..Default::default()
        }
    }

    fn workload(&self) -> WorkloadSettings {
        WorkloadSettings::default()
    }

    fn host(&self) -> HostSettings {
        HostSettings {
            hostname: self.settings.hostname.clone(),
        }
    }

    fn remote_config_enabled(&self) -> bool {
        self.settings.remote_config_enabled
    }

    fn memory_bounds(&self) -> MemoryBoundsConfiguration {
        MemoryBoundsConfiguration {
            memory_limit: self.settings.memory_limit,
            memory_slop_factor: 0.25,
            enable_global_limiter: false,
            memory_mode: if self.settings.memory_limit.is_some() {
                MemoryMode::Strict
            } else {
                MemoryMode::Disabled
            },
        }
    }
}

struct TestConfigUpdates {
    stream: Arc<tokio::sync::Mutex<mpsc::Receiver<ConfigUpdate>>>,
}

impl ConfigUpdatesRunner for TestConfigUpdates {
    fn run(&self) -> impl Future<Output = Result<(), GenericError>> + Send + 'static {
        let stream = Arc::clone(&self.stream);
        async move {
            let mut stream = stream.lock().await;
            while stream.recv().await.is_some() {}
            Err(generic_error!("Configuration stream closed."))
        }
    }
}

/// Runs until shutdown.
struct Idle;

#[async_trait]
impl Supervisable for Idle {
    fn name(&self) -> &str {
        "idle"
    }

    async fn initialize(&self, shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        Ok(Box::pin(async move {
            shutdown.await;
            Ok(())
        }))
    }
}

/// Starts a remote agent, standing in for the bootstrap supervisor with `test-bootstrap`.
async fn start(
    bootstrap_guard: &mut BootstrapGuard, settings: Settings, calls: &Calls,
) -> Result<RemoteAgent<TestAgent, Started>, GenericError> {
    let local = TestLocalConfig {
        settings,
        calls: calls.clone(),
    };
    let mut bootstrap_supervisor = Supervisor::new("test-bootstrap").unwrap();
    bootstrap_supervisor.add_worker(Idle);

    RemoteAgent::<TestAgent>::new(local, LoggingTranslator::new("facade-test.log"))
        .start(bootstrap_guard, bootstrap_supervisor)
        .await
}

/// Returns a minimal control plane holding just the `remote-agent` supervisor, which every binary's control plane must
/// include.
fn control_plane(agent: &RemoteAgent<TestAgent, Initialized>) -> Supervisor {
    let mut control_plane = Supervisor::new("ctrl-pln").unwrap();
    control_plane.add_worker(agent.create_remote_agent_supervisor(Vec::new(), Vec::new()).unwrap());
    control_plane
}

struct RunningAgent {
    tree: SupervisionTreeHandle,
    shutdown: oneshot::Sender<()>,
    run: JoinHandle<Result<(), GenericError>>,
}

impl RunningAgent {
    /// Runs the remote agent with an empty topology named `primary`, until [`shutdown`](Self::shutdown).
    fn spawn(agent: RemoteAgent<TestAgent, Initialized>) -> Self {
        let tree = agent.inner.root_supervisor.tree_handle();
        let control_plane = control_plane(&agent);
        let blueprint = TopologyBlueprint::new("primary", agent.component_registry());
        let (shutdown, shutdown_rx) = oneshot::channel();
        let run = tokio::spawn(agent.run_until(blueprint, control_plane, async move {
            let _ = shutdown_rx.await;
        }));

        Self { tree, shutdown, run }
    }

    /// Waits until every child of `internal-sup`, and of the control plane, is running, and returns the tree.
    ///
    /// Fails if any node has restarted, which a child that keeps failing would otherwise hide.
    async fn wait_until_running(&self) -> TreeSnapshot {
        let running = |node: &NodeSnapshot| node.state == NodeState::Running;
        let all_children_running = |node: &NodeSnapshot| running(node) && node.children.iter().all(running);

        let wait = async {
            loop {
                let snapshot = self.tree.snapshot();
                assert_no_restarts(&snapshot);
                if let Some(internal) = find_child(&snapshot.root, "internal-sup") {
                    if all_children_running(internal)
                        && find_child(internal, "ctrl-pln").is_some_and(all_children_running)
                    {
                        return snapshot;
                    }
                }

                assert!(
                    !self.run.is_finished(),
                    "remote agent stopped before its supervision tree was running:\n{}",
                    self.tree.snapshot_json()
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };

        tokio::time::timeout(WAIT_TIMEOUT, wait).await.unwrap_or_else(|_| {
            panic!(
                "timed out waiting for the supervision tree to run:\n{}",
                self.tree.snapshot_json()
            )
        })
    }

    /// Requests shutdown, and returns the result of the run once every child has stopped gracefully.
    async fn shutdown(self) -> Result<(), GenericError> {
        assert!(
            !self.run.is_finished(),
            "remote agent stopped before shutdown was requested"
        );
        self.shutdown.send(()).unwrap();
        let result = tokio::time::timeout(CLEAN_SHUTDOWN_TIMEOUT, self.run)
            .await
            .expect("remote agent should stop gracefully once shutdown is requested")
            .expect("remote agent should not panic");
        assert_no_restarts(&self.tree.snapshot());
        result
    }
}

#[track_caller]
fn assert_no_restarts(snapshot: &TreeSnapshot) {
    assert_eq!(
        snapshot.totals.restarts,
        0,
        "no node should restart:\n{}",
        serde_json::to_string_pretty(snapshot).unwrap()
    );
}

fn find_child<'a>(node: &'a NodeSnapshot, name: &str) -> Option<&'a NodeSnapshot> {
    node.children.iter().find(|child| child.name == name)
}

#[track_caller]
fn child<'a>(node: &'a NodeSnapshot, name: &str) -> &'a NodeSnapshot {
    find_child(node, name).unwrap_or_else(|| panic!("'{}' has no child '{}': {:?}", node.name, name, child_names(node)))
}

fn child_names(node: &NodeSnapshot) -> Vec<&str> {
    node.children.iter().map(|child| child.name.as_str()).collect()
}

/// Asserts the parts of the tree that don't depend on the mode, and returns `internal-sup`.
#[track_caller]
fn assert_common_tree(root: &NodeSnapshot) -> &NodeSnapshot {
    assert_eq!(root.name, "facade-test-root");
    let supervision = root.supervision.as_ref().expect("root should be a supervisor");
    assert_eq!(supervision.restart_mode, RestartMode::OneForOne);
    assert_eq!(supervision.restart_intensity, 0);
    assert_eq!(child_names(root), ["test-bootstrap", "internal-sup", "primary"]);

    let internal = child(root, "internal-sup");
    assert_eq!(child_names(child(internal, "ctrl-pln")), ["remote-agent"]);
    internal
}

#[tokio::test]
async fn standalone_runs_the_documented_tree_until_shutdown() {
    let mut bootstrap_guard = lock_bootstrap().await;
    let agent = start(&mut bootstrap_guard, Settings::standalone(), &Calls::default())
        .await
        .unwrap();
    let agent = agent.initialize().await.unwrap();

    let running = RunningAgent::spawn(agent);
    let snapshot = running.wait_until_running().await;
    let internal = assert_common_tree(&snapshot.root);
    assert_eq!(child_names(internal), ["ctrl-pln", "resource-registry"]);
    let remote_agent = child(child(internal, "ctrl-pln"), "remote-agent");
    assert_eq!(child_names(remote_agent), ["supervision-tree"]);

    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn connected_runs_config_updates_and_env_provider_under_internal_sup() {
    let mut bootstrap_guard = lock_bootstrap().await;
    let fake_agent = FakeAgent::start(&[]).await;
    let agent = start(
        &mut bootstrap_guard,
        Settings::connected(&fake_agent),
        &Calls::default(),
    )
    .await
    .unwrap();
    let agent = agent.initialize().await.unwrap();

    let running = RunningAgent::spawn(agent);
    let snapshot = running.wait_until_running().await;
    let internal = assert_common_tree(&snapshot.root);
    assert_eq!(
        child_names(internal),
        ["ctrl-pln", "resource-registry", "env-provider", "config-updates"]
    );
    assert_eq!(
        child_names(child(internal, "env-provider")),
        ["workload", "autodiscovery"]
    );
    let remote_agent = child(child(internal, "ctrl-pln"), "remote-agent");
    assert_eq!(
        child_names(remote_agent),
        [
            "supervision-tree",
            "flare-dataspace-anchor",
            "remote_agent_event_reporter",
            "remote-agent-services"
        ]
    );

    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn standalone_start_uses_only_the_local_configuration() {
    let mut bootstrap_guard = lock_bootstrap().await;
    let calls = Calls::default();
    let agent = start(&mut bootstrap_guard, Settings::standalone(), &calls)
        .await
        .unwrap();

    // Nothing reaches for the Datadog Agent, and logging keeps its bootstrap settings.
    assert_eq!(calls.get(), ["is_standalone", "into_standalone"]);
    assert!(agent.is_standalone());

    let agent = agent.initialize().await.unwrap();
    assert!(agent.is_standalone());
    assert!(agent.client_configuration().is_none());
    assert_eq!(
        agent.env_provider().host().get_hostname().await.unwrap(),
        STANDALONE_HOSTNAME
    );
}

#[tokio::test]
async fn connected_start_registers_with_the_agent() {
    let mut bootstrap_guard = lock_bootstrap().await;
    let fake_agent = FakeAgent::start(&[]).await;
    let calls = Calls::default();
    let agent = start(&mut bootstrap_guard, Settings::connected(&fake_agent), &calls)
        .await
        .unwrap();

    assert_eq!(
        calls.get(),
        [
            "is_standalone",
            "client_configuration",
            "advertised_secure_api_address",
            "connect",
            "logging"
        ]
    );
    assert!(!agent.is_standalone());

    let registrations = fake_agent.registrations();
    assert_eq!(registrations.len(), 1, "expected a single registration");
    assert_eq!(registrations[0].display_name, "Facade Test Agent");
    assert_eq!(registrations[0].api_endpoint_uri, "127.0.0.1:5101");

    let agent = agent.initialize().await.unwrap();
    assert!(!agent.is_standalone());
    assert_eq!(
        agent.client_configuration().map(|config| config.cmd_port),
        Some(fake_agent.client_configuration().cmd_port)
    );
    assert_eq!(
        agent.env_provider().host().get_hostname().await.unwrap(),
        FAKE_AGENT_HOSTNAME
    );
}

#[tokio::test]
async fn connected_start_reloads_logging_from_the_agent_configuration() {
    let mut bootstrap_guard = lock_bootstrap().await;
    let trace_enabled = || tracing::enabled!(target: "datadog_agent_runtime", Level::TRACE);
    assert!(!trace_enabled(), "bootstrap logging should not enable trace logs");

    let fake_agent = FakeAgent::start(&[("log_level", "trace")]).await;
    let agent = start(
        &mut bootstrap_guard,
        Settings::connected(&fake_agent),
        &Calls::default(),
    )
    .await
    .unwrap();
    assert_eq!(agent.config().settings.log_level, "trace");

    // The override worker applies the new level in the background.
    let reloaded = async {
        while !trace_enabled() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let result = tokio::time::timeout(WAIT_TIMEOUT, reloaded).await;

    let translator = LoggingTranslator::new("facade-test.log");
    let logging = LoggingSettings {
        disable_file_logging: true,
        ..Default::default()
    };
    bootstrap_guard
        .logging_mut()
        .reload(translator.translate(logging).unwrap())
        .await
        .unwrap();

    result.expect("logging should follow the log level from the Datadog Agent");
}

#[tokio::test]
async fn start_fails_when_the_configuration_authority_fails() {
    let mut bootstrap_guard = lock_bootstrap().await;

    let rejected = Settings {
        reject: true,
        ..Settings::standalone()
    };
    let error = start(&mut bootstrap_guard, rejected, &Calls::default())
        .await
        .err()
        .expect("standalone start should fail");
    assert_eq!(error.to_string(), "Failed to load configuration.");

    // Without a client configuration, connected mode fails before it registers.
    let calls = Calls::default();
    let unconfigured = Settings {
        standalone: false,
        ..Settings::standalone()
    };
    let error = start(&mut bootstrap_guard, unconfigured, &calls)
        .await
        .err()
        .expect("connected start without an Agent should fail");
    assert_eq!(error.to_string(), "No Datadog Agent is configured.");
    assert_eq!(calls.get(), ["is_standalone", "client_configuration"]);

    let fake_agent = FakeAgent::start(&[]).await;
    let rejected = Settings {
        reject: true,
        ..Settings::connected(&fake_agent)
    };
    let error = start(&mut bootstrap_guard, rejected, &Calls::default())
        .await
        .err()
        .expect("connected start should fail");
    assert_eq!(
        error.to_string(),
        "Failed to load initial configuration from the Datadog Agent."
    );
}

#[tokio::test]
async fn initialize_fails_without_a_hostname_in_standalone_mode() {
    let mut bootstrap_guard = lock_bootstrap().await;
    for hostname in [None, Some(String::new())] {
        let settings = Settings {
            hostname,
            ..Settings::standalone()
        };
        let agent = start(&mut bootstrap_guard, settings, &Calls::default()).await.unwrap();

        let error = agent.initialize().await.err().expect("initialize should fail");
        assert_eq!(
            error.to_string(),
            "A non-empty hostname must be configured (`hostname`) when running in standalone mode."
        );
    }
}

#[tokio::test]
async fn run_rejects_a_memory_limit_below_the_minimum_bounds_before_running() {
    let mut bootstrap_guard = lock_bootstrap().await;
    let settings = Settings {
        memory_limit: Some(ByteSize::mib(1)),
        ..Settings::standalone()
    };
    let agent = start(&mut bootstrap_guard, settings, &Calls::default()).await.unwrap();
    let agent = agent.initialize().await.unwrap();
    agent
        .component_registry()
        .bounds_builder(&SubsystemIdentifier::from_segments(["facade_test"]))
        .minimum()
        .with_fixed_amount("buffer", 64 * 1024 * 1024);

    let tree = agent.inner.root_supervisor.tree_handle();
    let control_plane = control_plane(&agent);
    let blueprint = TopologyBlueprint::new("primary", agent.component_registry());
    let result = tokio::time::timeout(
        WAIT_TIMEOUT,
        agent.run_until(blueprint, control_plane, std::future::pending()),
    )
    .await
    .expect("run should fail without waiting for shutdown");

    let error = result.expect_err("run should fail");
    assert_eq!(
        error.to_string(),
        "Configured memory limit is insufficient for the current configuration."
    );
    assert_eq!(tree.snapshot().root.state, NodeState::Registered);
}

#[tokio::test]
async fn remote_config_client_requires_connected_mode_with_remote_config_enabled() {
    let mut bootstrap_guard = lock_bootstrap().await;

    let standalone = Settings {
        remote_config_enabled: true,
        ..Settings::standalone()
    };
    let agent = start(&mut bootstrap_guard, standalone, &Calls::default())
        .await
        .unwrap();
    let mut agent = agent.initialize().await.unwrap();
    assert!(agent.remote_config_client().await.unwrap().is_none());

    let fake_agent = FakeAgent::start(&[]).await;
    let agent = start(
        &mut bootstrap_guard,
        Settings::connected(&fake_agent),
        &Calls::default(),
    )
    .await
    .unwrap();
    let mut agent = agent.initialize().await.unwrap();
    assert!(agent.remote_config_client().await.unwrap().is_none());
}

#[tokio::test]
async fn remote_config_worker_runs_last_under_internal_sup_once_requested() {
    let mut bootstrap_guard = lock_bootstrap().await;
    let fake_agent = FakeAgent::start(&[]).await;
    let settings = Settings {
        remote_config_enabled: true,
        ..Settings::connected(&fake_agent)
    };
    let agent = start(&mut bootstrap_guard, settings, &Calls::default()).await.unwrap();
    let mut agent = agent.initialize().await.unwrap();
    assert!(agent.remote_config_client().await.unwrap().is_some());
    assert!(agent.remote_config_client().await.unwrap().is_some());

    let running = RunningAgent::spawn(agent);
    let snapshot = running.wait_until_running().await;
    let internal = assert_common_tree(&snapshot.root);
    assert_eq!(
        child_names(internal),
        [
            "ctrl-pln",
            "resource-registry",
            "env-provider",
            "config-updates",
            "remote-config"
        ]
    );
    assert_eq!(child_names(child(internal, "remote-config")), ["remote-config"]);

    running.shutdown().await.unwrap();
}
