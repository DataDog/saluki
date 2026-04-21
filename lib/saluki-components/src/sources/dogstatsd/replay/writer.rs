use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use datadog_protos::agent::{TaggerState, UnixDogstatsdMsg};
use prost::Message;
use saluki_error::{generic_error, GenericError};
use tracing::{debug, error, warn};
use zstd::stream::write::Encoder as ZstdEncoder;

use super::file::write_header;

const FILE_TEMPLATE_PREFIX: &str = "datadog-capture";

/// Target directory metadata for a capture session.
pub(super) enum CaptureTargetDir {
    /// An explicit user-supplied capture directory.
    Explicit(PathBuf),
    /// An implicit default capture directory.
    Implicit(PathBuf),
}

impl CaptureTargetDir {
    fn as_path(&self) -> &Path {
        match self {
            Self::Explicit(path) | Self::Implicit(path) => path,
        }
    }
}

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
    queue_depth: usize,
    state: Arc<Mutex<WriterState>>,
}

struct WriterState {
    ongoing: bool,
    accepting: bool,
    target_path: Option<PathBuf>,
    compressed: bool,
    traffic_tx: Option<SyncSender<CaptureRecord>>,
}

impl Default for WriterState {
    fn default() -> Self {
        Self {
            ongoing: false,
            accepting: false,
            target_path: None,
            compressed: false,
            traffic_tx: None,
        }
    }
}

enum CaptureFileWriter {
    Plain(BufWriter<File>),
    Compressed(ZstdEncoder<'static, BufWriter<File>>),
}

impl CaptureFileWriter {
    fn finish(self) -> io::Result<()> {
        match self {
            Self::Plain(mut writer) => writer.flush(),
            Self::Compressed(writer) => {
                let mut writer = writer.finish()?;
                writer.flush()
            }
        }
    }
}

impl Write for CaptureFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(writer) => writer.write(buf),
            Self::Compressed(writer) => writer.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(writer) => writer.flush(),
            Self::Compressed(writer) => writer.flush(),
        }
    }
}

impl TrafficCaptureWriter {
    /// Creates a new capture writer with the given queue depth.
    pub(super) fn new(queue_depth: usize) -> Self {
        Self {
            queue_depth,
            state: Arc::new(Mutex::new(WriterState::default())),
        }
    }

    /// Returns whether a capture session is currently active.
    pub(super) fn is_ongoing(&self) -> bool {
        let state = self.state.lock().expect("capture writer mutex poisoned");
        state.ongoing
    }

    /// Starts a new capture session.
    pub(super) fn start_capture(
        &self, target_dir: CaptureTargetDir, duration: Duration, compressed: bool,
    ) -> Result<PathBuf, GenericError> {
        let mut state = self.state.lock().expect("capture writer mutex poisoned");

        if state.ongoing {
            return Err(generic_error!("capture already in progress"));
        }

        let target_path = resolve_target_path(&target_dir)?;
        let mut writer = open_target_writer(&target_path, compressed)?;
        write_header(&mut writer).map_err(|e| generic_error!("Failed to write capture file header: {}", e))?;
        let (traffic_tx, traffic_rx) = mpsc::sync_channel(self.queue_depth);

        state.ongoing = true;
        state.accepting = true;
        state.target_path = Some(target_path.clone());
        state.compressed = compressed;
        state.traffic_tx = Some(traffic_tx);

        let shared_state = Arc::clone(&self.state);
        if let Err(e) = thread::Builder::new()
            .name("dogstatsd-capture-writer".into())
            .spawn(move || run_capture_loop(shared_state, traffic_rx, writer, duration))
        {
            state.ongoing = false;
            state.accepting = false;
            state.target_path = None;
            state.compressed = false;
            state.traffic_tx = None;
            return Err(generic_error!("Failed to spawn capture writer thread: {}", e));
        }

        Ok(target_path)
    }

    /// Stops the current capture session, if one is running.
    pub(super) fn stop_capture(&self) {
        let traffic_tx = {
            let mut state = self.state.lock().expect("capture writer mutex poisoned");

            if !state.ongoing {
                return;
            }

            state.accepting = false;
            state.traffic_tx.take()
        };

        drop(traffic_tx);
    }

    /// Attempts to enqueue a captured packet for persistence.
    pub(super) fn enqueue(&self, record: CaptureRecord) -> bool {
        let traffic_tx = {
            let state = self.state.lock().expect("capture writer mutex poisoned");
            if !state.accepting {
                return false;
            }

            state.traffic_tx.clone()
        };

        match traffic_tx {
            Some(traffic_tx) => traffic_tx.send(record).is_ok(),
            None => false,
        }
    }
}

