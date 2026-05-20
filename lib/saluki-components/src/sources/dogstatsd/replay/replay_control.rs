//! Replay lifecycle and control surface for the DogStatsD source.
//!
//! Mirrors the capture-side split: `TrafficReplay` owns the in-source state, `DogStatsDReplayControl` is the
//! clone-able handle other parts of the process (notably the HTTP API handler) hold to drive that state.
//!
//! The actual replay loop runs as a tokio task spawned from `TrafficReplay::start_replay`. The task opens a
//! `unixgram` client connection to the DSD source's own UDS socket, reads the captured tagger state into the shared
//! store, and then walks the capture file replaying each record at its original cadence with a spoofed
//! `SCM_CREDENTIALS` ancillary block.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use saluki_error::{generic_error, GenericError};
use saluki_io::net::unix::send_replay_packet;
use tokio::net::UnixDatagram;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use super::{CapturedTaggerHandle, CapturedTaggerStore, TimestampResolution, TrafficCaptureReader};
use crate::sources::dogstatsd::REPLAY_CREDENTIALS_GID;

const UNAVAILABLE_REPLAY_CONTROL_ERROR: &str =
    "DogStatsD replay control is unavailable because the source is not running.";

const UDS_NOT_CONFIGURED_ERROR: &str =
    "DogStatsD replay requires UDS; the source is not configured with a dogstatsd_socket path.";

/// Default number of times to replay a capture file when the request omits an explicit count.
pub const DEFAULT_REPLAY_LOOPS: u32 = 1;

/// Options that parameterize a single replay invocation.
#[derive(Debug, Clone)]
pub struct ReplayOptions {
    /// Absolute path to the `.dog` or `.dog.zstd` capture file to replay.
    pub path: PathBuf,

    /// Number of times to replay the file. `0` means replay indefinitely until cancelled.
    pub loops: u32,
}

impl ReplayOptions {
    /// Creates a new `ReplayOptions` with the default loop count.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            loops: DEFAULT_REPLAY_LOOPS,
        }
    }

    /// Sets the loop count for this replay.
    pub fn with_loops(mut self, loops: u32) -> Self {
        self.loops = loops;
        self
    }
}

/// Response payload from `TrafficReplay::start_replay`, echoing what was accepted.
#[derive(Debug, Clone)]
pub struct ReplayHandle {
    /// Absolute path the replay is reading from.
    pub path: PathBuf,
}

/// In-source replay coordinator.
///
/// Lives for the lifetime of the DogStatsD source instance. Owns the "is a replay in flight?" flag, the cancellation
/// token used to stop the in-flight task, the configured UDS socket path the sender connects to, and the captured
/// tagger handle that the sender publishes into and the resolver reads from.
#[derive(Clone)]
pub(crate) struct TrafficReplay {
    inner: Arc<TrafficReplayInner>,
}

struct TrafficReplayInner {
    state: Mutex<ReplayState>,
    socket_path: Option<PathBuf>,
    captured_tagger: CapturedTaggerHandle,
}

#[derive(Default)]
struct ReplayState {
    ongoing: bool,
    cancel: Option<CancellationToken>,
}

impl TrafficReplay {
    /// Creates a new replay coordinator.
    ///
    /// `socket_path` is the UDS path the source listens on (cloned from `DogStatsDConfiguration::socket_path`). When
    /// `None`, `start_replay` returns a 412-class error.
    ///
    /// `captured_tagger` is the shared store handle the replay task writes to and the resolver reads from. Both sides
    /// hold clones of the same atomic slot.
    pub(crate) fn new(socket_path: Option<PathBuf>, captured_tagger: CapturedTaggerHandle) -> Self {
        Self {
            inner: Arc::new(TrafficReplayInner {
                state: Mutex::new(ReplayState::default()),
                socket_path,
                captured_tagger,
            }),
        }
    }

    pub(crate) fn is_ongoing(&self) -> bool {
        let state = self.inner.state.lock().expect("replay mutex poisoned");
        state.ongoing
    }

    /// Starts a new replay session.
    ///
    /// Performs two up-front checks before touching state: UDS configured, and capture file valid. Each rejection
    /// returns a distinct error message so the API caller can surface a meaningful 412/409 response.
    ///
    /// # Errors
    ///
    /// - "replay already in progress" — a replay is currently running (handler maps to 409).
    /// - UDS-not-configured — `dogstatsd_socket` isn't set on the source (handler maps to 412).
    /// - File error — capture file can't be opened or is malformed (handler maps to 412).
    pub(crate) fn start_replay(&self, opts: ReplayOptions) -> Result<ReplayHandle, GenericError> {
        let mut state = self.inner.state.lock().expect("replay mutex poisoned");

        if state.ongoing {
            return Err(generic_error!("replay already in progress"));
        }

        let socket_path = self
            .inner
            .socket_path
            .clone()
            .ok_or_else(|| generic_error!("{}", UDS_NOT_CONFIGURED_ERROR))?;

        // Replay assumes the operator is running ADP with `CAP_SETGID` (typically via `sudo`), matching the Go
        // agent's `dogstatsd-replay` subcommand. We don't proactively probe — if the capability is missing, the first
        // `sendmsg` will fail with `EPERM` and the replay task aborts (see `replay_one_iteration`).
        //
        // Validate the capture file up front so a bad path or corrupted file fails the request rather than getting
        // buried in a background log line.
        let _reader = TrafficCaptureReader::from_path(&opts.path)?;

        let cancel = CancellationToken::new();
        let inner = Arc::clone(&self.inner);
        let task_opts = opts.clone();
        let task_cancel = cancel.clone();
        tokio::spawn(async move { run_replay_task(inner, task_cancel, task_opts, socket_path).await });

        state.ongoing = true;
        state.cancel = Some(cancel);

        Ok(ReplayHandle { path: opts.path })
    }

