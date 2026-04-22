use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use saluki_error::{generic_error, GenericError};

use super::writer::{CaptureRecord, CaptureTargetDir, TrafficCaptureWriter};

const UNAVAILABLE_CAPTURE_CONTROL_ERROR: &str =
    "DogStatsD capture control is unavailable because the source is not running.";

/// Owns capture lifecycle for a running DogStatsD source instance.
#[derive(Clone)]
pub(crate) struct TrafficCapture {
    inner: Arc<TrafficCaptureInner>,
}

struct TrafficCaptureInner {
    writer: TrafficCaptureWriter,
    default_capture_dir: PathBuf,
}

impl TrafficCapture {
    /// Creates a new capture controller with the given default directory and queue depth.
    pub(crate) fn new(default_capture_dir: PathBuf, queue_depth: usize) -> Self {
        Self {
            inner: Arc::new(TrafficCaptureInner {
                writer: TrafficCaptureWriter::new(queue_depth),
                default_capture_dir,
            }),
        }
    }

    /// Returns whether a capture session is currently active.
    pub(crate) fn is_ongoing(&self) -> bool {
        self.inner.writer.is_ongoing()
    }

    /// Starts a new capture session.
    pub(crate) fn start_capture(
        &self, requested_dir: Option<&Path>, duration: Duration, compressed: bool,
    ) -> Result<PathBuf, GenericError> {
        let target_dir = match requested_dir {
            Some(path) => CaptureTargetDir::Explicit(path.to_path_buf()),
            None => CaptureTargetDir::Implicit(self.inner.default_capture_dir.clone()),
        };

        self.inner.writer.start_capture(target_dir, duration, compressed)
    }

    /// Stops the current capture session, if one is running.
    pub(crate) fn stop_capture(&self) {
        self.inner.writer.stop_capture();
    }

    /// Enqueues a captured packet for persistence.
    pub(crate) fn enqueue(&self, record: CaptureRecord) -> bool {
        self.inner.writer.enqueue(record)
    }
}

/// Shared control handle for starting and stopping DogStatsD traffic capture.
///
/// This handle is created before the source is built, then bound to the live [`TrafficCapture`] runtime during source
/// construction. That lets other parts of the process hold a stable handle without reaching into the source internals.
#[derive(Clone, Default)]
pub struct DogStatsDCaptureControl {
    inner: Arc<Mutex<Option<TrafficCapture>>>,
}

impl DogStatsDCaptureControl {
    /// Creates a new, unbound capture control handle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds the control handle to a running capture runtime.
    pub(crate) fn bind(&self, capture: TrafficCapture) {
        let mut state = self.inner.lock().expect("capture control mutex poisoned");
        *state = Some(capture);
    }

    /// Returns whether the bound capture runtime currently has an active capture session.
    ///
    /// # Errors
    ///
    /// Returns an error if the DogStatsD source has not been built and bound to this control handle yet.
    pub fn is_ongoing(&self) -> Result<bool, GenericError> {
        Ok(self.bound_capture()?.is_ongoing())
    }

    /// Starts a new capture session on the bound DogStatsD source.
    ///
    /// # Errors
    ///
    /// Returns an error if the DogStatsD source has not been built yet, or if the underlying capture runtime rejects
    /// the start request.
    pub fn start_capture(
        &self, requested_dir: Option<&Path>, duration: Duration, compressed: bool,
    ) -> Result<PathBuf, GenericError> {
        self.bound_capture()?.start_capture(requested_dir, duration, compressed)
    }

    /// Stops the current capture session on the bound DogStatsD source.
    ///
    /// # Errors
    ///
    /// Returns an error if the DogStatsD source has not been built yet.
    pub fn stop_capture(&self) -> Result<(), GenericError> {
        self.bound_capture()?.stop_capture();
        Ok(())
    }

    fn bound_capture(&self) -> Result<TrafficCapture, GenericError> {
        let state = self.inner.lock().expect("capture control mutex poisoned");
        state
            .clone()
            .ok_or_else(|| generic_error!("{}", UNAVAILABLE_CAPTURE_CONTROL_ERROR))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use super::{DogStatsDCaptureControl, TrafficCapture, UNAVAILABLE_CAPTURE_CONTROL_ERROR};

    #[test]
    fn control_requires_bound_runtime() {
        let control = DogStatsDCaptureControl::new();

        let error = control
            .start_capture(None, Duration::from_millis(25), false)
            .expect_err("unbound control should fail");

        assert_eq!(error.to_string(), UNAVAILABLE_CAPTURE_CONTROL_ERROR);
    }

    #[test]
    fn control_drives_bound_capture_runtime() {
        let control = DogStatsDCaptureControl::new();
        let target_dir = unique_dir("capture-control");
        let capture = TrafficCapture::new(target_dir.clone(), 1);
        control.bind(capture);

        let capture_path = control
            .start_capture(None, Duration::from_millis(250), false)
            .expect("capture should start");

        let error = control
            .start_capture(None, Duration::from_millis(250), false)
            .expect_err("second capture should fail");
        assert!(error.to_string().contains("capture already in progress"));

        control.stop_capture().expect("stop should succeed");
        control.stop_capture().expect("second stop should be safe");
        wait_until_inactive(&control);

        assert!(capture_path.exists());

        let _ = fs::remove_dir_all(target_dir);
    }

    fn wait_until_inactive(control: &DogStatsDCaptureControl) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while control.is_ongoing().expect("control should be bound") && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }

        assert!(
            !control.is_ongoing().expect("control should be bound"),
            "capture control did not stop in time"
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
}