fn resolve_target_path(target_dir: &CaptureTargetDir) -> Result<PathBuf, GenericError> {
    if target_dir.as_path().as_os_str().is_empty() {
        return Err(generic_error!(
            "DogStatsD capture path is not configured and no explicit path was provided."
        ));
    }

    ensure_target_dir_exists(target_dir)?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| generic_error!("Failed to compute capture file timestamp: {}", e))?
        .as_secs();

    Ok(target_dir
        .as_path()
        .join(format!("{}-{}", FILE_TEMPLATE_PREFIX, timestamp)))
}

fn ensure_target_dir_exists(target_dir: &CaptureTargetDir) -> Result<(), GenericError> {
    match fs::metadata(target_dir.as_path()) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                return Err(generic_error!(
                    "DogStatsD capture path '{}' is not a directory.",
                    target_dir.as_path().display()
                ));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => match target_dir {
            CaptureTargetDir::Implicit(path) => {
                fs::create_dir_all(path).map_err(|create_err| {
                    generic_error!(
                        "Failed to create DogStatsD capture directory '{}': {}",
                        path.display(),
                        create_err
                    )
                })?;
            }
            CaptureTargetDir::Explicit(path) => {
                return Err(generic_error!(
                    "DogStatsD capture path '{}' does not exist.",
                    path.display()
                ));
            }
        }
        Err(e) => {
            return Err(generic_error!(
                "Failed to inspect DogStatsD capture path '{}': {}",
                target_dir.as_path().display(),
                e
            ));
        }
    }

    Ok(())
}

fn open_target_writer(target_path: &Path, compressed: bool) -> Result<CaptureFileWriter, GenericError> {
    let file = File::options()
        .write(true)
        .create_new(true)
        .open(target_path)
        .map_err(|e| generic_error!("Failed to create DogStatsD capture file '{}': {}", target_path.display(), e))?;
    let writer = BufWriter::new(file);

    if compressed {
        let writer = ZstdEncoder::new(writer, 0).map_err(|e| {
            generic_error!(
                "Failed to create compressed DogStatsD capture writer for '{}': {}",
                target_path.display(),
                e
            )
        })?;
        Ok(CaptureFileWriter::Compressed(writer))
    } else {
        Ok(CaptureFileWriter::Plain(writer))
    }
}