    /// Stops the currently in-flight replay, if any. Cancellation is best-effort: the background task observes the
    /// cancellation token and exits at the next checkpoint.
    pub(crate) fn stop_replay(&self) {
        let cancel = {
            let mut state = self.inner.state.lock().expect("replay mutex poisoned");
            state.cancel.take()
        };

        if let Some(cancel) = cancel {
            cancel.cancel();
        }
    }
}

async fn run_replay_task(
    inner: Arc<TrafficReplayInner>, cancel: CancellationToken, opts: ReplayOptions, socket_path: PathBuf,
) {
    info!(
        path = %opts.path.display(),
        loops = opts.loops,
        socket = %socket_path.display(),
        "DogStatsD replay started.",
    );

    // Run the replay inside an async block so its `?`-propagated errors land in `result`. The cleanup below this
    // block always runs, regardless of how the replay exits (success, error, cancellation).
    let result: Result<(), GenericError> = async {
        // Publish the captured tagger state once, up front. It's reused across all loop iterations because the
        // captured snapshot doesn't change between iterations.
        let reader = TrafficCaptureReader::from_path(&opts.path)?;
        if let Some(state) = reader.read_state()? {
            inner
                .captured_tagger
                .publish(CapturedTaggerStore::from_tagger_state(state));
        } else {
            debug!("Capture file contains no tagger state trailer; replay packets will not carry captured tags.");
        }
        drop(reader);

        // Open the UDS client. `unbound()` leaves us without a bound source address (datagram replies aren't
        // expected); we connect to the listener's path so subsequent `send` calls don't need to specify the
        // destination.
        let socket =
            UnixDatagram::unbound().map_err(|e| generic_error!("Failed to open UDS client for replay: {}", e))?;
        socket
            .connect(&socket_path)
            .map_err(|e| generic_error!("Failed to connect replay client to '{}': {}", socket_path.display(), e))?;

        let mut iteration: u32 = 0;
        loop {
            // `opts.loops == 0` means forever; otherwise replay exactly `opts.loops` times.
            if opts.loops != 0 && iteration >= opts.loops {
                return Ok(());
            }
            iteration = iteration.saturating_add(1);

            // Race the iteration against cancellation. If cancelled during a sleep, we exit cleanly.
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(()),
                r = replay_one_iteration(&opts, &socket, &cancel) => {
                    r?;
                }
            }
        }
    }
    .await;

    // Always-runs cleanup.
    inner.captured_tagger.clear();
    match result {
        Ok(()) => debug!("DogStatsD replay finished cleanly."),
        Err(err) => error!(error = %err, "DogStatsD replay aborted due to an error."),
    }
    let mut state = inner.state.lock().expect("replay mutex poisoned");
    state.ongoing = false;
    state.cancel = None;
}

async fn replay_one_iteration(
    opts: &ReplayOptions, socket: &UnixDatagram, cancel: &CancellationToken,
) -> Result<(), GenericError> {
    let mut reader = TrafficCaptureReader::from_path(&opts.path)?;
    let resolution = reader.timestamp_resolution();

    let start = Instant::now();
    let mut first_timestamp: Option<i64> = None;
    let mut packets_sent: u64 = 0;

    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }

        let msg = match reader.read_next()? {
            Some(msg) => msg,
            None => {
                debug!(packets_sent, "Replay iteration completed (EOF).");
                return Ok(());
            }
        };

        // Anchor the wall-clock baseline to the first record's timestamp so subsequent sleeps preserve the original
        // inter-packet spacing.
        let first = *first_timestamp.get_or_insert(msg.timestamp);
        let target_offset = compute_target_offset(msg.timestamp, first, resolution);

        let target_deadline = start + target_offset;
        let now = Instant::now();
        if target_deadline > now {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(()),
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(target_deadline)) => {}
            }
        }

        // `msg.pid` is the captured sender's PID. We pack it into the `uid` field on the ancillary block; the
        // receive-side packet handler recovers it from `creds.uid` when it sees `creds.gid == REPLAY_CREDENTIALS_GID`.
        //
        // Any send error here is treated as fatal: kernel-level rejections (e.g. `EPERM` when the process lacks
        // `CAP_SETGID`) will reject every subsequent packet too, so continuing would just spam logs. Tokio's
        // `async_io` parks the task on transient `EAGAIN`, so the errors we actually see are systemic.
        send_replay_packet(socket, &msg.payload, msg.pid, REPLAY_CREDENTIALS_GID)
            .await
            .map_err(|err| generic_error!("Replay packet send failed (captured_pid={}): {}", msg.pid, err))?;
        packets_sent += 1;
    }
}

