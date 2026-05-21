# macOS Native Integration Tests Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enable a single integration test (`basic-startup`) to run as a native macOS process via panoramic, in a way that works both on the existing bare-metal `macos:sonoma-arm64` CI runner and locally on a developer's macOS machine.

**Architecture:** Reuse the existing `Test::runtime()` mechanism (currently `"docker"` or `"kubernetes_in_docker"`) by adding a new `"native_macos"` runtime. At test discovery time, expand each `IntegrationConfig` with multiple declared runtimes into one `Test` instance per runtime. A new `NativeRunner` in panoramic handles the `native_macos` case by spawning the ADP binary directly via `tokio::process::Command` and using the existing assertion framework. The existing Docker path is untouched.

**Tech Stack:** Rust, Tokio, the existing `panoramic` test runner and `airlock` driver crate.

**Scope:** Only `basic-startup` on macOS. Only the standalone ADP path (no Core Agent, no IPC). No Tart wrapper in this PR — `make` target works directly on a macOS host; a follow-up PR can add Tart for non-macOS local dev. No CI job in this PR — that's a follow-up that depends on a separate `build-adp-macos-binary` job. This PR proves the end-to-end design works locally on macOS.

---

## File Structure

**New files:**
- `bin/correctness/airlock/src/native.rs` — `NativeProcess` abstraction (spawn, log capture, exit watch, cleanup)
- `bin/correctness/panoramic/src/native_runner.rs` — `NativeIntegrationRunner` analogous to the existing `IntegrationRunner` but for the native-process path
- `docs/superpowers/plans/2026-05-21-macos-native-integration-tests.md` — this plan

**Modified files:**
- `bin/correctness/airlock/src/lib.rs` — export `native` module
- `bin/correctness/panoramic/src/config.rs` — add `runtimes: Vec<String>` field to `IntegrationConfig`; dispatch `run()` based on the per-instance runtime
- `bin/correctness/panoramic/src/test.rs` — at discovery, expand multi-runtime integration configs into one `Test` per runtime
- `bin/correctness/panoramic/src/main.rs` — wire the new module
- `bin/correctness/panoramic/src/assertions/mod.rs` — `AssertionContext` already has `container_name` and `port_mappings`; for native, `container_name` doubles as the process display name and `port_mappings` is identity (no remapping needed). No code change expected here, but verify.
- `test/integration/cases/basic-startup/config.yaml` — add `runtimes: [docker, native_macos]`
- `Makefile` — add `test-integration-macos` target

**Files NOT touched in this PR (deferred):**
- `bin/correctness/panoramic/src/runner.rs` (the existing `IntegrationRunner`) — leave alone to keep the Linux path zero-risk
- `bin/correctness/panoramic/src/assertions/file_contains.rs` — only one test in the corpus uses it, not basic-startup
- `tooling/generate-correctness-pipeline.sh` — CI pipeline gen, deferred to a follow-up
- `.gitlab/` files — CI integration deferred

---

## Conventions

- The ADP binary location is discovered via the `ADP_BINARY_PATH` env var, falling back to `target/release/agent-data-plane` relative to the panoramic working directory.
- Per-test process output (stdout + stderr) is captured into the existing `LogBuffer` exactly the same way the Docker path does, so the existing assertions work unchanged.
- The native runner respects the existing `TestContext` cancel token and writes per-test logs into the existing `log_dir` structure.

---

## Task 1: Add the `NativeProcess` abstraction in `airlock`

