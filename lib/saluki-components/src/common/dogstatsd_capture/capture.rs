use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use saluki_env::WorkloadProvider;
use saluki_error::GenericError;

use super::writer::{CaptureRecord, CaptureTargetDir, TrafficCaptureWriter};

/// Control handle for DogStatsD traffic capture.
///
/// The caller creates this handle, passes it to the source configuration, and keeps a clone to serve the capture API.
/// All clones share one capture writer.
#[derive(Clone)]
pub struct DogStatsDCaptureControl {
    inner: Arc<TrafficCaptureInner>,
}

struct TrafficCaptureInner {
    writer: TrafficCaptureWriter,
    default_capture_dir: PathBuf,
}

impl DogStatsDCaptureControl {
    /// Creates a capture handle that writes to `default_capture_dir` unless a capture names its own directory.
    ///
    /// `queue_depth` bounds the number of captured packets waiting to be written. Values below `1024` are raised to
    /// `1024`, so that capture persistence never serializes the stream handlers behind it.
    pub fn new(default_capture_dir: PathBuf, queue_depth: usize) -> Self {
        Self::with_workload_provider(default_capture_dir, queue_depth, None)
    }

    /// Creates a capture handle like [`new`](Self::new), which also records the tagger state of the captured
    /// processes from `workload_provider`.
    pub fn with_workload_provider(
        default_capture_dir: PathBuf, queue_depth: usize,
        workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,
    ) -> Self {
        Self {
            inner: Arc::new(TrafficCaptureInner {
                writer: TrafficCaptureWriter::with_workload_provider(queue_depth, workload_provider),
                default_capture_dir,
            }),
        }
    }

    /// Returns whether a capture session is currently active.
    pub fn is_ongoing(&self) -> bool {
        self.inner.writer.is_ongoing()
    }

    /// Starts a new capture session, returning the path of the capture file.
    ///
    /// # Errors
    ///
    /// Returns an error if a capture is already in progress, or if the capture file can't be created.
    pub fn start_capture(
        &self, requested_dir: Option<&Path>, duration: Duration, compressed: bool,
    ) -> Result<PathBuf, GenericError> {
        let target_dir = match requested_dir {
            Some(path) => CaptureTargetDir::Explicit(path.to_path_buf()),
            None => CaptureTargetDir::Implicit(self.inner.default_capture_dir.clone()),
        };

        self.inner.writer.start_capture(target_dir, duration, compressed)
    }

    /// Stops the current capture session, if one is running.
    pub fn stop_capture(&self) {
        self.inner.writer.stop_capture();
    }

    /// Enqueues a captured packet for persistence.
    pub(crate) fn enqueue(&self, record: CaptureRecord) -> bool {
        self.inner.writer.enqueue(record)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::test_support::{temp_dir, wait_until_inactive};
    use super::DogStatsDCaptureControl;

    #[test]
    fn control_starts_one_capture_at_a_time() {
        let target_dir = temp_dir();
        let control = DogStatsDCaptureControl::new(target_dir.path().to_path_buf(), 1);

        let capture_path = control
            .start_capture(None, Duration::from_millis(250), false)
            .expect("capture should start");

        let error = control
            .start_capture(None, Duration::from_millis(250), false)
            .expect_err("second capture should fail");
        assert!(error.to_string().contains("capture already in progress"));

        control.stop_capture();
        control.stop_capture();
        wait_until_inactive(|| control.is_ongoing());

        assert!(capture_path.exists());
    }
}