fn compute_target_offset(timestamp: i64, first_timestamp: i64, resolution: TimestampResolution) -> Duration {
    let delta = timestamp.saturating_sub(first_timestamp).max(0) as u64;
    match resolution {
        TimestampResolution::Seconds => Duration::from_secs(delta),
        TimestampResolution::Nanoseconds => Duration::from_nanos(delta),
    }
}

/// Shared control handle for starting and stopping DogStatsD traffic replay.
///
/// Created before the source is built and bound to the live replay runtime during source construction. The HTTP API
/// handler holds a clone of this handle and calls into the bound runtime synchronously.
#[derive(Clone, Default)]
pub struct DogStatsDReplayControl {
    inner: Arc<Mutex<Option<TrafficReplay>>>,
}

impl DogStatsDReplayControl {
    /// Creates a new, unbound replay control handle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds the control handle to a running replay runtime.
    pub(crate) fn bind(&self, replay: TrafficReplay) {
        let mut state = self.inner.lock().expect("replay control mutex poisoned");
        *state = Some(replay);
    }

    /// Returns whether the bound replay runtime has an active replay session.
    ///
    /// # Errors
    ///
    /// Returns an error if the DogStatsD source hasn't been built and bound to this control handle yet.
    pub fn is_ongoing(&self) -> Result<bool, GenericError> {
        Ok(self.bound_replay()?.is_ongoing())
    }

    /// Starts a new replay session on the bound DogStatsD source.
    ///
    /// # Errors
    ///
    /// Returns an error if the DogStatsD source hasn't been built yet, if a replay is already in progress, or if the
    /// requested capture file can't be opened or parsed.
    pub fn start_replay(&self, opts: ReplayOptions) -> Result<ReplayHandle, GenericError> {
        self.bound_replay()?.start_replay(opts)
    }

    /// Cancels the currently in-flight replay, if any.
    ///
    /// # Errors
    ///
    /// Returns an error if the DogStatsD source hasn't been built yet.
    pub fn stop_replay(&self) -> Result<(), GenericError> {
        self.bound_replay()?.stop_replay();
        Ok(())
    }

    fn bound_replay(&self) -> Result<TrafficReplay, GenericError> {
        let state = self.inner.lock().expect("replay control mutex poisoned");
        state
            .clone()
            .ok_or_else(|| generic_error!("{}", UNAVAILABLE_REPLAY_CONTROL_ERROR))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;

    #[test]
    fn control_requires_bound_runtime() {
        let control = DogStatsDReplayControl::new();
        let err = control
            .start_replay(ReplayOptions::new(PathBuf::from("/does/not/matter.dog")))
            .expect_err("unbound control should fail");
        assert_eq!(err.to_string(), UNAVAILABLE_REPLAY_CONTROL_ERROR);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_when_uds_not_configured() {
        let replay = TrafficReplay::new(None, CapturedTaggerHandle::new());
        let control = DogStatsDReplayControl::new();
        control.bind(replay);

        let err = control
            .start_replay(ReplayOptions::new(PathBuf::from("/tmp/whatever.dog")))
            .expect_err("missing UDS should reject");
        assert!(err.to_string().contains("requires UDS"), "unexpected error: {}", err);
    }

    #[tokio::test(flavor = "current_thread")]
    #[cfg(target_os = "linux")]
    async fn rejects_bad_capture_file() {
        use std::{
            fs,
            time::{SystemTime, UNIX_EPOCH},
        };

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let bad_path = std::env::temp_dir().join(format!("saluki-replay-bad-{}-{}", std::process::id(), timestamp));
        fs::write(&bad_path, b"not a capture").expect("write garbage");

        let socket_path = std::env::temp_dir().join(format!("saluki-replay-sock-{}-{}", std::process::id(), timestamp));
        let replay = TrafficReplay::new(Some(socket_path), CapturedTaggerHandle::new());
        let control = DogStatsDReplayControl::new();
        control.bind(replay);

        let err = control
            .start_replay(ReplayOptions::new(bad_path.clone()))
            .expect_err("bad file should be rejected");
        assert!(
            err.to_string().contains("Datadog capture header"),
            "unexpected error: {}",
            err
        );
        assert!(!control.is_ongoing().expect("control bound"));

        let _ = fs::remove_file(&bad_path);
    }

    #[test]
    fn compute_target_offset_handles_both_resolutions() {
        let seconds = compute_target_offset(105, 100, TimestampResolution::Seconds);
        assert_eq!(seconds, Duration::from_secs(5));

        let nanos = compute_target_offset(1_000_000_000, 0, TimestampResolution::Nanoseconds);
        assert_eq!(nanos, Duration::from_nanos(1_000_000_000));

        // Out-of-order timestamps should clamp to zero rather than panic.
        let clamped = compute_target_offset(50, 100, TimestampResolution::Nanoseconds);
        assert_eq!(clamped, Duration::ZERO);
    }
}