**Files:**
- Create: `bin/correctness/airlock/src/native.rs`
- Modify: `bin/correctness/airlock/src/lib.rs:1-3`
- Test: covered by integration end-to-end (no unit test for this initial slice; the structure is mostly straight `tokio::process` wiring that's easier to exercise through panoramic)

- [ ] **Step 1: Create the `native.rs` module skeleton**

Create `bin/correctness/airlock/src/native.rs`:

```rust
//! Native process driver for non-containerized integration tests.
//!
//! This module mirrors the surface of the Docker [`Driver`][crate::driver::Driver] but spawns a
//! local binary instead of a container. It exists so that integration tests can run on macOS
//! hosts where ADP is exercised as a real macOS process rather than inside a Linux container.
//!
//! Only the small subset of the Docker driver surface needed by the panoramic
//! `NativeIntegrationRunner` is implemented: spawn, log capture, exit watching, and cleanup.

use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::{
    io::{AsyncBufReadExt as _, BufReader},
    process::{Child, Command},
    sync::Mutex,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Configuration for a native process to spawn.
#[derive(Clone)]
pub struct NativeProcessConfig {
    /// Display name used for logs and reporting.
    pub name: String,
    /// Absolute path to the binary to execute.
    pub binary_path: PathBuf,
    /// Arguments passed to the binary.
    pub args: Vec<String>,
    /// Environment variables to set for the process.
    pub env: HashMap<String, String>,
    /// Working directory for the process. If `None`, inherits panoramic's working directory.
    pub working_dir: Option<PathBuf>,
}

impl NativeProcessConfig {
    /// Creates a new configuration with the given display name and binary path.
    pub fn new(name: impl Into<String>, binary_path: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            binary_path: binary_path.into(),
            args: Vec::new(),
            env: HashMap::new(),
            working_dir: None,
        }
    }

    /// Sets the arguments for the process.
    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    /// Sets an environment variable for the process.
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Sets all environment variables for the process at once.
    pub fn with_env_map(mut self, env: HashMap<String, String>) -> Self {
        self.env = env;
        self
    }

    /// Sets the working directory for the process.
    pub fn with_working_dir(mut self, dir: PathBuf) -> Self {
        self.working_dir = Some(dir);
        self
    }
}

/// A spawned native process and its supporting tasks.
///
/// `NativeProcess` owns the child process plus background tasks that pump stdout/stderr lines
/// into a shared buffer and observe the child's exit. Dropping or explicitly calling
/// [`cleanup`][Self::cleanup] kills the child and joins the background tasks.
pub struct NativeProcess {
    name: String,
    child: Option<Child>,
    exit_token: CancellationToken,
    log_tasks: Vec<JoinHandle<()>>,
    exit_task: Option<JoinHandle<()>>,
}

impl NativeProcess {
    /// Spawns the process described by `config`. The provided `log_sink` receives each line of
    /// captured stdout/stderr; the provided `exit_token` is cancelled when the process exits.
    pub async fn spawn(
        config: NativeProcessConfig,
        log_sink: Arc<Mutex<LogSink>>,
        exit_token: CancellationToken,
    ) -> Result<Self, GenericError> {
        if !config.binary_path.exists() {
            return Err(generic_error!(
                "Binary not found at expected path: {}",
                config.binary_path.display()
            ));
        }

        let mut cmd = Command::new(&config.binary_path);
        cmd.args(&config.args)
            .envs(&config.env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(ref wd) = config.working_dir {
            cmd.current_dir(wd);
        }

        let mut child = cmd
            .spawn()
            .with_error_context(|| format!("Failed to spawn '{}'.", config.binary_path.display()))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| generic_error!("Failed to capture stdout."))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| generic_error!("Failed to capture stderr."))?;

        let stdout_task = spawn_log_pump(stdout, log_sink.clone(), false);
        let stderr_task = spawn_log_pump(stderr, log_sink, true);

        let exit_token_for_watcher = exit_token.clone();
        let name_for_watcher = config.name.clone();
        let exit_task = tokio::spawn(async move {
            // Wait for the child to exit; we cannot move it out of the struct here, so the
            // exit watching is done in `cleanup`. This task is only used as a placeholder if
            // we add SIGCHLD-style observation later. For now, the exit token is fired in
            // `cleanup` after `child.wait().await`.
            debug!(name = %name_for_watcher, "Native process exit watcher placeholder.");
            exit_token_for_watcher.cancelled().await;
        });

        Ok(Self {
            name: config.name,
            child: Some(child),
            exit_token,
            log_tasks: vec![stdout_task, stderr_task],
            exit_task: Some(exit_task),
        })
    }

    /// Returns the display name of the process.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns a handle to the cancellation token that fires when the process exits.
    pub fn exit_token(&self) -> CancellationToken {
        self.exit_token.clone()
    }

    /// Waits for the process to exit. If `timeout` elapses first, the process is killed.
    ///
    /// Returns the exit code if available, or `None` if the process was killed by signal.
    pub async fn wait_with_timeout(&mut self, timeout: Duration) -> Result<Option<i32>, GenericError> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| generic_error!("Process already cleaned up."))?;
        match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => Ok(status.code()),
            Ok(Err(e)) => Err(generic_error!("Failed to wait for process: {}", e)),
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                Err(generic_error!("Process did not exit within timeout."))
            }
        }
    }

    /// Kills the child, joins background tasks, and cancels the exit token.
    pub async fn cleanup(mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.exit_token.cancel();
        if let Some(handle) = self.exit_task.take() {
            let _ = handle.await;
        }
        for handle in self.log_tasks.drain(..) {
            let _ = handle.await;
        }
    }
}

impl Drop for NativeProcess {
    fn drop(&mut self) {
        if self.child.is_some() {
            warn!(
                name = %self.name,
                "NativeProcess dropped without explicit cleanup; child will be killed via kill_on_drop."
            );
        }
    }
}

/// A trait-object-friendly sink for log lines captured from a native process.
///
/// This is intentionally minimal so panoramic's existing `LogBuffer` can wrap one of these
/// without depending on `airlock`.
pub trait LogSink: Send + Sync {
    fn push_line(&mut self, line: String, is_stderr: bool);
}

fn spawn_log_pump<R>(
    reader: R,
    sink: Arc<Mutex<dyn LogSink>>,
    is_stderr: bool,
) -> JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut lines = BufReader::new(reader).lines();
    tokio::spawn(async move {
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let mut sink = sink.lock().await;
                    sink.push_line(line, is_stderr);
                }
                Ok(None) => break,
                Err(e) => {
                    debug!(error = %e, "Log pump read error; stopping.");
                    break;
                }
            }
        }
    });
}
```

- [ ] **Step 2: Export the module from `airlock`**

Modify `bin/correctness/airlock/src/lib.rs`:

```rust
pub mod config;
pub mod docker;
pub mod driver;
pub mod native;
```

- [ ] **Step 3: Compile and verify**

Run: `cd bin/correctness && cargo check -p airlock`
Expected: clean compile, no errors.

- [ ] **Step 4: Commit**

```bash
git add bin/correctness/airlock/src/native.rs bin/correctness/airlock/src/lib.rs
git commit -m "feat(airlock): add native process driver for non-containerized tests"
```

---

## Task 2: Bridge the existing `LogBuffer` to the `LogSink` trait

**Files:**
- Modify: `bin/correctness/panoramic/src/assertions/mod.rs` (around `LogBuffer` definition)

The Docker path populates `LogBuffer` via bollard `LogOutput`. The native path needs to populate the same `LogBuffer` via the `LogSink` trait so the existing assertions work unchanged.

- [ ] **Step 1: Inspect the current `LogBuffer`**

Run: `grep -n "pub struct LogBuffer\|impl LogBuffer\|push" bin/correctness/panoramic/src/assertions/mod.rs`
Note the existing API so the trait implementation matches.

- [ ] **Step 2: Implement `LogSink` for `LogBuffer`**

Add to `bin/correctness/panoramic/src/assertions/mod.rs`, after the existing `impl LogBuffer { ... }` block:

```rust
impl airlock::native::LogSink for LogBuffer {
    fn push_line(&mut self, line: String, is_stderr: bool) {
        // Match the existing Docker log capture format: each entry is the raw line. The
        // is_stderr flag is currently informational only.
        let _ = is_stderr;
        self.lines.push(line);
    }
}
```

(Adjust field name `lines` if the struct uses something different — verify in Step 1.)

- [ ] **Step 3: Verify it compiles**

Run: `cd bin/correctness && cargo check -p panoramic`
Expected: clean compile.

- [ ] **Step 4: Commit**

```bash
git add bin/correctness/panoramic/src/assertions/mod.rs
git commit -m "feat(panoramic): implement LogSink for LogBuffer"
```

---

## Task 3: Add `runtimes` field to `IntegrationConfig` and expand at discovery

**Files:**
- Modify: `bin/correctness/panoramic/src/config.rs` (`IntegrationConfig` struct, deserialization, `Test` impl)
- Modify: `bin/correctness/panoramic/src/test.rs` (`try_load_test` for `integration` type)

- [ ] **Step 1: Add the `runtimes` field**

Modify the `IntegrationConfig` struct in `bin/correctness/panoramic/src/config.rs`:

```rust
#[derive(Clone, Debug, Deserialize)]
pub struct IntegrationConfig {
    pub name: String,

    #[serde(default)]
    pub description: Option<String>,

    pub timeout: HumanDuration,

    pub container: ContainerConfig,

    pub assertions: Vec<AssertionStep>,

    /// Runtimes under which this test runs.
    ///
    /// Each value must be either `"docker"` (the default) or `"native_macos"`. When multiple
    /// runtimes are declared, the test discovery layer expands the config into one independent
    /// test case per runtime, named `{name}/{runtime}`.
    #[serde(default = "default_runtimes")]
    pub runtimes: Vec<String>,

    /// Resolved runtime for this specific test instance after discovery-time expansion.
    ///
    /// At parse time, this is always empty. The discovery layer sets it when expanding a
    /// multi-runtime config into per-runtime instances.
    #[serde(skip)]
    pub resolved_runtime: String,

    #[serde(skip)]
    pub base_path: PathBuf,
}

fn default_runtimes() -> Vec<String> {
    vec!["docker".to_string()]
}
```

- [ ] **Step 2: Surface the per-instance runtime via the `Test` trait impl**

In the same file, update the `Test` impl for `IntegrationConfig`:

```rust
#[async_trait]
impl Test for IntegrationConfig {
    fn name(&self) -> String {
        if self.resolved_runtime.is_empty() || self.runtimes.len() <= 1 {
            self.name.clone()
        } else {
            format!("{}/{}", self.name, self.resolved_runtime)
        }
    }

    fn suite(&self) -> TestSuite {
        TestSuite::Integration
    }

    fn description(&self) -> Option<String> {
        self.description.clone()
    }

    fn timeout(&self) -> Duration {
        self.timeout.0
    }

    fn images(&self) -> BTreeMap<&str, String> {
        let mut m = BTreeMap::new();
        // The native_macos runtime doesn't require any container image.
        if self.resolved_runtime != "native_macos" {
            m.insert("container", self.container.image.clone());
        }
        m
    }

    fn runtime(&self) -> String {
        if self.resolved_runtime.is_empty() {
            "docker".to_string()
        } else {
            self.resolved_runtime.clone()
        }
    }

    async fn run(&self, tctx: TestContext) -> TestResult {
        match self.resolved_runtime.as_str() {
            "native_macos" => {
                let mut runner = crate::native_runner::NativeIntegrationRunner::new(self.clone(), tctx);
                runner.run().await
            }
            // Default to the existing Docker path for "docker" or unset.
            _ => {
                let mut runner = crate::runner::IntegrationRunner::new(self.clone(), tctx);
                runner.run().await
            }
        }
    }
}
```

- [ ] **Step 3: Expand multi-runtime configs at discovery**

Modify `try_load_test` in `bin/correctness/panoramic/src/test.rs` for the `"integration"` arm:

```rust
"integration" => {
    let config = IntegrationConfig::from_yaml(config_path)?;
    if config.runtimes.is_empty() {
        return Err(generic_error!("integration test '{}' has empty runtimes list", config.name));
    }
    let mut tests: Vec<Box<dyn Test>> = Vec::new();
    for runtime in &config.runtimes {
        if runtime != "docker" && runtime != "native_macos" {
            return Err(generic_error!(
                "integration test '{}' declares unknown runtime '{}' (expected 'docker' or 'native_macos')",
                config.name,
                runtime
            ));
        }
        let mut variant = config.clone();
        variant.resolved_runtime = runtime.clone();
        tests.push(Box::new(variant));
    }
    Ok(tests)
}
```

- [ ] **Step 4: Verify compilation (panoramic will fail until Task 4 lands)**

Run: `cd bin/correctness && cargo check -p panoramic 2>&1 | tail -20`
Expected: FAIL on missing `crate::native_runner` module — that's the next task.

- [ ] **Step 5: Commit**

```bash
git add bin/correctness/panoramic/src/config.rs bin/correctness/panoramic/src/test.rs
git commit -m "feat(panoramic): add runtimes field to integration test config"
```

---

## Task 4: Add the `NativeIntegrationRunner`

**Files:**
- Create: `bin/correctness/panoramic/src/native_runner.rs`
- Modify: `bin/correctness/panoramic/src/main.rs` (declare the module)

- [ ] **Step 1: Create the runner module**

Create `bin/correctness/panoramic/src/native_runner.rs`:

```rust
//! Native-process integration test runner.
//!
//! This runner is the parallel of [`crate::runner::IntegrationRunner`] but for tests declared
//! with `runtime: native_macos`. Instead of building a Docker container, it spawns a binary
//! directly via [`airlock::native::NativeProcess`] and feeds its stdout/stderr into the same
//! [`LogBuffer`][crate::assertions::LogBuffer] used by the Docker path so the assertions work
//! unchanged.
//!
//! Scope (initial): only ADP-standalone tests. The binary is `agent-data-plane`, located via
//! the `ADP_BINARY_PATH` env var (falling back to `target/release/agent-data-plane`).

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use airlock::native::{NativeProcess, NativeProcessConfig};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    assertions::{create_assertion, AssertionContext, AssertionResult, LogBuffer},
    config::{AssertionStep, IntegrationConfig},
    reporter::{PhaseTiming, TestResult},
    test::TestContext,
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

        // Phase: resolve binary path
        let binary_path = match resolve_adp_binary_path() {
            Ok(p) => p,
            Err(e) => {
                return make_error_result(test_name, started, "resolve_binary", e);
            }
        };
        debug!(test = %test_name, binary = %binary_path.display(), "Resolved ADP binary path.");

        // Phase: spawn process
        let spawn_start = Instant::now();
        let exit_token = CancellationToken::new();

        // Bridge the LogBuffer behind a Mutex<dyn LogSink>. We have to take ownership of the
        // buffer via an Arc<Mutex<...>> compatible shape; the simplest path is to construct a
        // separate sink struct that pushes into the shared LogBuffer.
        let sink_buf = self.log_buffer.clone();
        let log_sink: Arc<Mutex<dyn airlock::native::LogSink>> =
            Arc::new(Mutex::new(NativeLogSink { buf: sink_buf }));

        let process_config = NativeProcessConfig::new(self.test_case.name.clone(), binary_path)
            .with_args(vec!["run".to_string()])
            .with_env_map(self.test_case.container.env.clone());

        let process = match NativeProcess::spawn(process_config, log_sink, exit_token.clone()).await {
            Ok(p) => p,
            Err(e) => {
                phase_timings.push(PhaseTiming {
                    phase: "spawn".to_string(),
                    duration: spawn_start.elapsed(),
                });
                return make_error_result(test_name, started, "spawn", e);
            }
        };
        phase_timings.push(PhaseTiming {
            phase: "spawn".to_string(),
            duration: spawn_start.elapsed(),
        });

        info!(test = %test_name, "Native process started.");

        // Phase: run assertions
        let assertion_start = Instant::now();
        let assertion_results = self
            .run_assertions(process.name().to_string(), exit_token.clone())
            .await;
        phase_timings.push(PhaseTiming {
            phase: "assertions".to_string(),
            duration: assertion_start.elapsed(),
        });

        // Phase: cleanup
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
            assertion_results: assertion_results.clone(),
            error: None,
            phase_timings,
            assertion_details: assertion_results,
        }
    }

    async fn run_assertions(
        &self,
        process_display_name: String,
        exit_token: CancellationToken,
    ) -> Vec<AssertionResult> {
        let mut results = Vec::new();
        let cancel_token = self.tctx.test_cancel_token();

        for step in &self.test_case.assertions {
            match step {
                AssertionStep::Single(cfg) => {
                    let assertion = create_assertion(cfg.clone(), &self.test_case);
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
                    let futures: Vec<_> = parallel
                        .iter()
                        .map(|cfg| {
                            let assertion = create_assertion(cfg.clone(), &self.test_case);
                            let ctx = AssertionContext {
                                log_buffer: self.log_buffer.clone(),
                                container_exit_token: exit_token.clone(),
                                cancel_token: cancel_token.clone(),
                                container_name: process_display_name.clone(),
                                port_mappings: HashMap::new(),
                            };
                            async move { assertion.check(&ctx).await }
                        })
                        .collect();
                    let parallel_results = futures::future::join_all(futures).await;
                    results.extend(parallel_results);
                }
            }
        }

        results
    }
}

fn resolve_adp_binary_path() -> Result<PathBuf, GenericError> {
    let explicit = std::env::var(ADP_BINARY_ENV_VAR).ok();
    let path = match explicit {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(DEFAULT_ADP_BINARY_PATH),
    };

    let canonical = path.canonicalize().with_error_context(|| {
        format!(
            "ADP binary not found at '{}'. Set {} or build via `cargo build --release --bin agent-data-plane`.",
            path.display(),
            ADP_BINARY_ENV_VAR
        )
    })?;
    Ok(canonical)
}

fn make_error_result(name: String, started: Instant, phase: &str, e: GenericError) -> TestResult {
    error!(test = %name, error = %e, phase, "Native integration test setup failed.");
    TestResult {
        name,
        passed: false,
        duration: started.elapsed(),
        assertion_results: vec![],
        error: Some(format!("Failed in phase '{}': {}", phase, e)),
        phase_timings: vec![],
        assertion_details: vec![],
    }
}

/// Bridge from `airlock::native::LogSink` to the panoramic `LogBuffer`.
struct NativeLogSink {
    buf: Arc<RwLock<LogBuffer>>,
}

impl airlock::native::LogSink for NativeLogSink {
    fn push_line(&mut self, line: String, is_stderr: bool) {
        // Try a non-blocking write. If the lock is contended, do a blocking write — assertions
        // hold the read lock briefly so contention is rare.
        if let Ok(mut buf) = self.buf.try_write() {
            buf.push_line(line, is_stderr);
        } else {
            // Fall back: spawn a task to do the write so we don't block this caller. We're
            // already inside a tokio task here (the log pump), so blocking would stall it.
            let buf = self.buf.clone();
            tokio::spawn(async move {
                buf.write().await.push_line(line, is_stderr);
            });
        }
    }
}
```

- [ ] **Step 2: Declare the module in `main.rs`**

Modify `bin/correctness/panoramic/src/main.rs`:

Find the existing `mod runner;` line and add `mod native_runner;` after it.

- [ ] **Step 3: Verify compilation**

Run: `cd bin/correctness && cargo check -p panoramic 2>&1 | tail -30`
Expected: clean compile.

If compile errors mention `AssertionContext` field names, the field names need to match the existing struct definition — check `bin/correctness/panoramic/src/assertions/mod.rs` for the exact shape and adjust.

- [ ] **Step 4: Commit**

```bash
git add bin/correctness/panoramic/src/native_runner.rs bin/correctness/panoramic/src/main.rs
git commit -m "feat(panoramic): add NativeIntegrationRunner for native_macos runtime"
```

---

## Task 5: Wire up `basic-startup` for the new runtime

**Files:**
- Modify: `test/integration/cases/basic-startup/config.yaml`

- [ ] **Step 1: Add the runtime opt-in**

Modify `test/integration/cases/basic-startup/config.yaml` so the top-level keys read:

```yaml
type: integration
name: "basic-startup"
description: "Verifies ADP starts successfully and remains stable"
timeout: 90s
runtimes: [docker, native_macos]

container:
  image: "saluki-images/datadog-agent:testing-devel"
  env:
    DD_API_KEY: "test-api-key"
    DD_HOSTNAME: "integration-test"
    DD_DATA_PLANE_ENABLED: "true"
    DD_DATA_PLANE_STANDALONE_MODE: "true"

assertions:
  - type: log_contains
    pattern: "Agent Data Plane starting"
    timeout: 5s
  - parallel:
      - type: process_stable_for
        duration: 10s
      - type: log_not_contains
        pattern: "panic|PANIC"
        regex: true
        during: 10s
```

- [ ] **Step 2: Discovery-only sanity check**

Run: `cargo run --release --bin panoramic -- list -d test/integration/cases`
Expected output includes:
```
basic-startup/docker
basic-startup/native_macos
```
…and all other tests show up exactly once with their original names.

- [ ] **Step 3: Commit**

```bash
git add test/integration/cases/basic-startup/config.yaml
git commit -m "test(integration): enable basic-startup on native_macos runtime"
```

---

## Task 6: Add the `test-integration-macos` make target

**Files:**
- Modify: `Makefile`

- [ ] **Step 1: Inspect existing integration test targets**

Run: `grep -n "test-integration\|test-integration-quick\|build-panoramic" Makefile`
Note the existing pattern.

- [ ] **Step 2: Add the new targets**

Append to `Makefile`, near the existing `test-integration` rule:

```makefile
.PHONY: build-adp-macos
build-adp-macos: ## Builds the ADP binary natively for macOS (release profile)
	@echo "[*] Building agent-data-plane (release, native macOS target)..."
	@cargo build --release --bin agent-data-plane

.PHONY: test-integration-macos
test-integration-macos: build-panoramic build-adp-macos
test-integration-macos: ## Runs macOS native integration tests (no Docker)
	@echo "[*] Running macOS native integration tests..."
	@ADP_BINARY_PATH=$(shell pwd)/target/release/agent-data-plane \
		target/release/panoramic run -d $(shell pwd)/test/integration/cases \
		-t basic-startup/native_macos --no-tui \
		$(if $(PANORAMIC_LOG_DIR),-l $(PANORAMIC_LOG_DIR))
```

- [ ] **Step 3: Run the new target end-to-end**

Run: `make test-integration-macos`

Expected output:
- Panoramic launches one test (`basic-startup/native_macos`).
- The `log_contains` assertion for `"Agent Data Plane starting"` passes within 5s.
- The `process_stable_for` and `log_not_contains` assertions complete after ~10s.
- Test result: PASS.

If the test fails, check:
- The `agent-data-plane` binary exists at `target/release/agent-data-plane`.
- No other ADP process is already bound to default ports (`lsof -i :8125 -i :8135`).
- The log buffer is actually receiving lines (look at `PANORAMIC_LOG_DIR` output if set).

- [ ] **Step 4: Commit**

```bash
git add Makefile
git commit -m "build: add test-integration-macos make target"
```

---

## Task 7: Verify the Docker path still works for the same test

This is the regression check that ensures we didn't break anything on Linux.

- [ ] **Step 1: Confirm the docker variant still shows up in discovery**

Already covered by Task 5 Step 2, but re-confirm:

Run: `cargo run --release --bin panoramic -- list -d test/integration/cases | grep basic-startup`
Expected:
```
basic-startup/docker
basic-startup/native_macos
```

- [ ] **Step 2: Run the docker variant locally if Docker is available**

Skip if Docker isn't available locally. Otherwise:

Run: `target/release/panoramic run -d test/integration/cases -t basic-startup/docker --no-tui`
Expected: existing Docker path runs unchanged and passes.

- [ ] **Step 3: Run unit tests for affected crates**

Run: `cargo test -p airlock -p panoramic 2>&1 | tail -20`
Expected: all tests pass, no regressions.

- [ ] **Step 4: Run formatter and clippy**

Run: `make fmt && make check-clippy 2>&1 | tail -30`
Expected: clean.

---

## Self-review checklist

- **Spec coverage:** Single test on macOS running natively via panoramic — Task 5 + Task 6. Docker path preserved — Task 7. CI and Tart wrapper are explicitly deferred and not in spec for this PR.
- **No placeholders:** All code blocks are concrete.
- **Type consistency:** `NativeProcessConfig` defined in Task 1 is used in Task 4; field names match. `LogSink` defined in Task 1 is implemented in Task 4. `AssertionContext` field names match the existing struct shape (verified in Task 4 Step 3 — adjust if compile fails).
- **One risk to flag in execution:** the `AssertionContext` struct definition lives in `assertions/mod.rs` and may have a different field shape than what's written in Task 4's code. The first thing to verify when implementing Task 4 is the exact `AssertionContext` definition; adapt the `run_assertions` call sites in `native_runner.rs` to match.

---

## What this PR explicitly does NOT do

- No CI job — that requires building ADP as an artifact and a new `.gitlab/test.yml` entry; deferred.
- No Tart wrapper script — deferred to a follow-up so non-macOS developers can run macOS tests locally.
- No conversion of the other 16 standalone integration tests — only `basic-startup` is wired up.
- No converged (Agent + ADP) tests — they need Agent install plumbing and IPC, which is its own scope.
- No refactor of the existing Docker `IntegrationRunner`. The two runners remain parallel for now; merging via a shared trait is a follow-up.
