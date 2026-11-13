use std::{
    collections::{BTreeSet, HashSet},
    fs::{self, File},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use datadog_protos::agent::{Entity, EntityId as RemoteEntityId, TaggerState, UnixDogstatsdMsg};
use prost::Message;
use saluki_common::{collections::FastHashMap, time::get_unix_timestamp};
use saluki_core::data_model::{origin::OriginTagCardinality, tags::SharedTagSet};
use saluki_env::{workload::EntityId, WorkloadProvider};
use saluki_error::{generic_error, GenericError};
use tracing::{debug, error, warn};
use zstd::stream::write::Encoder as ZstdEncoder;

use super::super::MIN_CAPTURE_DEPTH;
use super::file_header::write_header;

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
    pub(crate) container_id: Option<String>,
}

/// Owns writer-side capture state.
pub(super) struct TrafficCaptureWriter {
    queue_depth: usize,
    workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,
    // Only written while holding `traffic_tx`, so it can be checked under that lock.
    ongoing: Arc<AtomicBool>,
    // `Some` while the session accepts records; `stop_capture` clears it before the session ends.
    traffic_tx: Arc<Mutex<Option<SyncSender<CaptureRecord>>>>,
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
    #[cfg(test)]
    pub(super) fn new(queue_depth: usize) -> Self {
        Self::with_workload_provider(queue_depth, None)
    }

    /// Creates a new capture writer with the given queue depth and optional workload provider.
    pub(super) fn with_workload_provider(
        queue_depth: usize, workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,
    ) -> Self {
        Self {
            queue_depth: queue_depth.max(MIN_CAPTURE_DEPTH),
            workload_provider,
            ongoing: Arc::new(AtomicBool::new(false)),
            traffic_tx: Arc::new(Mutex::new(None)),
        }
    }

    /// Returns whether a capture session is currently active.
    pub(super) fn is_ongoing(&self) -> bool {
        self.ongoing.load(Ordering::Acquire)
    }

    /// Starts a new capture session.
    pub(super) fn start_capture(
        &self, target_dir: CaptureTargetDir, duration: Duration, compressed: bool,
    ) -> Result<PathBuf, GenericError> {
        let mut traffic_tx_slot = self.traffic_tx.lock().expect("capture writer mutex poisoned");

        if self.ongoing.load(Ordering::Acquire) {
            return Err(generic_error!("capture already in progress"));
        }

        let target_path = resolve_target_path(&target_dir)?;
        let mut writer = open_target_writer(&target_path, compressed)?;
        write_header(&mut writer).map_err(|e| generic_error!("Failed to write capture file header: {}", e))?;
        let (traffic_tx, traffic_rx) = mpsc::sync_channel(self.queue_depth);

        *traffic_tx_slot = Some(traffic_tx);
        self.ongoing.store(true, Ordering::Release);

        let shared_traffic_tx = Arc::clone(&self.traffic_tx);
        let ongoing = Arc::clone(&self.ongoing);
        let workload_provider = self.workload_provider.clone();
        if let Err(e) = thread::Builder::new()
            .name("dogstatsd-capture-writer".into())
            .spawn(move || {
                run_capture_loop(
                    shared_traffic_tx,
                    ongoing,
                    traffic_rx,
                    writer,
                    duration,
                    workload_provider,
                )
            })
        {
            *traffic_tx_slot = None;
            self.ongoing.store(false, Ordering::Release);
            return Err(generic_error!("Failed to spawn capture writer thread: {}", e));
        }

        Ok(target_path)
    }

    /// Stops the current capture session, if one is running.
    pub(super) fn stop_capture(&self) {
        let traffic_tx = self.traffic_tx.lock().expect("capture writer mutex poisoned").take();
        drop(traffic_tx);
    }