fn run_capture_loop(
    state: Arc<Mutex<WriterState>>, traffic_rx: Receiver<CaptureRecord>, mut writer: CaptureFileWriter, duration: Duration,
) {
    let mut pid_map = HashMap::new();
    let start = std::time::Instant::now();

    loop {
        let remaining = duration.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            debug!("DogStatsD capture reached requested duration.");
            break;
        }

        match traffic_rx.recv_timeout(remaining) {
            Ok(record) => {
                if let Err(e) = write_record(&mut writer, record, &mut pid_map) {
                    error!("Failed to write captured DogStatsD record: {}", e);
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => break,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    if let Err(e) = write_state(&mut writer, pid_map, duration) {
        warn!("Failed to write DogStatsD capture state trailer: {}", e);
    }

    if let Err(e) = writer.finish() {
        error!("Failed to finish DogStatsD capture file: {}", e);
    }

    let mut state = state.lock().expect("capture writer mutex poisoned");
    state.ongoing = false;
    state.accepting = false;
    state.target_path = None;
    state.compressed = false;
    state.traffic_tx = None;
}

fn write_record(
    writer: &mut CaptureFileWriter, record: CaptureRecord, pid_map: &mut HashMap<i32, String>,
) -> io::Result<()> {
    let CaptureRecord {
        timestamp_ns,
        payload,
        pid,
        ancillary,
        container_id,
    } = record;

    if let (Some(pid), Some(container_id)) = (pid, container_id) {
        pid_map.insert(pid, container_id);
    }

    let payload_size = payload.len() as i32;
    let ancillary_size = ancillary.len() as i32;
    let message = UnixDogstatsdMsg {
        timestamp: timestamp_ns,
        payload_size,
        payload,
        pid: pid.unwrap_or_default(),
        ancillary_size,
        ancillary,
    };
    let serialized = message.encode_to_vec();

    writer.write_all(&(serialized.len() as u32).to_le_bytes())?;
    writer.write_all(&serialized)?;
    Ok(())
}

fn write_state(writer: &mut CaptureFileWriter, pid_map: HashMap<i32, String>, duration: Duration) -> io::Result<()> {
    let state = TaggerState {
        pid_map,
        duration: duration.as_millis().min(i64::MAX as u128) as i64,
        ..Default::default()
    };
    let serialized = state.encode_to_vec();

    writer.write_all(&[0, 0, 0, 0])?;
    writer.write_all(&serialized)?;
    writer.write_all(&(serialized.len() as u32).to_le_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Cursor,
        thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use prost::Message;

    use super::{CaptureRecord, CaptureTargetDir, TaggerState, TrafficCaptureWriter, UnixDogstatsdMsg};
    use crate::sources::dogstatsd::replay::file::datadog_matcher;

    #[test]
    fn explicit_missing_directory_fails() {
        let writer = TrafficCaptureWriter::new(1);
        let missing_dir = unique_path("explicit-missing");

        let error = writer
            .start_capture(CaptureTargetDir::Explicit(missing_dir.clone()), Duration::from_millis(25), false)
            .expect_err("explicit missing directories should fail");

        assert!(
            error.to_string().contains("does not exist"),
            "unexpected error: {}",
            error
        );
        assert!(!missing_dir.exists());
    }

    #[test]
    fn implicit_missing_directory_is_created() {
        let writer = TrafficCaptureWriter::new(1);
        let implicit_dir = unique_path("implicit-created");

        let capture_path = writer
            .start_capture(CaptureTargetDir::Implicit(implicit_dir.clone()), Duration::from_millis(25), false)
            .expect("implicit directory should be created");

        wait_until_inactive(&writer);

        assert!(implicit_dir.is_dir());
        assert!(capture_path.exists());

        let _ = fs::remove_dir_all(implicit_dir);
    }

    #[test]
    fn uncompressed_capture_writes_header_record_and_state() {
        let writer = TrafficCaptureWriter::new(1);
        let target_dir = unique_dir("writer-uncompressed");

        let capture_path = writer
            .start_capture(CaptureTargetDir::Explicit(target_dir.clone()), Duration::from_millis(250), false)
            .expect("capture should start");
        assert!(writer.enqueue(sample_record()));
        writer.stop_capture();
        wait_until_inactive(&writer);

        let bytes = fs::read(&capture_path).expect("capture should be readable");
        assert_capture_contents(&bytes);

        let _ = fs::remove_dir_all(target_dir);
    }

    #[test]
    fn compressed_capture_writes_same_logical_contents() {
        let writer = TrafficCaptureWriter::new(1);
        let target_dir = unique_dir("writer-compressed");

        let capture_path = writer
            .start_capture(CaptureTargetDir::Explicit(target_dir.clone()), Duration::from_millis(250), true)
            .expect("capture should start");
        assert!(writer.enqueue(sample_record()));
        writer.stop_capture();
        wait_until_inactive(&writer);

        let compressed_bytes = fs::read(&capture_path).expect("capture should be readable");
        let decoded_bytes =
            zstd::stream::decode_all(Cursor::new(compressed_bytes)).expect("compressed capture should decode");
        assert_capture_contents(&decoded_bytes);

        let _ = fs::remove_dir_all(target_dir);
    }

    #[test]
    fn capture_stops_after_requested_duration() {
        let writer = TrafficCaptureWriter::new(1);
        let target_dir = unique_dir("writer-autostop");

        let capture_path = writer
            .start_capture(CaptureTargetDir::Explicit(target_dir.clone()), Duration::from_millis(30), false)
            .expect("capture should start");

        wait_until_inactive(&writer);

        assert!(!writer.is_ongoing());
        assert!(capture_path.exists());

        let _ = fs::remove_dir_all(target_dir);
    }

    fn assert_capture_contents(bytes: &[u8]) {
        assert!(datadog_matcher(bytes));

        let header_len = 8;
        let record_size = u32::from_le_bytes(bytes[header_len..header_len + 4].try_into().expect("record size")) as usize;
        let record_start = header_len + 4;
        let record_end = record_start + record_size;
        let record = UnixDogstatsdMsg::decode(&bytes[record_start..record_end]).expect("record should decode");

        assert_eq!(record.timestamp, 123);
        assert_eq!(record.payload, b"test");
        assert_eq!(record.payload_size, 4);
        assert_eq!(record.pid, 42);
        assert_eq!(record.ancillary, b"oob");
        assert_eq!(record.ancillary_size, 3);

        assert_eq!(&bytes[record_end..record_end + 4], &[0, 0, 0, 0]);

        let state_size_offset = bytes.len() - 4;
        let state_size = u32::from_le_bytes(bytes[state_size_offset..].try_into().expect("state size")) as usize;
        let state_start = state_size_offset - state_size;
        let state = TaggerState::decode(&bytes[state_start..state_size_offset]).expect("state should decode");

        assert_eq!(state.pid_map.get(&42).map(String::as_str), Some("container-123"));
        assert!(!state.duration.is_negative());
    }

    fn sample_record() -> CaptureRecord {
        CaptureRecord {
            timestamp_ns: 123,
            payload: b"test".to_vec(),
            pid: Some(42),
            ancillary: b"oob".to_vec(),
            container_id: Some("container-123".to_string()),
        }
    }

    fn wait_until_inactive(writer: &TrafficCaptureWriter) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while writer.is_ongoing() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }

        assert!(!writer.is_ongoing(), "capture writer did not stop in time");
    }

    fn unique_dir(label: &str) -> std::path::PathBuf {
        let path = unique_path(label);
        fs::create_dir_all(&path).expect("test directory should be created");
        path
    }

    fn unique_path(label: &str) -> std::path::PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("saluki-{}-{}-{}", label, std::process::id(), timestamp))
    }
}
