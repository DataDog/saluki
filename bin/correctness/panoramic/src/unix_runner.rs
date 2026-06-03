//! Unix-process integration test runner.
//!
//! This runner is the parallel of [`crate::runner::IntegrationRunner`] but for tests declared
//! with `runtime: mac` (and, in the future, any other Unix host runtime opted in). Instead of
//! building a Docker container, it spawns binaries directly via
//! [`airlock::unix::UnixProcess`] and feeds their stdout/stderr into the same
//! [`LogBuffer`][crate::assertions::LogBuffer] used by the Docker path so the assertions work
//! unchanged.
//!
//! # Supported test shapes
//!
//! The Datadog Core Agent is always spawned alongside ADP, matching the Docker integration image's
//! fixture shape. Tests still control ADP behavior through configuration: standalone-mode tests set
//! `DD_DATA_PLANE_STANDALONE_MODE=true`, while converged tests enable remote-agent/config-stream
//! behavior. Both processes share a per-test config directory so they authenticate over IPC the same
//! way they would in production. See the per-phase comments in [`UnixIntegrationRunner::run`] for
//! the cert/auth_token plumbing.
//!
//! # Binary discovery
//!
//! - ADP: `ADP_BINARY_PATH` env var, default `target/release/agent-data-plane` (resolved
//!   relative to the current working directory).
//! - Core Agent (converged only): `CORE_AGENT_BINARY_PATH` env var, default
//!   `/tmp/saluki-dda/datadog-agent/bin/agent/agent` (the sandbox install written by
//!   `make provision-macos-test-env`). Set the env var explicitly to point at a different
//!   install (for example, a system-wide `/opt/datadog-agent` on a developer host).

