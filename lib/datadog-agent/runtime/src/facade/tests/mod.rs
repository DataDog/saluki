use std::{sync::OnceLock, time::Duration};

use saluki_app::{
    accounting::{MemoryBoundsConfiguration, MemoryMode},
    bootstrap::{AppBootstrapper, Bootstrap},
};
use saluki_config::dynamic::ConfigUpdate;
use saluki_core::runtime::{worker, NodeSnapshot, NodeState, SupervisionTreeHandle, TreeSnapshot};
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
const EXTRA_SERVICE_NAME: &str = "facade.test.v1.Extra";
const STANDALONE_HOSTNAME: &str = "facade-test-host";
const WAIT_TIMEOUT: Duration = Duration::from_secs(30);
// Below the default graceful shutdown timeout of 5s, so a child that ignores shutdown fails the test instead of being
// aborted, which `run_until` still reports as success.
const CLEAN_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(4);

/// Locks the process-wide bootstrap, creating it on first use.
///
/// A process can only be bootstrapped once, so every test shares one bootstrap. Its supervisor runs on a thread of its
/// own, so that the logging override worker outlives the runtime of each test.
async fn lock_bootstrap() -> MutexGuard<'static, BootstrapGuard> {
    static BOOTSTRAP: OnceLock<tokio::sync::Mutex<BootstrapGuard>> = OnceLock::new();

    BOOTSTRAP
        .get_or_init(|| {
            saluki_metadata::set_app_details(APP_DETAILS);
            let (guard_tx, guard_rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
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
            });
            tokio::sync::Mutex::new(guard_rx.recv().expect("process should bootstrap"))
        })
        .lock()
        .await
}

enum TestAgent {}

impl RemoteAgentSpecification for TestAgent {
    type LocalConfiguration = TestConfig;
    type AuthoritativeConfiguration = TestConfig;
    type ConfigurationUpdates = NoUpdates;

    const ADDITIONAL_SERVICE_NAMES: &'static [&'static str] = &[EXTRA_SERVICE_NAME];
}

/// Serves as both the local and the authoritative configuration, and panics if the facade uses it in a way that its
/// mode rules out.
#[derive(Default)]
struct TestConfig {
    standalone: bool,
    agent: Option<RemoteAgentClientConfiguration>,
    log_level: Option<&'static str>,
    remote_config_enabled: bool,
}

impl TestConfig {
    fn standalone() -> Self {
        Self {
            standalone: true,
            ..Default::default()
        }
    }

    fn connected(agent: &FakeAgent) -> Self {
        Self {
            agent: Some(agent.client_configuration()),
            ..Default::default()
        }
    }

    fn agent(&self) -> Result<RemoteAgentClientConfiguration, GenericError> {
        assert!(!self.standalone, "standalone mode must not reach for the Datadog Agent");
        self.agent
            .clone()
            .ok_or_else(|| generic_error!("No Datadog Agent is configured."))
    }
}

impl LocalConfiguration<TestAgent> for TestConfig {
    fn is_standalone(&self) -> bool {
        self.standalone
    }

    fn client_configuration(&self) -> Result<RemoteAgentClientConfiguration, GenericError> {
        self.agent()
    }

    fn advertised_secure_api_address(&self) -> Result<ListenAddress, GenericError> {
        ListenAddress::try_from(ADVERTISED_SECURE_API_ADDRESS).map_err(|e| generic_error!("{}", e))
    }

    async fn connect(self, mut config_stream: mpsc::Receiver<ConfigUpdate>) -> Result<(Self, NoUpdates), GenericError> {
        assert!(
            matches!(config_stream.recv().await, Some(ConfigUpdate::Snapshot(_))),
            "connected mode must hand over the configuration stream from the Datadog Agent"
        );
        Ok((self, NoUpdates))
    }

    async fn into_standalone(self) -> Result<Self, GenericError> {
        assert!(self.standalone, "connected mode must not fall back to standalone mode");
        Ok(self)
    }
}

impl AuthoritativeConfiguration for TestConfig {
    fn is_standalone(&self) -> bool {
        self.standalone
    }

    fn client_configuration(&self) -> Result<RemoteAgentClientConfiguration, GenericError> {
        self.agent()
    }

    fn logging(&self) -> LoggingSettings {
        assert!(!self.standalone, "standalone mode must not reload logging");
        LoggingSettings {
            level: self.log_level.unwrap_or("info").to_string(),
            disable_file_logging: true,
            ..Default::default()
        }
    }

