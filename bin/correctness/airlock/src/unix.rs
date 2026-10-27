//! Unix process driver for non-containerized integration tests.
//!
//! This module mirrors the relevant surface of the Docker [`Driver`][crate::driver::Driver] but
//! spawns a local binary instead of a container. It exists so that integration tests can run on
//! Unix hosts where ADP is exercised as a real host process rather than inside a container. The
//! code path is portable across POSIX hosts (Linux + macOS); only macOS is exercised today, but
//! the same module is used unchanged when we opt other Unix hosts into the suite.
//!
//! Only the small subset of the Docker driver surface needed by the panoramic Unix runner is
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

/// How long [`UnixProcess::cleanup`] waits after SIGTERM before sending SIGKILL.
#[cfg(unix)]
const SIGTERM_GRACE: Duration = Duration::from_millis(500);

/// How long [`UnixProcess::cleanup`] waits after SIGKILL for the exit watcher and log pumps before aborting them.
const TASK_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Shared cell that receives the exit code of a spawned [`UnixProcess`].
///
/// The exit watcher sets the cell before cancelling the exit token; read it after that token fires.
/// If [`UnixProcess::cleanup`] aborts the watcher first, the cell stays empty and the token stays uncancelled.
///
/// The inner `Option<i32>` is `None` if the process was terminated by signal rather than exiting
/// normally with a status code.
pub type ExitCodeCell = Arc<OnceLock<Option<i32>>>;

/// Configuration for a Unix process to spawn.
#[derive(Clone)]
pub struct UnixProcessConfig {
    /// Display name used for logs and reporting.
    pub name: String,
    /// Absolute path to the binary to execute.
    pub binary_path: PathBuf,
    /// Arguments passed to the binary.
    pub args: Vec<String>,
    /// Environment variables to set for the process.
    pub env: HashMap<String, String>,
}

impl UnixProcessConfig {
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

/// A trait-object-friendly sink for log lines captured from a Unix process.
///
/// This is intentionally minimal so consumers can implement it on their own log buffer type
/// without depending on `airlock`.
pub trait LogSink: Send + Sync {
    /// Pushes a captured log line. `is_stderr` is `true` for lines that came from the
    /// process's stderr stream, `false` for stdout.
    fn push_line(&mut self, line: String, is_stderr: bool);
}

/// A spawned Unix process and its supporting tasks.
///
/// The exit watcher owns the child, records its exit code in [`ExitCodeCell`], and cancels the provided exit token.
/// Background log pumps forward stdout/stderr lines to a shared sink.
///
/// The child leads a new process group so signals reach it and descendants that remain in the group.
/// Call [`cleanup`][Self::cleanup] to give the group time to shut down gracefully before sending SIGKILL.
/// Dropping the value also sends SIGKILL unless `cleanup` has already sent it.
///
/// # Limits
///
/// - Descendants can escape group signals by changing groups (for example with `setsid`). If they hold stdout or
///   stderr open, `cleanup` aborts the log pumps after a timeout.
/// - `Drop` does not run if the owning process is killed by SIGKILL or the OOM killer, or calls `std::process::exit`.
/// - The OS may reuse an empty group's ID before `killpg` runs, risking a signal to an unrelated group.
pub struct UnixProcess {
    name: String,
    /// Process group ID to kill on drop, or `None` after `cleanup` sends SIGKILL or if spawning returned no PID.
    #[cfg(unix)]
    process_group: Option<i32>,
    exit_code: ExitCodeCell,
    /// The exit watcher, then the stdout and stderr log pumps.
    tasks: Vec<JoinHandle<()>>,
}

impl UnixProcess {
    /// Spawns the process described by `config`. The provided `log_sink` receives each line of
    /// captured stdout/stderr; the provided `exit_token` is cancelled when the process exits.
    pub async fn spawn(
        config: UnixProcessConfig, log_sink: Arc<Mutex<dyn LogSink>>, exit_token: CancellationToken,
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
        // Group the child and its descendants so `cleanup` and `Drop` can signal them together.
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd
            .spawn()
            .with_error_context(|| format!("Failed to spawn '{}'.", config.binary_path.display()))?;

        #[cfg(unix)]
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

        // Exit watcher: moves the child into the task, calls `wait()`, records the exit code,
        // and fires the exit token so blocked assertions (process_stable_for / adp_exits_with)
        // unblock immediately rather than waiting for the test's own cleanup phase.
        let exit_code: ExitCodeCell = Arc::new(OnceLock::new());
        let exit_code_for_watcher = exit_code.clone();
        let name_for_watcher = config.name.clone();
        let exit_task = tokio::spawn(async move {
            match child.wait().await {
                Ok(status) => {
                    let code = status.code();
                    debug!(name = %name_for_watcher, ?code, "Unix process exited.");
                    let _ = exit_code_for_watcher.set(code);
                }
                Err(e) => {
                    warn!(name = %name_for_watcher, error = %e, "Failed to wait on Unix process; treating as exited.");
                    let _ = exit_code_for_watcher.set(None);
                }
            }
            exit_token.cancel();
        });

        Ok(Self {
            name: config.name,
            #[cfg(unix)]
            process_group,
            exit_code,
            tasks: vec![exit_task, stdout_task, stderr_task],
        })
    }