use std::sync::RwLock;
use std::{
    collections::HashMap,
    ops::Deref,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use airlock::unix::{LogSink, UnixProcess, UnixProcessConfig};
use rand::distr::SampleString as _;
use saluki_error::{ErrorContext as _, GenericError};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::{
    assertions::{AssertionContext, AssertionResult, LogBuffer},
    config::{parse_port_spec, IntegrationConfig},
    reporter::{PhaseTiming, TestResult},
    test::{Test, TestContext},
};

const ADP_BINARY_ENV_VAR: &str = "ADP_BINARY_PATH";
const DEFAULT_ADP_BINARY_PATH: &str = "target/release/agent-data-plane";

const CORE_AGENT_BINARY_ENV_VAR: &str = "CORE_AGENT_BINARY_PATH";
const DEFAULT_CORE_AGENT_BINARY_PATH: &str = "/tmp/saluki-dda/datadog-agent/bin/agent/agent";

/// How long to wait for the Core Agent to write its `auth_token` and `ipc_cert.pem` before
/// giving up and failing the test.
const CORE_AGENT_IPC_READY_TIMEOUT: Duration = Duration::from_secs(60);
const CORE_AGENT_IPC_READY_POLL: Duration = Duration::from_millis(200);

/// Builds the env for a target process (Core Agent or ADP) under the Unix runner.
///
/// Precedence (lowest to highest):
///   1. framework port-isolation defaults (`crate::test_env::port_isolation_env`)
///   2. the test's top-level `env` block
///   3. forced overrides supplied by the caller (auth token path, run path, …)
///
/// Forced overrides are bottom-of-stack from the framework's perspective but top-of-stack here
/// because they're path-bindings tests must not be able to override (they identify per-test
/// state directories that the runner owns).
fn build_process_env(test_env: &HashMap<String, String>, forced: &[(&str, String)]) -> HashMap<String, String> {
    let mut env = crate::test_env::port_isolation_env();
    for (k, v) in test_env {
        env.insert(k.clone(), v.clone());
    }
    for (k, v) in forced {
        env.insert((*k).to_string(), v.clone());
    }
    env
}

/// Runner for a single Unix-process integration test case.
pub(crate) struct UnixIntegrationRunner {
    test_case: IntegrationConfig,
    tctx: TestContext,
    log_buffer: Arc<RwLock<LogBuffer>>,
}

impl UnixIntegrationRunner {
    /// Creates a new runner for the given test case.
    pub(crate) fn new(test_case: IntegrationConfig, tctx: TestContext) -> Self {
        Self {
            test_case,
            tctx,
            log_buffer: Arc::new(RwLock::new(LogBuffer::default())),
        }
    }

    /// Runs the test case and returns the result.
    pub(crate) async fn run(&mut self) -> TestResult {
        let started = Instant::now();
        let test_name = self.test_case.name();
        let mut phase_timings = Vec::new();

        info!(test = %test_name, "Starting Unix integration test case.");

        // Phase: resolve binary path.
        let binary_path = match resolve_adp_binary_path() {
            Ok(p) => p,
            Err(e) => return make_error_result(test_name, started, "resolve_binary", e, phase_timings),
        };
        debug!(test = %test_name, binary = %binary_path.display(), "Resolved ADP binary path.");

        // Create a per-test state directory and seed it with an empty datadog.yaml. ADP's
        // bootstrap loader requires the file to exist; tests communicate config through env
        // vars, so the file itself is intentionally empty.
        let state_dir = match create_test_state_dir() {
            Ok(d) => d,
            Err(e) => return make_error_result(test_name, started, "prepare_state_dir", e, phase_timings),
        };
        let config_path = state_dir.join("datadog.yaml");
        if let Err(e) = std::fs::write(&config_path, b"") {
            return make_error_result(
                test_name,
                started,
                "prepare_state_dir",
                saluki_error::generic_error!(
                    "Failed to write empty datadog.yaml at '{}': {}",
                    config_path.display(),
                    e
                ),
                phase_timings,
            );
        }
        debug!(test = %test_name, state_dir = %state_dir.display(), "Prepared per-test state directory.");

        // Only ADP's exit lifecycle is observable to assertions. The Core Agent (when present)
        // gets a throwaway token at spawn time — it satisfies `UnixProcess::spawn`'s
        // signature but nothing consumes the resulting cancellation. If the Agent dies
        // independently it's treated as an environmental fault, not a test signal.
        let adp_exit_token = CancellationToken::new();
        let log_sink: Arc<Mutex<dyn LogSink>> = Arc::new(Mutex::new(PanoramicLogSink {
            buf: self.log_buffer.clone(),
        }));

        // Path that both the Agent and ADP use for auth_token / ipc_cert.pem.
        let auth_token_path = state_dir.join("auth_token").to_string_lossy().into_owned();

        // Phase: spawn the Core Agent.
        //
        // The Docker integration image always runs the Core Agent beside ADP via s6. Do the
        // same for the Unix runner so mac tests keep the same fixture shape: standalone-mode
        // tests still configure ADP not to use the Agent, but the Agent process exists.
        let agent_spawn_start = Instant::now();
        let agent_binary = match resolve_core_agent_binary_path() {
            Ok(p) => p,
            Err(e) => return make_error_result(test_name, started, "resolve_core_agent", e, phase_timings),
        };
        debug!(test = %test_name, binary = %agent_binary.display(), "Resolved Core Agent binary path.");

        // Forced runner-owned bindings:
        //   DD_AUTH_TOKEN_FILE_PATH — pin Agent + ADP to the same per-test path. The Agent's
        //     authoritative config (sent to ADP via the config stream) overrides ADP's env
        //     vars, so the Agent itself must be told about the per-test path; otherwise it
        //     advertises the platform default (`/opt/datadog-agent/etc/auth_token`), ADP
        //     follows that advice for its post-config-stream IPC clients, and TLS fails
        //     with UnknownIssuer because the platform default cert does not match what the
        //     per-test Agent is actually serving.
        //   DD_RUN_PATH — Agent's default `run_path` is the install prefix's `run/` dir
        //     (e.g., /opt/datadog-agent/run). Without overriding, a relocated Agent install
        //     would try to write its runtime state (remote-config db, sockets, pid file)
        //     back to /opt — typically not writable in CI. Scope it to the per-test state
        //     directory so each test gets a clean slate and nothing leaks across runs.
        //   DD_USE_DOGSTATSD / OTLP endpoint overrides — mirror the Docker cont-init script's
        //     collision avoidance when ADP owns those listeners.
        let agent_forced = build_core_agent_forced_env(&self.test_case.env, &state_dir, auth_token_path.clone());
        let agent_env = build_process_env(&self.test_case.env, &agent_forced);

        let agent_config = UnixProcessConfig::new(format!("{}-core-agent", self.test_case.name), agent_binary)
            .with_args(vec![
                "run".to_string(),
                "-c".to_string(),
                state_dir.to_string_lossy().into_owned(),
            ])
            .with_env_map(agent_env);

        let agent = match UnixProcess::spawn(agent_config, log_sink.clone(), CancellationToken::new()).await {
            Ok(p) => p,
            Err(e) => {
                phase_timings.push(PhaseTiming {
                    phase: "core_agent_spawn".to_string(),
                    duration: agent_spawn_start.elapsed(),
                });
                return make_error_result(test_name, started, "core_agent_spawn", e, phase_timings);
            }
        };
        phase_timings.push(PhaseTiming {
            phase: "core_agent_spawn".to_string(),
            duration: agent_spawn_start.elapsed(),
        });
        info!(test = %test_name, "Core Agent process started.");

        let wait_start = Instant::now();
        if let Err(e) = wait_for_agent_ipc_ready(&state_dir, CORE_AGENT_IPC_READY_TIMEOUT).await {
            agent.cleanup().await;
            phase_timings.push(PhaseTiming {
                phase: "core_agent_ipc_ready".to_string(),
                duration: wait_start.elapsed(),
            });
            return make_error_result(test_name, started, "core_agent_ipc_ready", e, phase_timings);
        }
        phase_timings.push(PhaseTiming {
            phase: "core_agent_ipc_ready".to_string(),
            duration: wait_start.elapsed(),
        });
        debug!(test = %test_name, "Core Agent IPC credentials present.");
        let mut core_agent = Some(agent);

        // Phase: spawn ADP.
        let spawn_start = Instant::now();
        let config_path_str = config_path.to_string_lossy().into_owned();
        let adp_forced = build_adp_forced_env(auth_token_path);
        let adp_env = build_process_env(&self.test_case.env, &adp_forced);
        let process_config = UnixProcessConfig::new(self.test_case.name.clone(), binary_path)
            .with_args(vec!["-c".to_string(), config_path_str, "run".to_string()])
            .with_env_map(adp_env);

        let process = match UnixProcess::spawn(process_config, log_sink, adp_exit_token.clone()).await {
            Ok(p) => p,
            Err(e) => {
                if let Some(agent) = core_agent.take() {
                    agent.cleanup().await;
                }
                phase_timings.push(PhaseTiming {
                    phase: "spawn".to_string(),
                    duration: spawn_start.elapsed(),
                });
                return make_error_result(test_name, started, "spawn", e, phase_timings);
            }
        };
        phase_timings.push(PhaseTiming {
            phase: "spawn".to_string(),
            duration: spawn_start.elapsed(),
        });

        info!(test = %test_name, "ADP process started.");

        // Phase: run assertions.
        let assertion_start = Instant::now();
        let assertion_results = self
            .run_assertions(
                process.name().to_string(),
                adp_exit_token.clone(),
                process.exit_code_cell(),
            )
            .await;
        phase_timings.push(PhaseTiming {
            phase: "assertions".to_string(),
            duration: assertion_start.elapsed(),
        });

        // Phase: cleanup. ADP first, Core Agent second — in case the Agent's shutdown depends on
        // ADP releasing connections gracefully.
        let cleanup_start = Instant::now();
        process.cleanup().await;
        if let Some(agent) = core_agent.take() {
            agent.cleanup().await;
        }
        phase_timings.push(PhaseTiming {
            phase: "cleanup".to_string(),
            duration: cleanup_start.elapsed(),
        });

        // Phase: write captured logs to disk so the artifact upload picks them up. Matches the
        // Docker runner's behavior; without this the artifact only contains result.log and a
        // failed assertion's truncated context is all we have to debug from.
        let write_logs_start = Instant::now();
        if let Err(e) = self.write_logs().await {
            debug!(test = %test_name, error = %e, "Failed to write captured logs to disk.");
        }
        phase_timings.push(PhaseTiming {
            phase: "write_logs".to_string(),
            duration: write_logs_start.elapsed(),
        });

        let passed = assertion_results.iter().all(|r| r.passed);
        TestResult {
            name: test_name,
            passed,
            duration: started.elapsed(),
            assertion_results,
            error: None,
            phase_timings,
            assertion_details: Vec::new(),
        }
    }

    /// Builds the port mappings for assertions. In the Docker runner this maps container ports
    /// to host ports allocated by Docker. As a host process there is no remapping: a port
    /// declared in `exposed_ports` is reachable on the host at the same number. We populate
    /// identity entries so the existing `port_listening` assertion (which expects every probed
    /// port to appear in the mapping) works unchanged.
    fn build_port_mappings(&self) -> HashMap<String, u16> {
        let mut mappings = HashMap::new();
        for spec in &self.test_case.container.exposed_ports {
            if let Ok((port, protocol)) = parse_port_spec(spec) {
                mappings.insert(format!("{}/{}", port, protocol), port);
            }
        }
        mappings
    }

    async fn write_logs(&self) -> Result<(), GenericError> {
        use std::io::Write as _;

        let log_dir = self.tctx.log_dir();
        let buffer = self.log_buffer.read().unwrap();

        let stdout_path = log_dir.join("stdout.log");
        let mut stdout_file = std::fs::File::create(&stdout_path)
            .with_error_context(|| format!("Failed to create stdout log at '{}'.", stdout_path.display()))?;
        for line in &buffer.stdout {
            writeln!(stdout_file, "{}", line).error_context("Failed to write stdout log line.")?;
        }

        let stderr_path = log_dir.join("stderr.log");
        let mut stderr_file = std::fs::File::create(&stderr_path)
            .with_error_context(|| format!("Failed to create stderr log at '{}'.", stderr_path.display()))?;
        for line in &buffer.stderr {
            writeln!(stderr_file, "{}", line).error_context("Failed to write stderr log line.")?;
        }

        Ok(())
    }

    async fn run_assertions(
        &self, process_display_name: String, exit_token: CancellationToken, exit_code_cell: airlock::unix::ExitCodeCell,
    ) -> Vec<AssertionResult> {
        let ctx = AssertionContext {
            log_buffer: self.log_buffer.clone(),
            container_exit_token: exit_token,
            cancel_token: self.tctx.test_cancel_token(),
            port_mappings: self.build_port_mappings(),
            container_name: process_display_name,
            is_host_process: true,
            host_process_exit_code: Some(exit_code_cell),
        };
        crate::assertions::run_assertion_steps(&self.test_case, &ctx).await
    }
}

fn resolve_adp_binary_path() -> Result<PathBuf, GenericError> {
    let raw = std::env::var(ADP_BINARY_ENV_VAR)
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_ADP_BINARY_PATH));

    raw.canonicalize().with_error_context(|| {
        format!(
            "ADP binary not found at '{}'. Set {} or run `cargo build --release --bin agent-data-plane`.",
            raw.display(),
            ADP_BINARY_ENV_VAR
        )
    })
}