    fn workload(&self) -> WorkloadSettings {
        WorkloadSettings::default()
    }

    // Connected mode must ignore this in favor of the hostname from the Datadog Agent.
    fn host(&self) -> HostSettings {
        HostSettings {
            hostname: Some(STANDALONE_HOSTNAME.to_string()),
        }
    }

    fn remote_config_enabled(&self) -> bool {
        self.remote_config_enabled
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

struct NoUpdates;

impl ConfigUpdatesRunner for NoUpdates {
    fn run(&self) -> impl Future<Output = Result<(), GenericError>> + Send + 'static {
        std::future::pending()
    }
}

/// Starts a remote agent, with an empty `test-bootstrap` standing in for the bootstrap supervisor.
async fn start(
    bootstrap_guard: &mut BootstrapGuard, config: TestConfig,
) -> Result<RemoteAgent<TestAgent, Started>, GenericError> {
    RemoteAgent::<TestAgent>::new(config, LoggingTranslator::new("facade-test.log"))
        .start(bootstrap_guard, Supervisor::new("test-bootstrap").unwrap())
        .await
}

/// Returns a control plane holding just the `remote-agent` supervisor, which every binary's control plane must include.
fn control_plane(agent: &RemoteAgent<TestAgent, Initialized>) -> Supervisor {
    let mut control_plane = Supervisor::new("ctrl-pln").unwrap();
    control_plane.add_worker(
        agent
            .create_remote_agent_supervisor(Vec::new(), Vec::new(), |routes, _| routes)
            .unwrap(),
    );
    control_plane
}

struct RunningAgent {
    tree: SupervisionTreeHandle,
    shutdown: oneshot::Sender<()>,
    run: JoinHandle<Result<(), GenericError>>,
}

impl RunningAgent {
    /// Runs the remote agent with an empty topology named `primary`, until [`shutdown`](Self::shutdown).
    fn spawn(agent: RemoteAgent<TestAgent, Initialized>, control_plane: Supervisor) -> Self {
        let tree = agent.inner.root_supervisor.tree_handle();
        let blueprint = TopologyBlueprint::new("primary", agent.component_registry());
        let (shutdown, shutdown_rx) = oneshot::channel();
        let run = tokio::spawn(agent.run_until(blueprint, control_plane, async move {
            let _ = shutdown_rx.await;
        }));

        Self { tree, shutdown, run }
    }

