//! Native-process integration test runner.
//!
//! This runner is the parallel of [`crate::runner::IntegrationRunner`] but for tests declared
//! with `runtime: native_macos`. Instead of building a Docker container, it spawns a binary
//! directly via [`airlock::native::NativeProcess`] and feeds its stdout/stderr into the same
//! [`LogBuffer`][crate::assertions::LogBuffer] used by the Docker path so the assertions work
//! unchanged.
//!
//! # Scope
//!
//! Initial scope is ADP-standalone tests: a single binary, no Core Agent, no IPC. The binary
//! path is discovered via the `ADP_BINARY_PATH` env var, falling back to
//! `target/release/agent-data-plane` (resolved relative to the current working directory).

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use airlock::native::{LogSink, NativeProcess, NativeProcessConfig};
use rand::distr::SampleString as _;
use saluki_error::{ErrorContext as _, GenericError};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::{
    assertions::{create_assertion, AssertionContext, AssertionResult, LogBuffer},
    config::{parse_port_spec, AssertionStep, IntegrationConfig},
    reporter::{PhaseTiming, TestResult},
    test::{Test, TestContext},
};

const ADP_BINARY_ENV_VAR: &str = "ADP_BINARY_PATH";
const DEFAULT_ADP_BINARY_PATH: &str = "target/release/agent-data-plane";

const CORE_AGENT_BINARY_ENV_VAR: &str = "CORE_AGENT_BINARY_PATH";
const DEFAULT_CORE_AGENT_BINARY_PATH: &str = "/opt/datadog-agent/bin/agent/agent";

/// How long to wait for the Core Agent to write its `auth_token` and `ipc_cert.pem` before
/// giving up and failing the test.
const CORE_AGENT_IPC_READY_TIMEOUT: Duration = Duration::from_secs(60);
const CORE_AGENT_IPC_READY_POLL: Duration = Duration::from_millis(200);

/// Runner for a single native-process integration test case.
pub(crate) struct NativeIntegrationRunner {
    test_case: IntegrationConfig,
    tctx: TestContext,
    log_buffer: Arc<RwLock<LogBuffer>>,
}

impl NativeIntegrationRunner {
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

        info!(test = %test_name, "Starting native integration test case.");

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
        // gets a throwaway token at spawn time — it satisfies `NativeProcess::spawn`'s
        // signature but nothing consumes the resulting cancellation. If the Agent dies
        // independently it's treated as an environmental fault, not a test signal.
        let adp_exit_token = CancellationToken::new();
        let log_sink: Arc<Mutex<dyn LogSink>> = Arc::new(Mutex::new(NativeLogSink {
            buf: self.log_buffer.clone(),
        }));

        // Path that both the Agent and ADP use for auth_token / ipc_cert.pem. Always computed,
        // only inserted into env when the Agent is in the picture (see comments below).
        let auth_token_path = state_dir.join("auth_token").to_string_lossy().into_owned();

