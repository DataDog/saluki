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
    config::{AssertionStep, IntegrationConfig},
    reporter::{PhaseTiming, TestResult},
    test::{Test, TestContext},
};

const ADP_BINARY_ENV_VAR: &str = "ADP_BINARY_PATH";
const DEFAULT_ADP_BINARY_PATH: &str = "target/release/agent-data-plane";

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

        // Phase: spawn the process.
        let spawn_start = Instant::now();
        let exit_token = CancellationToken::new();
        let log_sink: Arc<Mutex<dyn LogSink>> = Arc::new(Mutex::new(NativeLogSink {
            buf: self.log_buffer.clone(),
        }));

        let config_path_str = config_path.to_string_lossy().into_owned();
        let process_config = NativeProcessConfig::new(self.test_case.name.clone(), binary_path)
            .with_args(vec!["-c".to_string(), config_path_str, "run".to_string()])
            .with_env_map(self.test_case.container.env.clone());

        let process = match NativeProcess::spawn(process_config, log_sink, exit_token.clone()).await {
            Ok(p) => p,
            Err(e) => {
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

        info!(test = %test_name, "Native process started.");

        // Phase: run assertions.
        let assertion_start = Instant::now();
        let assertion_results = self
            .run_assertions(process.name().to_string(), exit_token.clone())
            .await;
        phase_timings.push(PhaseTiming {
            phase: "assertions".to_string(),
            duration: assertion_start.elapsed(),
        });

        // Phase: cleanup.
        let cleanup_start = Instant::now();
        process.cleanup().await;
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

    async fn run_assertions(
        &self, process_display_name: String, exit_token: CancellationToken,
    ) -> Vec<AssertionResult> {
        let mut results = Vec::new();
        let cancel_token = self.tctx.test_cancel_token();

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
                        port_mappings: HashMap::new(),
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
                                    port_mappings: HashMap::new(),
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
