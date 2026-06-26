//! Reader for Datadog DogStatsD capture files.
//!
//! Decodes a `.dog` or `.dog.zstd` capture file into the sequence of `UnixDogstatsdMsg` records it
//! contains, plus the optional `TaggerState` trailer.

use std::{fs, path::Path};

use datadog_protos::agent::{TaggerState, UnixDogstatsdMsg};
use prost::Message;
use saluki_error::{generic_error, GenericError};

use super::file_header::{file_version, valid_header, DATADOG_HEADER, MIN_NANO_VERSION, MIN_STATE_VERSION};

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const LENGTH_PREFIX_SIZE: usize = 4;

/// Timestamp resolution recorded in a capture file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampResolution {
    /// Timestamps are recorded in whole seconds (file version < 3).
    Seconds,
    /// Timestamps are recorded in nanoseconds (file version >= 3).
    Nanoseconds,
}

/// Reads back a DogStatsD traffic capture file.
#[derive(Debug)]
pub struct TrafficCaptureReader {
    contents: Vec<u8>,
    version: u8,
    offset: usize,
}

impl TrafficCaptureReader {
    /// Opens a capture file at the given path.
    ///
    /// Detects zstd-compressed inputs by magic bytes and decompresses transparently. Validates the
    /// Datadog capture header and parses the file version.
    pub fn from_path(path: &Path) -> Result<Self, GenericError> {
        let raw =
            fs::read(path).map_err(|e| generic_error!("Failed to read capture file '{}': {}", path.display(), e))?;

        let contents = if has_zstd_magic(&raw) {
            zstd::stream::decode_all(raw.as_slice())
                .map_err(|e| generic_error!("Failed to decompress capture file '{}': {}", path.display(), e))?
        } else {
            raw
        };

        if !valid_header(&contents) {
            return Err(generic_error!(
                "Capture file '{}' does not begin with a valid Datadog capture header.",
                path.display()
            ));
        }

        let version = file_version(&contents)?;

        Ok(Self {
            contents,
            version,
            offset: DATADOG_HEADER.len(),
        })
    }

    /// Returns the capture file version.
    pub fn version(&self) -> u8 {
        self.version
    }

    /// Returns the timestamp resolution implied by the file version.
    pub fn timestamp_resolution(&self) -> TimestampResolution {
        if self.version < MIN_NANO_VERSION {
            TimestampResolution::Seconds
        } else {
            TimestampResolution::Nanoseconds
        }
    }

    /// Reads the next captured DogStatsD record from the file.
    ///
    /// Returns `Ok(None)` when the stream of records is exhausted: at EOF, at the four-byte state
    /// separator that introduces the tagger trailer, or at a truncated record boundary.
    pub fn read_next(&mut self) -> Result<Option<UnixDogstatsdMsg>, GenericError> {
        if self.offset + LENGTH_PREFIX_SIZE > self.contents.len() {
            return Ok(None);
        }

        let size_bytes = &self.contents[self.offset..self.offset + LENGTH_PREFIX_SIZE];
        let size = u32::from_le_bytes(size_bytes.try_into().expect("length prefix is 4 bytes")) as usize;
        self.offset += LENGTH_PREFIX_SIZE;

        // The writer emits a zero-length prefix to mark the start of the tagger state trailer; treat
        // that (and any size that would overrun the buffer) as the end of the record stream.
        if size == 0 || self.offset + size > self.contents.len() {
            // A zero-length prefix is the legitimate trailer marker. A non-zero `size` that overruns the buffer is a
            // corrupt/oversized length prefix being silently read as clean EOF, which drops every following
            // well-formed record. Surface the corrupt case as distinct from a real trailer.
            saluki_antithesis::always_or_unreachable!(
                size == 0,
                "replay read_next stopped at the real trailer, not on a corrupt length prefix",
                { "size": size, "offset": self.offset, "len": self.contents.len() }
            );

            return Ok(None);
        }

        let msg = UnixDogstatsdMsg::decode(&self.contents[self.offset..self.offset + size])
            .map_err(|e| generic_error!("Failed to decode captured DogStatsD record: {}", e))?;
        self.offset += size;

        Ok(Some(msg))
    }