    /// Returns the display name of the process.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns a clone of the shared [`ExitCodeCell`].
    ///
    /// Wait for the exit token passed to [`spawn`][Self::spawn] before reading.
    pub fn exit_code_cell(&self) -> ExitCodeCell {
        self.exit_code.clone()
    }

    /// Stops the process group and waits for its background tasks.
    ///
    /// Sends SIGTERM to the group, allows a grace period for shutdown, then sends SIGKILL. Waits for the exit watcher
    /// and log pumps to finish, aborting unfinished tasks after a timeout.
    ///
    /// Dropping this future before SIGKILL drops the [`UnixProcess`], which sends SIGKILL instead.
    pub async fn cleanup(mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.process_group {
            signal_process_group(pgid, libc::SIGTERM);
            tokio::time::sleep(SIGTERM_GRACE).await;
            signal_process_group(pgid, libc::SIGKILL);
            // Retain the group ID through the grace period so cancellation still triggers SIGKILL in `Drop`.
            self.process_group = None;
        }

        let mut tasks = std::mem::take(&mut self.tasks);
        let joined = tokio::time::timeout(TASK_JOIN_TIMEOUT, async {
            for task in &mut tasks {
                let _ = task.await;
            }
        })
        .await;
        if joined.is_err() {
            warn!(
                name = %self.name,
                "Process tasks still running after SIGKILL; aborting them. A descendant outside the process group \
                 may still hold the output pipes."
            );
            for task in &tasks {
                task.abort();
            }
        }
    }
}

impl Drop for UnixProcess {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.process_group.take() {
            // Let the detached tasks reap the child and drain its output.
            signal_process_group(pgid, libc::SIGKILL);
            warn!(name = %self.name, pgid, "UnixProcess dropped before cleanup; killed its process group.");
        }
    }
}

