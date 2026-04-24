use std::{
    collections::{BTreeSet, HashMap, HashSet},
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

use datadog_protos::agent::{Entity, EntityId as RemoteEntityId, TaggerState, UnixDogstatsdMsg};
use prost::Message;
use saluki_context::{origin::OriginTagCardinality, tags::SharedTagSet};
use saluki_env::{workload::EntityId, WorkloadProvider};
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
    workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,
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
        Self::with_workload_provider(queue_depth, None)
    }

    /// Creates a new capture writer with the given queue depth and optional workload provider.
    pub(super) fn with_workload_provider(
        queue_depth: usize, workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,
    ) -> Self {
        Self {
            queue_depth,
            workload_provider,
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
        let workload_provider = self.workload_provider.clone();
        if let Err(e) = thread::Builder::new()
            .name("dogstatsd-capture-writer".into())
            .spawn(move || run_capture_loop(shared_state, traffic_rx, writer, duration, workload_provider))
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
        },
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
        .map_err(|e| {
            generic_error!(
                "Failed to create DogStatsD capture file '{}': {}",
                target_path.display(),
                e
            )
        })?;
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
    state: Arc<Mutex<WriterState>>, traffic_rx: Receiver<CaptureRecord>, mut writer: CaptureFileWriter,
    duration: Duration, workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,
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

    if let Err(e) = write_state(&mut writer, pid_map, duration, workload_provider.as_deref()) {
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

fn write_state(
    writer: &mut CaptureFileWriter, pid_map: HashMap<i32, String>, duration: Duration,
    workload_provider: Option<&(dyn WorkloadProvider + Send + Sync)>,
) -> io::Result<()> {
    let state = TaggerState {
        state: build_saved_entity_state(&pid_map, workload_provider),
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

fn build_saved_entity_state(
    pid_map: &HashMap<i32, String>, workload_provider: Option<&(dyn WorkloadProvider + Send + Sync)>,
) -> HashMap<String, Entity> {
    let Some(workload_provider) = workload_provider else {
        return HashMap::new();
    };

    let mut saved_entities = HashMap::new();
    let mut seen_entities = HashSet::new();

    for raw_entity_id in pid_map.values() {
        let Some(entity_id) = parse_entity_id_str(raw_entity_id) else {
            warn!(
                raw_entity_id,
                "Skipping captured entity with an unsupported ID while building replay state."
            );
            continue;
        };

        if !seen_entities.insert(entity_id.clone()) {
            continue;
        }

        let Some(entity) = build_saved_entity(workload_provider, &entity_id) else {
            continue;
        };

        saved_entities.insert(entity_id.to_string(), entity);
    }

    saved_entities
}

fn build_saved_entity(
    workload_provider: &(dyn WorkloadProvider + Send + Sync), entity_id: &EntityId,
) -> Option<Entity> {
    let low_tags =
        sorted_unique_tag_strings(workload_provider.get_tags_for_entity(entity_id, OriginTagCardinality::Low));
    let orchestrator_cumulative =
        sorted_unique_tag_strings(workload_provider.get_tags_for_entity(entity_id, OriginTagCardinality::Orchestrator));
    let high_cumulative =
        sorted_unique_tag_strings(workload_provider.get_tags_for_entity(entity_id, OriginTagCardinality::High));

    let orchestrator_tags = subtract_tag_strings(orchestrator_cumulative.clone(), &low_tags);
    let high_tags = subtract_tag_strings(high_cumulative, &orchestrator_cumulative);

    if low_tags.is_empty() && orchestrator_tags.is_empty() && high_tags.is_empty() {
        return None;
    }

    Some(Entity {
        id: Some(entity_id_to_remote_entity_id(entity_id)),
        low_cardinality_tags: low_tags,
        orchestrator_cardinality_tags: orchestrator_tags,
        high_cardinality_tags: high_tags,
        ..Default::default()
    })
}

fn sorted_unique_tag_strings(tags: Option<SharedTagSet>) -> Vec<String> {
    let Some(tags) = tags else {
        return Vec::new();
    };

    let unique = (&tags)
        .into_iter()
        .map(|tag| tag.as_str().to_string())
        .collect::<BTreeSet<_>>();
    unique.into_iter().collect()
}

fn subtract_tag_strings(current: Vec<String>, lower: &[String]) -> Vec<String> {
    if lower.is_empty() {
        return current;
    }

    let lower = lower.iter().map(String::as_str).collect::<HashSet<_>>();
    current
        .into_iter()
        .filter(|tag| !lower.contains(tag.as_str()))
        .collect()
}

fn entity_id_to_remote_entity_id(entity_id: &EntityId) -> RemoteEntityId {
    match entity_id {
        EntityId::Container(container_id) => RemoteEntityId {
            prefix: "container_id".to_string(),
            uid: container_id.to_string(),
        },
        EntityId::PodUid(pod_uid) => RemoteEntityId {
            prefix: "kubernetes_pod_uid".to_string(),
            uid: pod_uid.to_string(),
        },
        EntityId::Global => RemoteEntityId {
            prefix: "internal".to_string(),
            uid: "global-entity-id".to_string(),
        },
        EntityId::ContainerPid(pid) => RemoteEntityId {
            prefix: "container_pid".to_string(),
            uid: pid.to_string(),
        },
        EntityId::ContainerInode(inode) => RemoteEntityId {
            prefix: "container_inode".to_string(),
            uid: inode.to_string(),
        },
    }
}

fn parse_entity_id_str(value: &str) -> Option<EntityId> {
    const CONTAINER_ID_PREFIX: &str = "container_id://";
    const POD_UID_PREFIX: &str = "kubernetes_pod_uid://";
    const CONTAINER_PID_PREFIX: &str = "container_pid://";
    const CONTAINER_INODE_PREFIX: &str = "container_inode://";

    if let Some(container_id) = value.strip_prefix(CONTAINER_ID_PREFIX) {
        Some(EntityId::Container(container_id.into()))
    } else if let Some(pod_uid) = value.strip_prefix(POD_UID_PREFIX) {
        Some(EntityId::PodUid(pod_uid.into()))
    } else if let Some(pid) = value.strip_prefix(CONTAINER_PID_PREFIX) {
        pid.parse().ok().map(EntityId::ContainerPid)
    } else if let Some(inode) = value.strip_prefix(CONTAINER_INODE_PREFIX) {
        inode.parse().ok().map(EntityId::ContainerInode)
    } else if value == "system://global" {
        Some(EntityId::Global)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        fs,
        io::Cursor,
        sync::Arc,
        thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use prost::Message;
    use saluki_context::{
        origin::OriginTagCardinality,
        tags::{SharedTagSet, Tag, TagSet},
    };
    use saluki_env::{
        workload::{origin::ResolvedOrigin, EntityId},
        WorkloadProvider,
    };

    use super::{CaptureRecord, CaptureTargetDir, TaggerState, TrafficCaptureWriter, UnixDogstatsdMsg};
    use crate::sources::dogstatsd::replay::file::datadog_matcher;
    use crate::sources::dogstatsd::replay::DogStatsDReplayState;

    #[derive(Default)]
    struct MockWorkloadProvider {
        entities: HashMap<EntityId, MockEntityTags>,
    }

    struct MockEntityTags {
        low: SharedTagSet,
        orchestrator: SharedTagSet,
        high: SharedTagSet,
    }

    impl MockWorkloadProvider {
        fn with_entity(entity_id: EntityId, low: &[&str], orchestrator: &[&str], high: &[&str]) -> Self {
            let mut entities = HashMap::new();
            entities.insert(
                entity_id,
                MockEntityTags {
                    low: shared_tags(low),
                    orchestrator: shared_tags(orchestrator),
                    high: shared_tags(high),
                },
            );
            Self { entities }
        }
    }

    impl WorkloadProvider for MockWorkloadProvider {
        fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: OriginTagCardinality) -> Option<SharedTagSet> {
            let tags = self.entities.get(entity_id)?;

            let mut merged = SharedTagSet::default();
            if !tags.low.is_empty() {
                merged.extend_from_shared(&tags.low);
            }
            if cardinality == OriginTagCardinality::Low {
                return (!merged.is_empty()).then_some(merged);
            }

            if !tags.orchestrator.is_empty() {
                merged.extend_from_shared(&tags.orchestrator);
            }
            if cardinality == OriginTagCardinality::Orchestrator {
                return (!merged.is_empty()).then_some(merged);
            }

            if !tags.high.is_empty() {
                merged.extend_from_shared(&tags.high);
            }

            (!merged.is_empty()).then_some(merged)
        }

        fn get_resolved_origin(&self, _origin: saluki_context::origin::RawOrigin<'_>) -> Option<ResolvedOrigin> {
            None
        }
    }

    #[test]
    fn explicit_missing_directory_fails() {
        let writer = TrafficCaptureWriter::new(1);
        let missing_dir = unique_path("explicit-missing");

        let error = writer
            .start_capture(
                CaptureTargetDir::Explicit(missing_dir.clone()),
                Duration::from_millis(25),
                false,
            )
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
            .start_capture(
                CaptureTargetDir::Implicit(implicit_dir.clone()),
                Duration::from_millis(25),
                false,
            )
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
            .start_capture(
                CaptureTargetDir::Explicit(target_dir.clone()),
                Duration::from_millis(250),
                false,
            )
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
            .start_capture(
                CaptureTargetDir::Explicit(target_dir.clone()),
                Duration::from_millis(250),
                true,
            )
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
    fn capture_with_workload_provider_writes_entity_state() {
        let writer = TrafficCaptureWriter::with_workload_provider(
            1,
            Some(Arc::new(MockWorkloadProvider::with_entity(
                EntityId::Container("container-123".into()),
                &["env:prod", "service:api"],
                &["pod_name:api-123"],
                &["container_name:api"],
            ))),
        );
        let target_dir = unique_dir("writer-state");

        let capture_path = writer
            .start_capture(
                CaptureTargetDir::Explicit(target_dir.clone()),
                Duration::from_millis(250),
                false,
            )
            .expect("capture should start");
        assert!(writer.enqueue(sample_record()));
        writer.stop_capture();
        wait_until_inactive(&writer);

        let bytes = fs::read(&capture_path).expect("capture should be readable");
        let state = decode_capture_state(&bytes);
        let entity = state
            .state
            .get("container_id://container-123")
            .expect("capture should include entity state");

        assert_eq!(
            entity.low_cardinality_tags,
            vec!["env:prod".to_string(), "service:api".to_string()]
        );
        assert_eq!(
            entity.orchestrator_cardinality_tags,
            vec!["pod_name:api-123".to_string()]
        );
        assert_eq!(entity.high_cardinality_tags, vec!["container_name:api".to_string()]);
        assert_eq!(
            state.pid_map.get(&42).map(String::as_str),
            Some("container_id://container-123")
        );

        let replay_state = DogStatsDReplayState::new();
        replay_state
            .load(state.clone())
            .expect("capture state should load back into replay state");
        assert_eq!(
            replay_state.resolve_container_entity_for_pid(42),
            Some(EntityId::Container("container-123".into()))
        );
        let high_tags = replay_state
            .get_tags_for_entity(&EntityId::ContainerPid(42), OriginTagCardinality::High)
            .expect("high-cardinality replay tags should resolve");
        assert_eq!(
            TagSet::from_iter((&high_tags).into_iter().cloned()),
            TagSet::from_iter([
                Tag::from("container_name:api"),
                Tag::from("env:prod"),
                Tag::from("pod_name:api-123"),
                Tag::from("service:api"),
            ])
        );

        let _ = fs::remove_dir_all(target_dir);
    }

    #[test]
    fn capture_stops_after_requested_duration() {
        let writer = TrafficCaptureWriter::new(1);
        let target_dir = unique_dir("writer-autostop");

        let capture_path = writer
            .start_capture(
                CaptureTargetDir::Explicit(target_dir.clone()),
                Duration::from_millis(30),
                false,
            )
            .expect("capture should start");

        wait_until_inactive(&writer);

        assert!(!writer.is_ongoing());
        assert!(capture_path.exists());

        let _ = fs::remove_dir_all(target_dir);
    }

    fn assert_capture_contents(bytes: &[u8]) {
        assert!(datadog_matcher(bytes));

        let header_len = 8;
        let record_size =
            u32::from_le_bytes(bytes[header_len..header_len + 4].try_into().expect("record size")) as usize;
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

        let state = decode_capture_state(bytes);

        assert!(
            state.state.is_empty(),
            "captures without a workload provider should keep degraded replay state"
        );
        assert_eq!(
            state.pid_map.get(&42).map(String::as_str),
            Some("container_id://container-123")
        );
        assert!(!state.duration.is_negative());
    }

    fn sample_record() -> CaptureRecord {
        CaptureRecord {
            timestamp_ns: 123,
            payload: b"test".to_vec(),
            pid: Some(42),
            ancillary: b"oob".to_vec(),
            container_id: Some("container_id://container-123".to_string()),
        }
    }

    fn decode_capture_state(bytes: &[u8]) -> TaggerState {
        let state_size_offset = bytes.len() - 4;
        let state_size = u32::from_le_bytes(bytes[state_size_offset..].try_into().expect("state size")) as usize;
        let state_start = state_size_offset - state_size;
        TaggerState::decode(&bytes[state_start..state_size_offset]).expect("state should decode")
    }

    fn shared_tags(tags: &[&str]) -> SharedTagSet {
        TagSet::from_iter(tags.iter().copied().map(Tag::from)).into_shared()
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