    /// Reads the tagger state trailer from the end of the capture file.
    ///
    /// Returns `Ok(None)` if the file version predates state support, or if the trailer is empty.
    /// Does not modify the read offset, so it can be called independently of `read_next`.
    pub fn read_state(&self) -> Result<Option<TaggerState>, GenericError> {
        if self.version < MIN_STATE_VERSION {
            return Ok(None);
        }

        let len = self.contents.len();
        if len < LENGTH_PREFIX_SIZE {
            return Ok(None);
        }

        let size_bytes = &self.contents[len - LENGTH_PREFIX_SIZE..len];
        let size = u32::from_le_bytes(size_bytes.try_into().expect("length suffix is 4 bytes")) as usize;
        if size == 0 {
            return Ok(None);
        }

        if size + LENGTH_PREFIX_SIZE > len {
            return Err(generic_error!(
                "Tagger state trailer size ({}) exceeds capture file length ({}).",
                size,
                len
            ));
        }

        let state_start = len - LENGTH_PREFIX_SIZE - size;
        let state = TaggerState::decode(&self.contents[state_start..len - LENGTH_PREFIX_SIZE])
            .map_err(|e| generic_error!("Failed to decode tagger state trailer: {}", e))?;
        Ok(Some(state))
    }
}

fn has_zstd_magic(buf: &[u8]) -> bool {
    buf.len() >= ZSTD_MAGIC.len() && buf[..ZSTD_MAGIC.len()] == ZSTD_MAGIC
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::Arc,
        thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use saluki_common::collections::FastHashMap;
    use saluki_context::{
        origin::OriginTagCardinality,
        tags::{SharedTagSet, Tag, TagSet},
    };
    use saluki_env::{
        workload::{origin::ResolvedOrigin, EntityId},
        WorkloadProvider,
    };

    use super::*;
    use crate::sources::dogstatsd::replay::writer::{CaptureRecord, CaptureTargetDir, TrafficCaptureWriter};

    #[derive(Default)]
    struct MockWorkloadProvider {
        entities: FastHashMap<EntityId, SharedTagSet>,
    }

    impl MockWorkloadProvider {
        fn with_entity(entity_id: EntityId, tags: &[&str]) -> Self {
            let mut entities = FastHashMap::default();
            entities.insert(entity_id, shared_tags(tags));
            Self { entities }
        }
    }

    impl WorkloadProvider for MockWorkloadProvider {
        fn get_tags_for_entity(
            &self, entity_id: &EntityId, _cardinality: OriginTagCardinality,
        ) -> Option<SharedTagSet> {
            self.entities.get(entity_id).cloned()
        }

        fn get_resolved_origin(&self, _origin: saluki_context::origin::RawOrigin<'_>) -> Option<ResolvedOrigin> {
            None
        }
    }

    #[test]
    fn plain_capture_round_trip() {
        let (path, _dir_guard) = run_capture(1, false, &[sample_record(100, b"metric.a:1|c", 11)]);

        let mut reader = TrafficCaptureReader::from_path(&path).expect("reader should open");
        assert_eq!(reader.version(), 3);
        assert_eq!(reader.timestamp_resolution(), TimestampResolution::Nanoseconds);

        let msg = reader.read_next().expect("read should succeed").expect("first record");
        assert_eq!(msg.timestamp, 100);
        assert_eq!(msg.payload, b"metric.a:1|c");
        assert_eq!(msg.payload_size, msg.payload.len() as i32);
        assert_eq!(msg.pid, 11);

        assert!(reader.read_next().expect("read should succeed").is_none());
    }

    #[test]
    fn compressed_capture_round_trip() {
        let (path, _dir_guard) = run_capture(
            1,
            true,
            &[
                sample_record(1, b"metric.a:1|c", 1),
                sample_record(2, b"metric.b:2|c", 1),
                sample_record(3, b"metric.c:3|c", 1),
            ],
        );

        let mut reader = TrafficCaptureReader::from_path(&path).expect("reader should open");

        for expected_ts in [1, 2, 3] {
            let msg = reader.read_next().expect("read should succeed").expect("record");
            assert_eq!(msg.timestamp, expected_ts);
        }

        assert!(reader.read_next().expect("read should succeed").is_none());
    }

    #[test]
    fn read_next_stops_at_state_separator() {
        let (path, _dir_guard) = run_capture(1, false, &[sample_record(7, b"x:1|c", 5)]);

        let mut reader = TrafficCaptureReader::from_path(&path).expect("reader should open");
        assert!(reader.read_next().expect("first record").is_some());

        // Subsequent calls must yield None rather than attempting to decode the trailer as a record.
        assert!(reader.read_next().expect("trailer boundary").is_none());
        assert!(reader.read_next().expect("idempotent EOF").is_none());
    }

    #[test]
    fn truncated_record_returns_none() {
        let (path, _dir_guard) = run_capture(1, false, &[sample_record(1, b"metric.a:1|c", 1)]);

        let bytes = fs::read(&path).expect("capture readable");
        let truncated_path = path.with_extension("truncated");
        // Drop the last 8 bytes so the trailer length prefix is gone and a record is incomplete.
        fs::write(&truncated_path, &bytes[..bytes.len().saturating_sub(8)]).expect("write truncated");

        let mut reader = TrafficCaptureReader::from_path(&truncated_path).expect("reader should open");
        let _ = reader.read_next();
        // Whatever the reader recovered, the next call must terminate cleanly rather than error.
        assert!(reader.read_next().expect("clean EOF on truncation").is_none());
    }

    #[test]
    fn bad_header_is_rejected() {
        let tmp = unique_path("reader-bad-header");
        fs::write(&tmp, b"this is not a capture file").expect("write garbage");

        let err = TrafficCaptureReader::from_path(&tmp).expect_err("bad header should fail");
        assert!(err.to_string().contains("Datadog capture header"));

        let _ = fs::remove_file(&tmp);
    }

    #[test]
    fn read_state_recovers_entity_tags() {
        let target_dir = unique_dir("reader-state");
        let workload = Arc::new(MockWorkloadProvider::with_entity(
            EntityId::Container("container-xyz".into()),
            &["env:prod", "service:api"],
        ));
        let writer = TrafficCaptureWriter::with_workload_provider(1, Some(workload));

        let path = writer
            .start_capture(
                CaptureTargetDir::Explicit(target_dir.clone()),
                Duration::from_millis(250),
                false,
            )
            .expect("capture should start");
        assert!(writer.enqueue(CaptureRecord {
            timestamp_ns: 1,
            payload: b"metric:1|c".to_vec(),
            pid: Some(99),
            ancillary: Vec::new(),
            container_id: Some("container_id://container-xyz".to_string()),
        }));
        writer.stop_capture();
        wait_until_inactive(&writer);

        let reader = TrafficCaptureReader::from_path(&path).expect("reader should open");
        let state = reader
            .read_state()
            .expect("state should decode")
            .expect("state present");
        let entity = state
            .state
            .get("container_id://container-xyz")
            .expect("captured entity present");
        assert_eq!(
            entity.low_cardinality_tags,
            vec!["env:prod".to_string(), "service:api".to_string()]
        );
        assert_eq!(
            state.pid_map.get(&99).map(String::as_str),
            Some("container_id://container-xyz")
        );

        let _ = fs::remove_dir_all(target_dir);
    }

    fn run_capture(queue_depth: usize, compressed: bool, records: &[CaptureRecord]) -> (PathBuf, DirGuard) {
        let target_dir = unique_dir("reader-capture");
        let writer = TrafficCaptureWriter::new(queue_depth);

        let path = writer
            .start_capture(
                CaptureTargetDir::Explicit(target_dir.clone()),
                Duration::from_millis(500),
                compressed,
            )
            .expect("capture should start");

        for record in records {
            assert!(writer.enqueue(clone_record(record)));
        }
        writer.stop_capture();
        wait_until_inactive(&writer);

        (path, DirGuard { path: target_dir })
    }

    fn clone_record(record: &CaptureRecord) -> CaptureRecord {
        CaptureRecord {
            timestamp_ns: record.timestamp_ns,
            payload: record.payload.clone(),
            pid: record.pid,
            ancillary: record.ancillary.clone(),
            container_id: record.container_id.clone(),
        }
    }

    fn sample_record(timestamp_ns: i64, payload: &[u8], pid: i32) -> CaptureRecord {
        CaptureRecord {
            timestamp_ns,
            payload: payload.to_vec(),
            pid: Some(pid),
            ancillary: Vec::new(),
            container_id: Some(format!("container_id://container-{}", pid)),
        }
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