    /// Attempts to enqueue a captured packet for persistence.
    pub(super) fn enqueue(&self, record: CaptureRecord) -> bool {
        let traffic_tx = self.traffic_tx.lock().expect("capture writer mutex poisoned").clone();
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

    let timestamp = get_unix_timestamp();

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
    traffic_tx: Arc<Mutex<Option<SyncSender<CaptureRecord>>>>, ongoing: Arc<AtomicBool>,
    traffic_rx: Receiver<CaptureRecord>, mut writer: CaptureFileWriter, duration: Duration,
    workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,
) {
    let mut pid_map = FastHashMap::<i32, String>::default();
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

    let mut traffic_tx = traffic_tx.lock().expect("capture writer mutex poisoned");
    *traffic_tx = None;
    ongoing.store(false, Ordering::Release);
}

fn write_record(
    writer: &mut CaptureFileWriter, record: CaptureRecord, pid_map: &mut FastHashMap<i32, String>,
) -> io::Result<()> {
    let CaptureRecord {
        timestamp_ns,
        payload,
        pid,
        container_id,
    } = record;

    if let (Some(pid), Some(container_id)) = (pid, container_id) {
        pid_map.insert(pid, container_id);
    }

    let payload_size = payload.len() as i32;
    let message = UnixDogstatsdMsg {
        timestamp: timestamp_ns,
        payload_size,
        payload,
        pid: pid.unwrap_or_default(),
        ancillary_size: 0,
        ancillary: Vec::new(),
    };
    let serialized = message.encode_to_vec();

    writer.write_all(&(serialized.len() as u32).to_le_bytes())?;
    writer.write_all(&serialized)?;
    Ok(())
}

fn write_state(
    writer: &mut CaptureFileWriter, pid_map: FastHashMap<i32, String>, duration: Duration,
    workload_provider: Option<&(dyn WorkloadProvider + Send + Sync)>,
) -> io::Result<()> {
    let state = TaggerState {
        state: build_saved_entity_state(&pid_map, workload_provider)
            .into_iter()
            .collect(),
        pid_map: pid_map.into_iter().collect(),
        duration: duration.as_millis().min(i64::MAX as u128) as i64,
    };
    let serialized = state.encode_to_vec();

    writer.write_all(&[0, 0, 0, 0])?;
    writer.write_all(&serialized)?;
    writer.write_all(&(serialized.len() as u32).to_le_bytes())?;
    Ok(())
}

fn build_saved_entity_state(
    pid_map: &FastHashMap<i32, String>, workload_provider: Option<&(dyn WorkloadProvider + Send + Sync)>,
) -> FastHashMap<String, Entity> {
    let Some(workload_provider) = workload_provider else {
        return FastHashMap::default();
    };

    let mut saved_entities = FastHashMap::default();
    let mut seen_entities = HashSet::new();

    for raw_entity_id in pid_map.values() {
        let Some(entity_id) = parse_entity_id_str(raw_entity_id) else {
            warn!(
                raw_entity_id,
                "Skipping captured entity with an unsupported ID while building capture state trailer."
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
        EntityId::ContainerImageMetadata(digest) => RemoteEntityId {
            prefix: "container_image_metadata".to_string(),
            uid: digest.to_string(),
        },
        EntityId::EcsTask(task_arn) => RemoteEntityId {
            prefix: "ecs_task".to_string(),
            uid: task_arn.to_string(),
        },
        EntityId::KubernetesDeployment(deployment) => RemoteEntityId {
            prefix: "deployment".to_string(),
            uid: deployment.to_string(),
        },
        EntityId::KubernetesMetadata(metadata) => RemoteEntityId {
            prefix: "kubernetes_metadata".to_string(),
            uid: metadata.to_string(),
        },
        EntityId::KubernetesNode(node) => RemoteEntityId {
            prefix: "kubernetes_node".to_string(),
            uid: node.to_string(),
        },
        EntityId::PodUid(pod_uid) => RemoteEntityId {
            prefix: "kubernetes_pod_uid".to_string(),
            uid: pod_uid.to_string(),
        },
        EntityId::Global => RemoteEntityId {
            prefix: "internal".to_string(),
            uid: "global-entity-id".to_string(),
        },
        EntityId::Process(process_id) => RemoteEntityId {
            prefix: "process".to_string(),
            uid: process_id.to_string(),
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

pub(super) fn parse_entity_id_str(value: &str) -> Option<EntityId> {
    const CONTAINER_ID_PREFIX: &str = "container_id://";
    const CONTAINER_IMAGE_METADATA_PREFIX: &str = "container_image_metadata://";
    const ECS_TASK_PREFIX: &str = "ecs_task://";
    const KUBERNETES_DEPLOYMENT_PREFIX: &str = "deployment://";
    const KUBERNETES_METADATA_PREFIX: &str = "kubernetes_metadata://";
    const KUBERNETES_NODE_PREFIX: &str = "kubernetes_node://";
    const POD_UID_PREFIX: &str = "kubernetes_pod_uid://";
    const PROCESS_PREFIX: &str = "process://";
    const CONTAINER_PID_PREFIX: &str = "container_pid://";
    const CONTAINER_INODE_PREFIX: &str = "container_inode://";

    if let Some(container_id) = value.strip_prefix(CONTAINER_ID_PREFIX) {
        Some(EntityId::Container(container_id.into()))
    } else if let Some(digest) = value.strip_prefix(CONTAINER_IMAGE_METADATA_PREFIX) {
        Some(EntityId::ContainerImageMetadata(digest.into()))
    } else if let Some(task_arn) = value.strip_prefix(ECS_TASK_PREFIX) {
        Some(EntityId::EcsTask(task_arn.into()))
    } else if let Some(deployment) = value.strip_prefix(KUBERNETES_DEPLOYMENT_PREFIX) {
        Some(EntityId::KubernetesDeployment(deployment.into()))
    } else if let Some(metadata) = value.strip_prefix(KUBERNETES_METADATA_PREFIX) {
        Some(EntityId::KubernetesMetadata(metadata.into()))
    } else if let Some(node) = value.strip_prefix(KUBERNETES_NODE_PREFIX) {
        Some(EntityId::KubernetesNode(node.into()))
    } else if let Some(pod_uid) = value.strip_prefix(POD_UID_PREFIX) {
        Some(EntityId::PodUid(pod_uid.into()))
    } else if let Some(process_id) = value.strip_prefix(PROCESS_PREFIX) {
        Some(EntityId::Process(process_id.into()))
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
    use std::{fs, sync::Arc, time::Duration};

    use saluki_env::workload::{providers::TestWorkloadProvider, EntityId};

    use super::super::test_support::{run_capture, temp_dir, wait_until_inactive};
    use super::{CaptureRecord, CaptureTargetDir, TrafficCaptureWriter, MIN_CAPTURE_DEPTH};
    use crate::sources::dogstatsd::replay::{file_header::valid_header, TrafficCaptureReader};

    #[test]
    fn queue_depth_is_raised_to_minimum() {
        let writer = TrafficCaptureWriter::new(0);
        assert_eq!(writer.queue_depth, MIN_CAPTURE_DEPTH);

        let writer = TrafficCaptureWriter::new(MIN_CAPTURE_DEPTH - 1);
        assert_eq!(writer.queue_depth, MIN_CAPTURE_DEPTH);

        let writer = TrafficCaptureWriter::new(MIN_CAPTURE_DEPTH + 1);
        assert_eq!(writer.queue_depth, MIN_CAPTURE_DEPTH + 1);
    }

    #[test]
    fn explicit_missing_directory_fails() {
        let writer = TrafficCaptureWriter::new(1);
        let parent = temp_dir();
        let missing_dir = parent.path().join("explicit-missing");

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
        let parent = temp_dir();
        let implicit_dir = parent.path().join("implicit-created");

        let capture_path = writer
            .start_capture(
                CaptureTargetDir::Implicit(implicit_dir.clone()),
                Duration::from_millis(25),
                false,
            )
            .expect("implicit directory should be created");

        wait_until_inactive(|| writer.is_ongoing());

        assert!(implicit_dir.is_dir());
        assert!(capture_path.exists());
    }

    #[test]
    fn capture_writes_header_record_and_state() {
        for compressed in [false, true] {
            let (_dir, path) = run_capture(&TrafficCaptureWriter::new(1), compressed, [sample_record()]);

            // A compressed capture must not start with the plain header; the reader then only opens it via zstd.
            let raw = fs::read(&path).expect("capture should be readable");
            assert_eq!(valid_header(&raw), !compressed, "compressed={compressed}");

            let mut reader = TrafficCaptureReader::from_path(&path).expect("capture should open");
            let record = reader
                .read_next()
                .expect("record should decode")
                .unwrap_or_else(|| panic!("record present (compressed={compressed})"));
            assert_eq!(record.timestamp, 123, "compressed={compressed}");
            assert_eq!(record.payload, b"test", "compressed={compressed}");
            assert_eq!(record.payload_size, 4, "compressed={compressed}");
            assert_eq!(record.pid, 42, "compressed={compressed}");
            assert!(record.ancillary.is_empty(), "compressed={compressed}");
            assert_eq!(record.ancillary_size, 0, "compressed={compressed}");

            assert!(
                reader.read_next().expect("separator should read").is_none(),
                "compressed={compressed}"
            );
            assert!(reader.stopped_at_state_separator(), "compressed={compressed}");

            let state = reader
                .read_state()
                .expect("state should decode")
                .unwrap_or_else(|| panic!("state present (compressed={compressed})"));
            assert!(
                state.state.is_empty(),
                "captures without a workload provider should not include entity state (compressed={compressed})"
            );
            assert_eq!(
                state.pid_map.get(&42).map(String::as_str),
                Some("container_id://container-123"),
                "compressed={compressed}"
            );
            assert!(!state.duration.is_negative(), "compressed={compressed}");
        }
    }

    #[test]
    fn capture_with_workload_provider_writes_entity_state() {
        let writer = TrafficCaptureWriter::with_workload_provider(
            1,
            Some(Arc::new(TestWorkloadProvider::with_entity_cardinalities(
                EntityId::Container("container-123".into()),
                &["env:prod", "service:api"],
                &["pod_name:api-123"],
                &["container_name:api"],
            ))),
        );
        let (_dir, path) = run_capture(&writer, false, [sample_record()]);

        let state = TrafficCaptureReader::from_path(&path)
            .expect("capture should open")
            .read_state()
            .expect("state should decode")
            .expect("state present");
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
    }

    #[test]
    fn capture_stops_after_requested_duration() {
        let writer = TrafficCaptureWriter::new(1);
        let target_dir = temp_dir();

        let capture_path = writer
            .start_capture(
                CaptureTargetDir::Explicit(target_dir.path().to_path_buf()),
                Duration::from_millis(30),
                false,
            )
            .expect("capture should start");

        wait_until_inactive(|| writer.is_ongoing());

        assert!(!writer.is_ongoing());
        assert!(capture_path.exists());
    }

    fn sample_record() -> CaptureRecord {
        CaptureRecord {
            timestamp_ns: 123,
            payload: b"test".to_vec(),
            pid: Some(42),
            container_id: Some("container_id://container-123".to_string()),
        }
    }
}
