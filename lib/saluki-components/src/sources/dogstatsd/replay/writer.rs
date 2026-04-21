use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use saluki_error::{generic_error, GenericError};

const FILE_TEMPLATE_PREFIX: &str = "datadog-capture";

/// One raw DogStatsD packet captured before framing and decoding.
#[derive(Debug)]
pub(crate) struct CaptureRecord {
    pub(crate) timestamp_ns: i64,
    pub(crate) payload: Vec<u8>,
    pub(crate) pid: Option<i32>,
    pub(crate) ancillary: Vec<u8>,
    pub(crate) container_id: Option<String>,
}

/// Owns writer-side capture state.
pub(super) struct TrafficCaptureWriter {
    _queue_depth: usize,
    state: Mutex<WriterState>,
}

#[derive(Debug, Default)]
struct WriterState {
    ongoing: bool,
    accepting: bool,
    target_path: Option<PathBuf>,
    compressed: bool,
}

impl TrafficCaptureWriter {
    /// Creates a new capture writer with the given queue depth.
    pub(super) fn new(queue_depth: usize) -> Self {
        Self {
            _queue_depth: queue_depth,
            state: Mutex::new(WriterState::default()),
        }
    }

    /// Returns whether a capture session is currently active.
    pub(super) fn is_ongoing(&self) -> bool {
        let state = self.state.lock().expect("capture writer mutex poisoned");
        state.ongoing
    }

    /// Starts a new capture session.
    pub(super) fn start_capture(
        &self, target_dir: PathBuf, duration: Duration, compressed: bool,
    ) -> Result<PathBuf, GenericError> {
        let mut state = self.state.lock().expect("capture writer mutex poisoned");

        if state.ongoing {
            return Err(generic_error!("capture already in progress"));
        }

        let target_path = resolve_target_path(&target_dir)?;

        state.ongoing = true;
        state.accepting = true;
        state.target_path = Some(target_path.clone());
        state.compressed = compressed;

        // TODO(Task 2):
        // - open the file
        // - create the buffered and optional compressed writer
        // - spawn the background write loop
        // - stop automatically after `duration`
        let _ = duration;

        Ok(target_path)
    }

    /// Stops the current capture session, if one is running.
    pub(super) fn stop_capture(&self) {
        let mut state = self.state.lock().expect("capture writer mutex poisoned");

        if !state.ongoing {
            return;
        }

        state.accepting = false;
        state.ongoing = false;
        state.target_path = None;
        state.compressed = false;

        // TODO(Task 2):
        // - signal the background writer task
        // - flush and close the file
        // - write trailing replay state
    }

    /// Attempts to enqueue a captured packet for persistence.
    pub(super) fn enqueue(&self, record: CaptureRecord) -> bool {
        let state = self.state.lock().expect("capture writer mutex poisoned");
        let CaptureRecord {
            timestamp_ns,
            payload,
            pid,
            ancillary,
            container_id,
        } = record;
        let _ = (timestamp_ns, payload, pid, ancillary, container_id);

        // Task 1 only wires the controller boundary, so packets are never accepted yet.
        state.accepting && false
    }
}

fn resolve_target_path(target_dir: &Path) -> Result<PathBuf, GenericError> {
    if target_dir.as_os_str().is_empty() {
        return Err(generic_error!(
            "DogStatsD capture path is not configured and no explicit path was provided."
        ));
    }

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| generic_error!("Failed to compute capture file timestamp: {}", e))?
        .as_secs();

    Ok(target_dir.join(format!("{}-{}", FILE_TEMPLATE_PREFIX, timestamp)))
}

#[cfg(test)]
mod tests {
    use std::{env::temp_dir, time::Duration};

    use super::{CaptureRecord, TrafficCaptureWriter};

    #[test]
    fn writer_starts_inactive() {
        let writer = TrafficCaptureWriter::new(0);

        assert!(!writer.is_ongoing());
    }

    #[test]
    fn writer_rejects_overlapping_captures() {
        let writer = TrafficCaptureWriter::new(0);

        writer
            .start_capture(temp_dir(), Duration::from_secs(1), false)
            .expect("capture should start");
        let result = writer.start_capture(temp_dir(), Duration::from_secs(1), false);

        assert!(result.is_err());
    }

    #[test]
    fn writer_stop_is_idempotent() {
        let writer = TrafficCaptureWriter::new(0);

        writer.stop_capture();
        assert!(!writer.is_ongoing());
    }

    #[test]
    fn writer_does_not_accept_packets_in_task_one() {
        let writer = TrafficCaptureWriter::new(0);

        writer
            .start_capture(temp_dir(), Duration::from_secs(1), false)
            .expect("capture should start");

        let accepted = writer.enqueue(CaptureRecord {
            timestamp_ns: 1,
            payload: b"test".to_vec(),
            pid: Some(123),
            ancillary: Vec::new(),
            container_id: None,
        });

        assert!(!accepted);
    }
}
