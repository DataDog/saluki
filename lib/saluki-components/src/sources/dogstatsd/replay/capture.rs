use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use saluki_error::GenericError;

use super::writer::{CaptureRecord, CaptureTargetDir, TrafficCaptureWriter};

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