fn resolve_core_agent_binary_path() -> Result<PathBuf, GenericError> {
    let raw = std::env::var(CORE_AGENT_BINARY_ENV_VAR)
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CORE_AGENT_BINARY_PATH));

    raw.canonicalize().with_error_context(|| {
        format!(
            "Core Agent binary not found at '{}'. Set {} or install the Datadog Agent (https://docs.datadoghq.com/agent/).",
            raw.display(),
            CORE_AGENT_BINARY_ENV_VAR
        )
    })
}

fn build_core_agent_forced_env(
    test_env: &HashMap<String, String>, state_dir: &Path, auth_token_path: String,
) -> Vec<(&'static str, String)> {
    let mut forced = vec![
        ("DD_AUTH_TOKEN_FILE_PATH", auth_token_path),
        ("DD_RUN_PATH", state_dir.to_string_lossy().into_owned()),
    ];

    if adp_owns_dogstatsd(test_env) {
        forced.push(("DD_USE_DOGSTATSD", "false".to_string()));
    }

    if env_is_true(test_env, "DD_DATA_PLANE_OTLP_ENABLED") {
        forced.extend([
            (
                "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT",
                "127.0.0.1:14317".to_string(),
            ),
            (
                "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_ENDPOINT",
                "127.0.0.1:14318".to_string(),
            ),
        ]);
    }

    forced
}