/// Sends `signal` to every process in group `pgid`, ignoring failure (the group may already be empty).
#[cfg(unix)]
fn signal_process_group(pgid: i32, signal: libc::c_int) {
    // SAFETY: `killpg` takes no pointers and touches no memory of this process.
    unsafe {
        libc::killpg(pgid, signal);
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

#[cfg(all(test, unix))]
mod tests {
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

    use super::*;

    /// Timeout for expected test events.
    const WAIT: Duration = Duration::from_secs(5);

    /// Forwards captured lines to a channel.
    ///
    /// Both log pumps own the sender, so the channel closes when they finish or are aborted. The test scripts keep
    /// their output pipes open until exit, making EOF an exit signal even for zombie processes.
    struct ChannelSink(UnboundedSender<String>);

    impl LogSink for ChannelSink {
        fn push_line(&mut self, line: String, _is_stderr: bool) {
            let _ = self.0.send(line);
        }
    }

    /// Kills a process group when the test ends, including on panic.
    struct KillGroupOnDrop(i32);

    impl Drop for KillGroupOnDrop {
        fn drop(&mut self) {
            signal_process_group(self.0, libc::SIGKILL);
        }
    }

    /// Spawns `/bin/sh -c script` and returns its output receiver and exit token.
    async fn spawn_shell(script: &str) -> (UnixProcess, UnboundedReceiver<String>, CancellationToken) {
        let (tx, rx) = unbounded_channel();
        let sink: Arc<Mutex<dyn LogSink>> = Arc::new(Mutex::new(ChannelSink(tx)));
        let exit_token = CancellationToken::new();
        let config = UnixProcessConfig::new("test", "/bin/sh").with_args(vec!["-c".to_string(), script.to_string()]);
        let process = UnixProcess::spawn(config, sink, exit_token.clone())
            .await
            .expect("spawning /bin/sh should succeed");
        (process, rx, exit_token)
    }

    /// Returns the first line that starts with `prefix`.
    async fn wait_for_line(rx: &mut UnboundedReceiver<String>, prefix: &str) -> String {
        tokio::time::timeout(WAIT, async {
            while let Some(line) = rx.recv().await {
                if line.starts_with(prefix) {
                    return line;
                }
            }
            panic!("output closed before a line starting with {prefix:?} (is the script's tool installed?)");
        })
        .await
        .unwrap_or_else(|_| panic!("no line starting with {prefix:?} within {WAIT:?}"))
    }

    /// Collects remaining lines until both log pumps release the sink.
    async fn wait_for_pipes_closed(rx: &mut UnboundedReceiver<String>) -> Vec<String> {
        tokio::time::timeout(WAIT, async {
            let mut lines = Vec::new();
            while let Some(line) = rx.recv().await {
                lines.push(line);
            }
            lines
        })
        .await
        .expect("a process still holds the output pipes")
    }

    // Regression: `Drop` only logged, leaving host processes holding test ports after a macOS test timeout.
    // The detached watcher owns the `Child`, so dropping `UnixProcess` does not trigger `kill_on_drop`.
    #[tokio::test]
    async fn dropping_a_process_kills_its_whole_group() {
        // The backgrounded `sleep` shares the shell's process group and output pipes.
        let (process, mut rx, exit_token) = spawn_shell("sleep 60 & echo ready; wait").await;
        let exit_code = process.exit_code_cell();
        wait_for_line(&mut rx, "ready").await;

        drop(process);

        wait_for_pipes_closed(&mut rx).await;
        tokio::time::timeout(WAIT, exit_token.cancelled())
            .await
            .expect("the exit watcher should fire the exit token after the kill");
        assert_eq!(exit_code.get(), Some(&None), "the shell should die of a signal");
    }

    #[tokio::test]
    async fn dropping_cleanup_during_its_sigterm_grace_still_kills_the_group() {
        // `trap ''` is inherited, so the backgrounded `sleep` ignores SIGTERM too and only SIGKILL can end the group.
        let (process, mut rx, _exit_token) = spawn_shell("trap '' TERM; sleep 60 & echo ready; wait").await;
        wait_for_line(&mut rx, "ready").await;

        let cleanup = tokio::time::timeout(SIGTERM_GRACE / 2, process.cleanup()).await;
        assert!(cleanup.is_err(), "cleanup should still be waiting out its grace");

        wait_for_pipes_closed(&mut rx).await;
    }

    #[tokio::test]
    async fn cleanup_sends_sigterm_before_sigkill() {
        let (process, mut rx, _exit_token) =
            spawn_shell("trap 'echo got-sigterm; exit 0' TERM; sleep 60 & echo ready; wait").await;
        let exit_code = process.exit_code_cell();
        wait_for_line(&mut rx, "ready").await;

        tokio::time::timeout(WAIT, process.cleanup())
            .await
            .expect("cleanup should return");

        let lines = wait_for_pipes_closed(&mut rx).await;
        assert!(lines.iter().any(|line| line == "got-sigterm"), "lines: {lines:?}");
        assert_eq!(exit_code.get(), Some(&Some(0)), "the trap should have exited the shell");
    }

    #[tokio::test]
    async fn cleanup_returns_when_an_escaped_descendant_holds_the_output_pipes() {
        // Perl's `setpgrp` works on macOS and Linux. `$|` flushes the PID before `exec`.
        // The escaped `sleep` holds the shell's output pipes open from a separate process group.
        let (process, mut rx, _exit_token) =
            spawn_shell(r#"perl -e '$| = 1; setpgrp(0, 0); print "escaped $$\n"; exec @ARGV' sleep 60 & wait"#).await;
        let line = wait_for_line(&mut rx, "escaped ").await;
        let escaped_pid: i32 = line["escaped ".len()..].trim().parse().expect("a PID after `escaped `");
        let _guard = KillGroupOnDrop(escaped_pid);
        // SAFETY: `getpgid` takes no pointers and touches no memory of this process.
        assert_eq!(
            unsafe { libc::getpgid(escaped_pid) },
            escaped_pid,
            "the descendant should lead its own group"
        );

        tokio::time::timeout(SIGTERM_GRACE + TASK_JOIN_TIMEOUT + WAIT, process.cleanup())
            .await
            .expect("cleanup should give up on the pumps after its bound");

        // The escaped `sleep` still runs, so only aborting the pumps can drop the sink.
        tokio::time::timeout(Duration::from_secs(1), async { while rx.recv().await.is_some() {} })
            .await
            .expect("cleanup should have aborted the log pumps");
    }
}
