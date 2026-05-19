//! Replay lifecycle and control surface for the DogStatsD source.
//!
//! Mirrors the capture-side split: `TrafficReplay` owns the in-source state, `DogStatsDReplayControl` is the
//! clone-able handle other parts of the process (notably the HTTP API handler) hold to drive that state.
//!
//! Layer B (this PR) wires the state machine and validates the requested capture file. Layer C will replace the
//! placeholder task body with the actual UDS sender loop.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use saluki_error::{generic_error, GenericError};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::TrafficCaptureReader;

const UNAVAILABLE_REPLAY_CONTROL_ERROR: &str =
    "DogStatsD replay control is unavailable because the source is not running.";

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
/// Lives for the lifetime of the DogStatsD source instance. Owns the "is a replay in flight?" flag and the
/// cancellation token used to stop the in-flight task.
#[derive(Clone)]
pub(crate) struct TrafficReplay {
    inner: Arc<TrafficReplayInner>,
}

struct TrafficReplayInner {
    state: Mutex<ReplayState>,
}

#[derive(Default)]
struct ReplayState {
    ongoing: bool,
    cancel: Option<CancellationToken>,
    task: Option<JoinHandle<()>>,
}

impl TrafficReplay {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(TrafficReplayInner {
                state: Mutex::new(ReplayState::default()),
            }),
        }
    }

    pub(crate) fn is_ongoing(&self) -> bool {
        let state = self.inner.state.lock().expect("replay mutex poisoned");
        state.ongoing
    }

    /// Starts a new replay session.
    ///
    /// Validates the requested capture file by opening it with `TrafficCaptureReader` so that bad files are rejected
    /// before the state machine flips to ongoing. Spawns a background task that owns the replay until either it
    /// finishes naturally (Layer C) or `stop_replay` cancels it.
    ///
    /// # Errors
    ///
    /// Returns an error if a replay is already in progress, or if the capture file can't be opened or parsed.
    pub(crate) fn start_replay(&self, opts: ReplayOptions) -> Result<ReplayHandle, GenericError> {
        let mut state = self.inner.state.lock().expect("replay mutex poisoned");

        if state.ongoing {
            return Err(generic_error!("replay already in progress"));
        }

        // Validate up-front so a bad path or corrupted file fails the request rather than getting buried in a
        // background log line.
        let _reader = TrafficCaptureReader::from_path(&opts.path)?;

        let cancel = CancellationToken::new();
        let inner = Arc::clone(&self.inner);
        let task_opts = opts.clone();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move { run_replay_task(inner, task_cancel, task_opts).await });

        state.ongoing = true;
        state.cancel = Some(cancel);
        state.task = Some(task);

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

async fn run_replay_task(inner: Arc<TrafficReplayInner>, cancel: CancellationToken, opts: ReplayOptions) {
    // TODO(Layer C): replace this scaffold with the actual UDS sender loop.
    //
    // The Layer C task will:
    //   1. Re-open the capture file via `TrafficCaptureReader::from_path`.
    //   2. Read the tagger state trailer and seed the origin-resolution overlay.
    //   3. Open a `unixgram` client connection to the DogStatsD source's UDS socket.
    //   4. For `opts.loops` iterations (forever if 0), drive the read/sleep/send loop, honoring `cancel`.
    //   5. On cancellation or natural exit, fall through to the state-clearing block below.
    info!(
        path = %opts.path.display(),
        loops = opts.loops,
        "DogStatsD replay scaffold started; waiting for cancellation (Layer C will replace this loop)."
    );

    cancel.cancelled().await;
    debug!("DogStatsD replay scaffold observed cancellation; clearing state.");

    let mut state = inner.state.lock().expect("replay mutex poisoned");
    state.ongoing = false;
    state.cancel = None;
    state.task = None;
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
    use std::{
        fs,
        path::PathBuf,
        sync::Arc,
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use saluki_env::WorkloadProvider;

    use super::*;
    use crate::sources::dogstatsd::replay::writer::{CaptureRecord, CaptureTargetDir, TrafficCaptureWriter};

    #[test]
    fn control_requires_bound_runtime() {
        let control = DogStatsDReplayControl::new();
        let err = control
            .start_replay(ReplayOptions::new(PathBuf::from("/does/not/matter.dog")))
            .expect_err("unbound control should fail");
        assert_eq!(err.to_string(), UNAVAILABLE_REPLAY_CONTROL_ERROR);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_concurrent_replays() {
        let (capture_path, _guard) = make_capture();
        let replay = TrafficReplay::new();
        let control = DogStatsDReplayControl::new();
        control.bind(replay);

        control
            .start_replay(ReplayOptions::new(capture_path.clone()))
            .expect("first replay should start");

        let err = control
            .start_replay(ReplayOptions::new(capture_path))
            .expect_err("second replay should be rejected while the first is in flight");
        assert!(
            err.to_string().contains("replay already in progress"),
            "unexpected error: {}",
            err
        );

        control.stop_replay().expect("stop should succeed");
        wait_until_inactive(&control).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_bad_capture_file() {
        let bad_path = unique_path("replay-bad-file");
        fs::write(&bad_path, b"not a capture").expect("write garbage");

        let replay = TrafficReplay::new();
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

    #[tokio::test(flavor = "current_thread")]
    async fn stop_replay_clears_ongoing_flag() {
        let (capture_path, _guard) = make_capture();
        let replay = TrafficReplay::new();
        let control = DogStatsDReplayControl::new();
        control.bind(replay);

        control
            .start_replay(ReplayOptions::new(capture_path))
            .expect("replay should start");
        assert!(control.is_ongoing().expect("control bound"));

        control.stop_replay().expect("stop should succeed");
        wait_until_inactive(&control).await;
    }

    fn make_capture() -> (PathBuf, DirGuard) {
        let target_dir = unique_dir("replay-capture");
        let writer = TrafficCaptureWriter::with_workload_provider(1, None::<Arc<dyn WorkloadProvider + Send + Sync>>);
        let path = writer
            .start_capture(
                CaptureTargetDir::Explicit(target_dir.clone()),
                Duration::from_millis(250),
                false,
            )
            .expect("capture should start");
        assert!(writer.enqueue(CaptureRecord {
            timestamp_ns: 1,
            payload: b"x:1|c".to_vec(),
            pid: Some(1),
            ancillary: Vec::new(),
            container_id: None,
        }));
        writer.stop_capture();
        wait_capture_inactive(&writer);
        (path, DirGuard { path: target_dir })
    }

    fn wait_capture_inactive(writer: &TrafficCaptureWriter) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while writer.is_ongoing() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!writer.is_ongoing(), "capture writer did not stop in time");
    }

    async fn wait_until_inactive(control: &DogStatsDReplayControl) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while control.is_ongoing().expect("control bound") && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !control.is_ongoing().expect("control bound"),
            "replay control did not stop in time"
        );
    }

    fn unique_dir(label: &str) -> PathBuf {
        let path = unique_path(label);
        fs::create_dir_all(&path).expect("test directory should be created");
        path
    }

    fn unique_path(label: &str) -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("saluki-{}-{}-{}", label, std::process::id(), timestamp))
    }

    struct DirGuard {
        path: PathBuf,
    }

    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