        // Optional Phase: spawn the Core Agent (converged tests).
        //
        // Converged tests need both the Core Agent and ADP running side-by-side, sharing a
        // config directory so they can authenticate over IPC. We spawn the Agent first against
        // the per-test state dir, wait until it has written `auth_token` and `ipc_cert.pem`,
        // then spawn ADP with `DD_AUTH_TOKEN_FILE_PATH` pointing at the per-test auth token so
        // ADP's IPC client uses the same per-test credentials (and ADP's own API server uses
        // the matching cert).
        let mut core_agent: Option<NativeProcess> = None;
        if self.test_case.requires_core_agent {
            let agent_spawn_start = Instant::now();
            let agent_binary = match resolve_core_agent_binary_path() {
                Ok(p) => p,
                Err(e) => return make_error_result(test_name, started, "resolve_core_agent", e, phase_timings),
            };
            debug!(test = %test_name, binary = %agent_binary.display(), "Resolved Core Agent binary path.");

            // The Agent and ADP must agree on the auth_token / ipc_cert.pem path. The Agent's
            // authoritative config (sent to ADP via the config stream) overrides ADP's env vars
            // by design, so the Agent must itself be told about the per-test path — otherwise
            // it advertises the platform default (`/opt/datadog-agent/etc/auth_token`), ADP
            // follows that advice for its post-config-stream IPC clients, and TLS fails with
            // UnknownIssuer because the platform default cert does not match what the per-test
            // Agent is actually serving.
            let mut agent_env = self.test_case.container.env.clone();
            agent_env.insert("DD_AUTH_TOKEN_FILE_PATH".to_string(), auth_token_path.clone());

            let agent_config = NativeProcessConfig::new(format!("{}-core-agent", self.test_case.name), agent_binary)
                .with_args(vec![
                    "run".to_string(),
                    "-c".to_string(),
                    state_dir.to_string_lossy().into_owned(),
                ])
                .with_env_map(agent_env);

            let agent = match NativeProcess::spawn(agent_config, log_sink.clone(), CancellationToken::new()).await {
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
            core_agent = Some(agent);
        }

        // Phase: spawn ADP.
        let spawn_start = Instant::now();
        let config_path_str = config_path.to_string_lossy().into_owned();
        let mut adp_env = self.test_case.container.env.clone();
        if self.test_case.requires_core_agent {
            // Point ADP's IPC client at the per-test auth token (and by derivation, the
            // per-test ipc_cert.pem in the same directory).
            adp_env.insert("DD_AUTH_TOKEN_FILE_PATH".to_string(), auth_token_path);
        }
        let process_config = NativeProcessConfig::new(self.test_case.name.clone(), binary_path)
            .with_args(vec!["-c".to_string(), config_path_str, "run".to_string()])
            .with_env_map(adp_env);

        let process = match NativeProcess::spawn(process_config, log_sink, adp_exit_token.clone()).await {
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
    /// to host ports allocated by Docker. On native there is no remapping: a port declared in
    /// `exposed_ports` is reachable on the host at the same number. We populate identity entries
    /// so the existing `port_listening` assertion (which expects every probed port to appear in
    /// the mapping) works unchanged.
    fn build_port_mappings(&self) -> HashMap<String, u16> {
        let mut mappings = HashMap::new();
        for spec in &self.test_case.container.exposed_ports {
            if let Ok((port, protocol)) = parse_port_spec(spec) {
                mappings.insert(format!("{}/{}", port, protocol), port);
            }
        }
        mappings
    }

    async fn run_assertions(
        &self, process_display_name: String, exit_token: CancellationToken,
        exit_code_cell: airlock::native::ExitCodeCell,
    ) -> Vec<AssertionResult> {
        let mut results = Vec::new();
        let cancel_token = self.tctx.test_cancel_token();
        let port_mappings = self.build_port_mappings();

        for step in &self.test_case.assertions {
            match step {
                AssertionStep::Single(cfg) => {
                    let assertion = match create_assertion(cfg) {
                        Ok(a) => a,
                        Err(e) => {
                            results.push(AssertionResult {
                                name: "create_assertion".to_string(),
                                passed: false,
                                message: format!("Failed to create assertion: {}", e),
                                duration: Duration::ZERO,
                            });
                            continue;
                        }
                    };
                    let ctx = AssertionContext {
                        log_buffer: self.log_buffer.clone(),
                        container_exit_token: exit_token.clone(),
                        cancel_token: cancel_token.clone(),
                        container_name: process_display_name.clone(),
                        is_native: true,
                        native_exit_code: Some(exit_code_cell.clone()),
                        port_mappings: port_mappings.clone(),
                    };
                    results.push(assertion.check(&ctx).await);
                }
                AssertionStep::Parallel { parallel } => {
                    let mut futures = Vec::with_capacity(parallel.len());
                    for cfg in parallel {
                        match create_assertion(cfg) {
                            Ok(a) => {
                                let ctx = AssertionContext {
                                    log_buffer: self.log_buffer.clone(),
                                    container_exit_token: exit_token.clone(),
                                    cancel_token: cancel_token.clone(),
                                    container_name: process_display_name.clone(),
                                    is_native: true,
                                    native_exit_code: Some(exit_code_cell.clone()),
                                    port_mappings: port_mappings.clone(),
                                };
                                futures.push(async move { a.check(&ctx).await });
                            }
                            Err(e) => {
                                results.push(AssertionResult {
                                    name: "create_assertion".to_string(),
                                    passed: false,
                                    message: format!("Failed to create parallel assertion: {}", e),
                                    duration: Duration::ZERO,
                                });
                            }
                        }
                    }
                    let parallel_results = futures::future::join_all(futures).await;
                    results.extend(parallel_results);
                }
            }
        }

        results
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

async fn wait_for_agent_ipc_ready(state_dir: &std::path::Path, timeout: Duration) -> Result<(), GenericError> {
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

fn create_test_state_dir() -> Result<PathBuf, GenericError> {
    let suffix = rand::distr::Alphanumeric
        .sample_string(&mut rand::rng(), 8)
        .to_lowercase();
    let dir = std::env::temp_dir().join(format!("panoramic-native-{}", suffix));
    std::fs::create_dir_all(&dir)
        .with_error_context(|| format!("Failed to create state directory '{}'.", dir.display()))?;
    Ok(dir)
}

fn make_error_result(
    name: String, started: Instant, phase: &str, e: GenericError, phase_timings: Vec<PhaseTiming>,
) -> TestResult {
    error!(test = %name, error = %e, phase, "Native integration test setup failed.");
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

/// Bridges [`airlock::native::LogSink`] to the panoramic [`LogBuffer`].
struct NativeLogSink {
    buf: Arc<RwLock<LogBuffer>>,
}

impl LogSink for NativeLogSink {
    fn push_line(&mut self, line: String, is_stderr: bool) {
        // Try a non-blocking write first. If contended, spawn a task to defer the write so we
        // don't stall the log pump (which is itself a tokio task).
        if let Ok(mut buf) = self.buf.try_write() {
            if is_stderr {
                buf.stderr.push(line);
            } else {
                buf.stdout.push(line);
            }
        } else {
            let buf = self.buf.clone();
            tokio::spawn(async move {
                let mut buf = buf.write().await;
                if is_stderr {
                    buf.stderr.push(line);
                } else {
                    buf.stdout.push(line);
                }
            });
        }
    }
}
