//! Native process driver for non-containerized integration tests.
//!
//! This module mirrors the relevant surface of the Docker [`Driver`][crate::driver::Driver] but
//! spawns a local binary instead of a container. It exists so that integration tests can run on
//! macOS hosts where ADP is exercised as a real macOS process rather than inside a Linux
//! container.
//!
//! Only the small subset of the Docker driver surface needed by the panoramic native runner is
//! implemented: spawn, log capture, exit watching, and cleanup.

use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, OnceLock},
    time::Duration,
};

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncRead, BufReader},
    process::Command,
    sync::Mutex,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Shared cell that receives the exit code of a spawned [`NativeProcess`].
///
/// The cell is populated by the background exit watcher when the child exits on its own, or by
/// [`NativeProcess::cleanup`] when the test tears down. Consumers (for example, the
/// `process_exits_with` assertion in panoramic) read the cell after the exit token fires.
///
/// The inner `Option<i32>` is `None` if the process was terminated by signal rather than exiting
/// normally with a status code.
pub type ExitCodeCell = Arc<OnceLock<Option<i32>>>;

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
}

impl NativeProcessConfig {
    /// Creates a new configuration with the given display name and binary path.
    pub fn new(name: impl Into<String>, binary_path: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            binary_path: binary_path.into(),
            args: Vec::new(),
            env: HashMap::new(),
        }
    }

    /// Sets the arguments for the process.
    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    /// Sets all environment variables for the process at once.
    pub fn with_env_map(mut self, env: HashMap<String, String>) -> Self {
        self.env = env;
        self
    }
}

/// A trait-object-friendly sink for log lines captured from a native process.
///
/// This is intentionally minimal so consumers can implement it on their own log buffer type
/// without depending on `airlock`.
pub trait LogSink: Send + Sync {
    /// Pushes a captured log line. `is_stderr` is `true` for lines that came from the
    /// process's stderr stream, `false` for stdout.
    fn push_line(&mut self, line: String, is_stderr: bool);
}

/// A spawned native process and its supporting tasks.
///
/// `NativeProcess` owns the child process plus background tasks that pump stdout/stderr lines
/// into a shared sink and observe the child's exit. The provided exit token is cancelled when
/// the child process exits on its own (observed by the background watcher) or when
/// [`cleanup`][Self::cleanup] is called. The exit code is recorded in the shared
/// [`ExitCodeCell`] returned by [`exit_code_cell`][Self::exit_code_cell].
///
/// The spawned process is always made the leader of a new process group, so
/// [`cleanup`][Self::cleanup] can signal the entire group (parent plus any forked helpers).
/// This matters for binaries like the Datadog Core Agent that spawn `trace-agent` /
/// `process-agent` which would otherwise orphan onto launchd when only the parent is killed.
pub struct NativeProcess {
    name: String,
    /// PGID of the spawned process. We made the child the group leader at spawn time, so this
    /// equals the child's PID. `None` only if spawn failed to return a PID (very rare).
    process_group: Option<i32>,
    exit_token: CancellationToken,
    exit_code: ExitCodeCell,
    log_tasks: Vec<JoinHandle<()>>,
    exit_task: Option<JoinHandle<()>>,
}

impl NativeProcess {
    /// Spawns the process described by `config`. The provided `log_sink` receives each line of
    /// captured stdout/stderr; the provided `exit_token` is cancelled when the process exits.
    pub async fn spawn(
        config: NativeProcessConfig, log_sink: Arc<Mutex<dyn LogSink>>, exit_token: CancellationToken,
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
        // Always place the spawned process in a new process group so cleanup can signal the
        // entire group (parent + any forked helpers) without leaking orphans.
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd
            .spawn()
            .with_error_context(|| format!("Failed to spawn '{}'.", config.binary_path.display()))?;

        // PGID == child PID since we made the child the group leader (process_group(0)).
        let process_group = child.id().map(|pid| pid as i32);

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

        // Real exit watcher: moves the child into the task, calls `wait()`, records the exit
        // code, and fires the exit token so blocked assertions (process_stable_for /
        // process_exits_with) unblock immediately rather than waiting for the test's own
        // cleanup phase.
        let exit_code: ExitCodeCell = Arc::new(OnceLock::new());
        let exit_code_for_watcher = exit_code.clone();
        let exit_token_for_watcher = exit_token.clone();
        let name_for_watcher = config.name.clone();
        let exit_task = tokio::spawn(async move {
            match child.wait().await {
                Ok(status) => {
                    let code = status.code();
                    debug!(name = %name_for_watcher, ?code, "Native process exited.");
                    let _ = exit_code_for_watcher.set(code);
                }
                Err(e) => {
                    warn!(name = %name_for_watcher, error = %e, "Failed to wait on native process; treating as exited.");
                    let _ = exit_code_for_watcher.set(None);
                }
            }
            exit_token_for_watcher.cancel();
        });

        Ok(Self {
            name: config.name,
            process_group,
            exit_token,
            exit_code,
            log_tasks: vec![stdout_task, stderr_task],
            exit_task: Some(exit_task),
        })
    }

    /// Returns the display name of the process.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns a clone of the shared exit-code cell. The cell is populated once the process
    /// exits (either on its own or via cleanup). Consumers should wait on [`exit_token`] before
    /// reading.
    pub fn exit_code_cell(&self) -> ExitCodeCell {
        self.exit_code.clone()
    }

    /// Kills the spawned process group, joins background tasks, and cancels the exit token.
    ///
    /// Sends SIGTERM to the whole group, waits a short grace period, then sends SIGKILL to
    /// guarantee nothing is left behind. The grace period gives well-behaved descendants
    /// (for example, the Core Agent's `trace-agent` / `process-agent` helpers) a chance to
    /// shut down cleanly before we hard-kill them.
    pub async fn cleanup(mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.process_group {
            // SAFETY: killpg with a valid pgid is a safe syscall; we ignore the return value.
            unsafe {
                libc::killpg(pgid, libc::SIGTERM);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }

        // The exit watcher will have observed the kill and set the exit code + fired the token.
        // Join it so we don't leak the task.
        if let Some(handle) = self.exit_task.take() {
            let _ = handle.await;
        }
        // Defensive: make sure the token is fired even if the watcher never set it (for example,
        // on a failed wait).
        self.exit_token.cancel();
        for handle in self.log_tasks.drain(..) {
            let _ = handle.await;
        }
    }
}

impl Drop for NativeProcess {
    fn drop(&mut self) {
        if self.exit_task.is_some() {
            warn!(
                name = %self.name,
                "NativeProcess dropped without explicit cleanup; child may have been killed via kill_on_drop."
            );
        }
    }
}

fn spawn_log_pump<R>(reader: R, sink: Arc<Mutex<dyn LogSink>>, is_stderr: bool) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
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
    })
}