    /// Waits until everything under `internal-sup` is running, and returns the tree.
    ///
    /// Fails if any node has restarted, which a child that keeps failing would otherwise hide.
    async fn wait_until_running(&self) -> TreeSnapshot {
        fn all_running(node: &NodeSnapshot) -> bool {
            node.state == NodeState::Running && node.children.iter().all(all_running)
        }

        let wait = async {
            loop {
                let snapshot = self.tree.snapshot();
                assert_no_restarts(&snapshot);
                let internal = snapshot.root.children.iter().find(|child| child.name == "internal-sup");
                if internal.is_some_and(all_running) {
                    return snapshot;
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

    /// Requests shutdown, and returns the result of the run once every child has stopped.
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

/// Returns every node below `node`, as paths such as `internal-sup/ctrl-pln`.
fn paths(node: &NodeSnapshot) -> Vec<String> {
    node.children
        .iter()
        .flat_map(|child| {
            std::iter::once(child.name.clone())
                .chain(paths(child).into_iter().map(|path| format!("{}/{path}", child.name)))
        })
        .collect()
}

#[tokio::test]
async fn standalone_runs_the_local_tree_until_shutdown() {
    let mut bootstrap_guard = lock_bootstrap().await;
    let config = TestConfig {
        remote_config_enabled: true,
        ..TestConfig::standalone()
    };
    let agent = start(&mut bootstrap_guard, config).await.unwrap();
    let mut agent = agent.initialize().await.unwrap();
    assert_eq!(
        agent.env_provider().host().get_hostname().await.unwrap(),
        STANDALONE_HOSTNAME
    );
    assert!(agent.remote_config_client().await.unwrap().is_none());

    // A child that ignores shutdown makes the shutdown unclean, which `run_until` must still report as success.
    let mut control_plane = control_plane(&agent);
    control_plane.add_worker(
        worker("stuck", std::future::pending::<()>())
            .with_shutdown_timeout(Duration::from_millis(100))
            .build(),
    );

    let running = RunningAgent::spawn(agent, control_plane);
    let snapshot = running.wait_until_running().await;
    let root = &snapshot.root;
    let supervision = root.supervision.as_ref().expect("root should be a supervisor");
    assert_eq!(supervision.restart_mode, RestartMode::OneForOne);
    assert_eq!(supervision.restart_intensity, 0);
    let children: Vec<_> = root.children.iter().map(|child| child.name.as_str()).collect();
    assert_eq!(children, ["test-bootstrap", "internal-sup", "primary"]);
    assert_eq!(
        paths(&root.children[1]),
        [
            "ctrl-pln",
            "ctrl-pln/remote-agent",
            "ctrl-pln/remote-agent/supervision-tree",
            "ctrl-pln/stuck",
            "resource-registry",
        ]
    );

    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn connected_registers_and_runs_the_agent_workers_until_shutdown() {
    let mut bootstrap_guard = lock_bootstrap().await;
    let fake_agent = FakeAgent::start().await;
    let config = TestConfig {
        remote_config_enabled: true,
        ..TestConfig::connected(&fake_agent)
    };
    let agent = start(&mut bootstrap_guard, config).await.unwrap();

    let registrations = fake_agent.registrations();
    let [registration] = &registrations[..] else {
        panic!("expected a single registration: {registrations:?}");
    };
    assert_eq!(registration.api_endpoint_uri, "127.0.0.1:5101");
    assert!(
        registration
            .services
            .iter()
            .any(|service| service == EXTRA_SERVICE_NAME),
        "additional service names should be registered: {:?}",
        registration.services
    );

    let mut agent = agent.initialize().await.unwrap();
    assert_eq!(
        agent.env_provider().host().get_hostname().await.unwrap(),
        FAKE_AGENT_HOSTNAME
    );
    let first = agent
        .remote_config_client()
        .await
        .unwrap()
        .expect("remote config client");
    let second = agent
        .remote_config_client()
        .await
        .unwrap()
        .expect("remote config client");
    // `Debug` shows the client's random ID, so equal output means the client was reused rather than recreated.
    assert_eq!(format!("{first:?}"), format!("{second:?}"));

    let control_plane = control_plane(&agent);
    let running = RunningAgent::spawn(agent, control_plane);
    let snapshot = running.wait_until_running().await;
    let paths = paths(&snapshot.root);
    for expected in [
        "internal-sup/env-provider/workload",
        "internal-sup/env-provider/autodiscovery",
        "internal-sup/config-updates",
        "internal-sup/remote-config/remote-config",
        "internal-sup/ctrl-pln/remote-agent/supervision-tree",
        "internal-sup/ctrl-pln/remote-agent/flare-dataspace-anchor",
        "internal-sup/ctrl-pln/remote-agent/remote_agent_event_reporter",
        "internal-sup/ctrl-pln/remote-agent/remote-agent-services",
    ] {
        assert!(
            paths.iter().any(|path| path == expected),
            "missing {expected}: {paths:#?}"
        );
    }

    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn connected_start_reloads_logging_from_the_authoritative_configuration() {
    let mut bootstrap_guard = lock_bootstrap().await;
    let trace_enabled = || tracing::enabled!(target: "datadog_agent_runtime", Level::TRACE);
    assert!(!trace_enabled(), "bootstrap logging should not enable trace logs");

    let fake_agent = FakeAgent::start().await;
    let config = TestConfig {
        log_level: Some("trace"),
        ..TestConfig::connected(&fake_agent)
    };
    start(&mut bootstrap_guard, config).await.unwrap();

    // The override worker applies the new level in the background. Restore the bootstrap level before asserting, so a
    // failure doesn't leak trace logging into later tests.
    let reloaded = async {
        while !trace_enabled() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let result = tokio::time::timeout(WAIT_TIMEOUT, reloaded).await;
    let logging = LoggingSettings {
        disable_file_logging: true,
        ..Default::default()
    };
    bootstrap_guard
        .logging_mut()
        .reload(LoggingTranslator::new("facade-test.log").translate(logging).unwrap())
        .await
        .unwrap();

    result.expect("logging should follow the log level from the authoritative configuration");
}

#[tokio::test]
async fn connected_start_without_an_agent_fails() {
    let mut bootstrap_guard = lock_bootstrap().await;
    // `TestConfig` panics if this falls back to standalone mode instead.
    let error = start(&mut bootstrap_guard, TestConfig::default())
        .await
        .err()
        .expect("connected start without an Agent should fail");
    assert_eq!(error.to_string(), "No Datadog Agent is configured.");
}
