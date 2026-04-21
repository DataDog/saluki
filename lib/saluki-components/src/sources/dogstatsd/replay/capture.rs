use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use saluki_error::GenericError;

use super::writer::{CaptureRecord, TrafficCaptureWriter};

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
        let target_dir = requested_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| self.inner.default_capture_dir.clone());

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

#[cfg(test)]
mod tests {
    use std::{env::temp_dir, path::PathBuf, time::Duration};

    use super::super::writer::CaptureRecord;
    use super::TrafficCapture;

    #[test]
    fn capture_starts_inactive() {
        let capture = TrafficCapture::new(temp_dir(), 0);

        assert!(!capture.is_ongoing());
    }

    #[test]
    fn capture_start_stop_transitions_state() {
        let capture = TrafficCapture::new(temp_dir(), 0);

        let path = capture
            .start_capture(None, Duration::from_secs(1), false)
            .expect("capture should start");
        assert_eq!(path.parent(), Some(PathBuf::from(temp_dir()).as_path()));
        assert!(capture.is_ongoing());

        capture.stop_capture();
        assert!(!capture.is_ongoing());
    }

    #[test]
    fn capture_enqueue_defers_to_writer() {
        let capture = TrafficCapture::new(temp_dir(), 0);

        capture
            .start_capture(None, Duration::from_secs(1), false)
            .expect("capture should start");

        let accepted = capture.enqueue(CaptureRecord {
            timestamp_ns: 1,
            payload: b"test".to_vec(),
            pid: Some(123),
            ancillary: Vec::new(),
            container_id: None,
        });

        assert!(!accepted);
    }
}