fn build_adp_forced_env(auth_token_path: String) -> Vec<(&'static str, String)> {
    vec![("DD_AUTH_TOKEN_FILE_PATH", auth_token_path)]
}

fn adp_owns_dogstatsd(env: &HashMap<String, String>) -> bool {
    env_is_true(env, "DD_DATA_PLANE_ENABLED") && env.get("DD_DATA_PLANE_DOGSTATSD_ENABLED").is_none_or(|v| v != "false")
}

fn env_is_true(env: &HashMap<String, String>, key: &str) -> bool {
    env.get(key).is_some_and(|v| v == "true")
}

async fn wait_for_agent_ipc_ready(state_dir: &Path, timeout: Duration) -> Result<(), GenericError> {
    let auth_token = state_dir.join("auth_token");
    let ipc_cert = state_dir.join("ipc_cert.pem");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if auth_token.is_file() && ipc_cert.is_file() {
            return Ok(());
        }
        tokio::time::sleep(CORE_AGENT_IPC_READY_POLL).await;
    }
    Err(saluki_error::generic_error!(
        "Core Agent did not write 'auth_token' and 'ipc_cert.pem' to '{}' within {:?}.",
        state_dir.display(),
        timeout
    ))
}

struct TestStateDir {
    path: PathBuf,
}

impl TestStateDir {
    fn create() -> Result<Self, GenericError> {
        let suffix = rand::distr::Alphanumeric
            .sample_string(&mut rand::rng(), 8)
            .to_lowercase();
        let path = std::env::temp_dir().join(format!("panoramic-unix-{}", suffix));
        std::fs::create_dir_all(&path)
            .with_error_context(|| format!("Failed to create state directory '{}'.", path.display()))?;
        Ok(Self { path })
    }
}

impl Deref for TestStateDir {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl Drop for TestStateDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.path) {
            debug!(state_dir = %self.path.display(), error = %e, "Failed to remove per-test state directory.");
        }
    }
}

fn create_test_state_dir() -> Result<TestStateDir, GenericError> {
    TestStateDir::create()
}

fn make_error_result(
    name: String, started: Instant, phase: &str, e: GenericError, phase_timings: Vec<PhaseTiming>,
) -> TestResult {
    error!(test = %name, error = %e, phase, "Unix integration test setup failed.");
    TestResult {
        name,
        passed: false,
        duration: started.elapsed(),
        assertion_results: vec![],
        error: Some(format!("Failed in phase '{}': {}", phase, e)),
        phase_timings,
        assertion_details: vec![],
    }
}

/// Bridges [`airlock::unix::LogSink`] to the panoramic [`LogBuffer`].
struct PanoramicLogSink {
    buf: Arc<RwLock<LogBuffer>>,
}

impl LogSink for PanoramicLogSink {
    fn push_line(&mut self, line: String, is_stderr: bool) {
        // Synchronous write into the shared LogBuffer. The lock is briefly contended with
        // assertion readers; the critical section is just a Vec::push, so it's cheap. Doing
        // this synchronously (rather than spawning a tokio task per line) guarantees the
        // buffer is up-to-date by the time the assertion polls.
        let mut buf = self.buf.write().unwrap();
        if is_stderr {
            buf.stderr.push(line);
        } else {
            buf.stdout.push(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adp_env_points_ipc_credentials_at_test_state_dir() {
        let auth_token_path = "/tmp/panoramic-unix-test/auth_token".to_string();

        let env: HashMap<_, _> = build_adp_forced_env(auth_token_path.clone()).into_iter().collect();

        assert_eq!(env.get("DD_AUTH_TOKEN_FILE_PATH"), Some(&auth_token_path));
    }

    #[test]
    fn core_agent_env_mirrors_docker_listener_collision_avoidance() {
        let state_dir = PathBuf::from("/tmp/panoramic-unix-test");
        let auth_token_path = state_dir.join("auth_token").to_string_lossy().into_owned();
        let test_env = HashMap::from([
            ("DD_DATA_PLANE_ENABLED".to_string(), "true".to_string()),
            ("DD_DATA_PLANE_DOGSTATSD_ENABLED".to_string(), "true".to_string()),
            ("DD_DATA_PLANE_OTLP_ENABLED".to_string(), "true".to_string()),
        ]);

        let env: HashMap<_, _> = build_core_agent_forced_env(&test_env, &state_dir, auth_token_path.clone())
            .into_iter()
            .collect();

        assert_eq!(env.get("DD_AUTH_TOKEN_FILE_PATH"), Some(&auth_token_path));
        assert_eq!(env.get("DD_RUN_PATH"), Some(&state_dir.to_string_lossy().into_owned()));
        assert_eq!(env.get("DD_USE_DOGSTATSD"), Some(&"false".to_string()));
        assert_eq!(
            env.get("DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT"),
            Some(&"127.0.0.1:14317".to_string())
        );
        assert_eq!(
            env.get("DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_ENDPOINT"),
            Some(&"127.0.0.1:14318".to_string())
        );
    }

    #[test]
    fn core_agent_dogstatsd_is_disabled_when_adp_uses_default_dogstatsd_setting() {
        let state_dir = PathBuf::from("/tmp/panoramic-unix-test");
        let auth_token_path = state_dir.join("auth_token").to_string_lossy().into_owned();
        let test_env = HashMap::from([("DD_DATA_PLANE_ENABLED".to_string(), "true".to_string())]);

        let env: HashMap<_, _> = build_core_agent_forced_env(&test_env, &state_dir, auth_token_path)
            .into_iter()
            .collect();

        assert_eq!(env.get("DD_USE_DOGSTATSD"), Some(&"false".to_string()));
    }

    #[test]
    fn core_agent_dogstatsd_is_not_disabled_when_adp_dogstatsd_is_explicitly_disabled() {
        let state_dir = PathBuf::from("/tmp/panoramic-unix-test");
        let auth_token_path = state_dir.join("auth_token").to_string_lossy().into_owned();
        let test_env = HashMap::from([
            ("DD_DATA_PLANE_ENABLED".to_string(), "true".to_string()),
            ("DD_DATA_PLANE_DOGSTATSD_ENABLED".to_string(), "false".to_string()),
        ]);

        let env: HashMap<_, _> = build_core_agent_forced_env(&test_env, &state_dir, auth_token_path)
            .into_iter()
            .collect();

        assert!(!env.contains_key("DD_USE_DOGSTATSD"));
    }
}
