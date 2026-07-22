use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use saluki_components::transforms::{AggregateContextSnapshotEntry, AggregateMetricType};
use saluki_context::tags::TagSet;
use saluki_error::{ErrorContext as _, GenericError};
use serde::{ser::SerializeStruct as _, Deserialize, Serialize, Serializer};
use uuid::Uuid;

const ZSTD_MAGIC: &[u8; 4] = b"\x28\xb5\x2f\xfd";
pub(crate) const CONTEXT_DUMP_FILENAME: &str = "dogstatsd_contexts.json.zstd";

struct AgentContextSnapshotRecord<'a>(&'a AggregateContextSnapshotEntry);

impl Serialize for AgentContextSnapshotRecord<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let entry = self.0;
        let context = entry.context();
        let name: &str = context.name().as_ref();
        let mut record = serializer.serialize_struct("AgentContextRecord", 7)?;
        record.serialize_field("Name", name)?;
        record.serialize_field("Host", context.host().unwrap_or_default())?;
        record.serialize_field("Type", agent_metric_type(entry))?;
        record.serialize_field("TaggerTags", &BorrowedTags(context.origin_tags()))?;
        record.serialize_field("MetricTags", &BorrowedTags(context.tags()))?;
        record.serialize_field("NoIndex", &false)?;
        record.serialize_field("Source", &1u32)?;
        record.end()
    }
}

struct BorrowedTags<'a>(&'a TagSet);

impl Serialize for BorrowedTags<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_seq(self.0.into_iter().map(|tag| tag.as_str()))
    }
}

fn agent_metric_type(entry: &AggregateContextSnapshotEntry) -> &'static str {
    match entry.metric_type() {
        AggregateMetricType::Counter => "Counter",
        AggregateMetricType::Rate => "Rate",
        AggregateMetricType::Gauge => "Gauge",
        AggregateMetricType::Set => "Set",
        AggregateMetricType::Distribution => "Distribution",
        AggregateMetricType::Histogram if entry.unit() == Some("millisecond") => "Historate",
        AggregateMetricType::Histogram => "Histogram",
    }
}

fn write_snapshot_records<W: Write>(
    writer: &mut W, snapshot: &[AggregateContextSnapshotEntry], path: &Path,
) -> Result<(), GenericError> {
    for (index, entry) in snapshot.iter().enumerate() {
        serde_json::to_writer(&mut *writer, &AgentContextSnapshotRecord(entry)).with_error_context(|| {
            format!(
                "Failed to serialize DogStatsD context dump record {} for '{}'.",
                index + 1,
                path.display()
            )
        })?;
        writer.write_all(b"\n").with_error_context(|| {
            format!(
                "Failed to write DogStatsD context dump record {} terminator for '{}'.",
                index + 1,
                path.display()
            )
        })?;
    }
    Ok(())
}

pub(crate) fn publish_context_dump(
    run_path: &Path, snapshot: &[AggregateContextSnapshotEntry],
) -> Result<PathBuf, GenericError> {
    publish_context_dump_with(run_path, snapshot, &FileSystemAtomicReplace)
}

fn publish_context_dump_with<R: AtomicReplace>(
    run_path: &Path, snapshot: &[AggregateContextSnapshotEntry], replacer: &R,
) -> Result<PathBuf, GenericError> {
    publish_context_dump_with_services(
        run_path,
        snapshot,
        replacer,
        &SystemEntropy,
        &OwnerOnlyPermissions,
        &FileSystemCleanup,
    )
}

fn publish_context_dump_with_services<R, E, P, C>(
    run_path: &Path, snapshot: &[AggregateContextSnapshotEntry], replacer: &R, entropy: &E, permissions: &P,
    cleanup: &C,
) -> Result<PathBuf, GenericError>
where
    R: AtomicReplace,
    E: EntropySource,
    P: PermissionNormalizer,
    C: TemporaryFileCleanup,
{
    let target = run_path.join(CONTEXT_DUMP_FILENAME);
    if run_path.as_os_str().is_empty() {
        return Err(saluki_error::generic_error!(
            "Failed to validate run path for DogStatsD context dump target '{}': the run path is empty.",
            target.display()
        ));
    }

    let temporary_path = temporary_context_dump_path(run_path, &target, entropy)?;
    let file = open_temporary_file(&temporary_path).with_error_context(|| {
        format!(
            "Failed to create temporary DogStatsD context dump for target '{}'.",
            target.display()
        )
    })?;

    run_with_temporary_cleanup(&temporary_path, &target, cleanup, || {
        permissions.normalize(&file).with_error_context(|| {
            format!(
                "Failed to set owner-only permissions on temporary DogStatsD context dump '{}' for target '{}'.",
                temporary_path.display(),
                target.display()
            )
        })?;
        publish_buffered_temporary_unmanaged(BufWriter::new(file), &temporary_path, &target, snapshot, replacer)
    })?;
    Ok(target)
}

fn temporary_context_dump_path<E: EntropySource>(
    run_path: &Path, target: &Path, entropy: &E,
) -> Result<PathBuf, GenericError> {
    let mut random_bytes = [0u8; 16];
    entropy.fill(&mut random_bytes).with_error_context(|| {
        format!(
            "Failed to generate random temporary filename for DogStatsD context dump target '{}'.",
            target.display()
        )
    })?;
    random_bytes[6] = (random_bytes[6] & 0x0f) | 0x40;
    random_bytes[8] = (random_bytes[8] & 0x3f) | 0x80;
    let identifier = Uuid::from_bytes(random_bytes);
    Ok(run_path.join(format!(".{CONTEXT_DUMP_FILENAME}.{}.tmp", identifier.as_hyphenated())))
}

trait EntropySource {
    fn fill(&self, bytes: &mut [u8]) -> io::Result<()>;
}

struct SystemEntropy;

impl EntropySource for SystemEntropy {
    fn fill(&self, bytes: &mut [u8]) -> io::Result<()> {
        getrandom::fill(bytes).map_err(|error| io::Error::other(error.to_string()))
    }
}

trait PermissionNormalizer {
    fn normalize(&self, file: &File) -> io::Result<()>;
}

struct OwnerOnlyPermissions;

impl PermissionNormalizer for OwnerOnlyPermissions {
    fn normalize(&self, file: &File) -> io::Result<()> {
        set_owner_only_permissions(file)
    }
}

#[cfg(unix)]
fn set_owner_only_permissions(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_owner_only_permissions(_file: &File) -> io::Result<()> {
    Ok(())
}

fn open_temporary_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(all(test, not(feature = "context-dump-benchmark")))]
fn publish_open_temporary<W, R>(
    writer: W, temporary_path: &Path, target: &Path, snapshot: &[AggregateContextSnapshotEntry], replacer: &R,
) -> Result<(), GenericError>
where
    W: Write + SyncAll,
    R: AtomicReplace,
{
    run_with_temporary_cleanup(temporary_path, target, &FileSystemCleanup, || {
        publish_buffered_temporary_unmanaged(BufWriter::new(writer), temporary_path, target, snapshot, replacer)
    })
}

#[cfg(all(test, not(feature = "context-dump-benchmark")))]
fn publish_buffered_temporary<W, R>(
    buffer: BufWriter<W>, temporary_path: &Path, target: &Path, snapshot: &[AggregateContextSnapshotEntry],
    replacer: &R,
) -> Result<(), GenericError>
where
    W: Write + SyncAll,
    R: AtomicReplace,
{
    run_with_temporary_cleanup(temporary_path, target, &FileSystemCleanup, || {
        publish_buffered_temporary_unmanaged(buffer, temporary_path, target, snapshot, replacer)
    })
}

fn publish_buffered_temporary_unmanaged<W, R>(
    buffer: BufWriter<W>, temporary_path: &Path, target: &Path, snapshot: &[AggregateContextSnapshotEntry],
    replacer: &R,
) -> Result<(), GenericError>
where
    W: Write + SyncAll,
    R: AtomicReplace,
{
    let writer = write_compressed_snapshot(buffer, snapshot, target)?;
    writer.sync_all().with_error_context(|| {
        format!(
            "Failed to sync temporary DogStatsD context dump for target '{}'.",
            target.display()
        )
    })?;
    drop(writer);

    replacer.replace(temporary_path, target).with_error_context(|| {
        format!(
            "Failed to atomically replace DogStatsD context dump target '{}'.",
            target.display()
        )
    })
}

fn run_with_temporary_cleanup<T, C>(
    temporary_path: &Path, target: &Path, cleanup: &C, operation: impl FnOnce() -> Result<T, GenericError>,
) -> Result<T, GenericError>
where
    C: TemporaryFileCleanup + ?Sized,
{
    let mut guard = TemporaryPathCleanup::new(temporary_path, cleanup);
    match operation() {
        Ok(value) => {
            guard.disarm();
            Ok(value)
        }
        Err(primary_error) => match guard.cleanup() {
            Ok(()) => Err(primary_error),
            Err(cleanup_error) => Err(saluki_error::generic_error!(
                "Failed to publish DogStatsD context dump target '{}': {:#}. Additionally, failed to remove temporary \
                 DogStatsD context dump '{}': {}",
                target.display(),
                primary_error,
                temporary_path.display(),
                cleanup_error
            )),
        },
    }
}

fn write_compressed_snapshot<W: Write>(
    buffer: BufWriter<W>, snapshot: &[AggregateContextSnapshotEntry], target: &Path,
) -> Result<W, GenericError> {
    let mut encoder = zstd::stream::write::Encoder::new(buffer, 0).with_error_context(|| {
        format!(
            "Failed to initialize zstd stream for DogStatsD context dump target '{}'.",
            target.display()
        )
    })?;
    write_snapshot_records(&mut encoder, snapshot, target)?;
    let buffer = finish_zstd_encoder(encoder, target)?;
    finish_buffered_writer(buffer, target)
}

fn finish_zstd_encoder<'a, W: Write>(
    encoder: zstd::stream::write::Encoder<'a, W>, target: &Path,
) -> Result<W, GenericError> {
    encoder.finish().with_error_context(|| {
        format!(
            "Failed to finalize zstd stream for DogStatsD context dump target '{}'.",
            target.display()
        )
    })
}

fn finish_buffered_writer<W: Write>(buffer: BufWriter<W>, target: &Path) -> Result<W, GenericError> {
    buffer
        .into_inner()
        .map_err(|error| error.into_error())
        .with_error_context(|| {
            format!(
                "Failed to finalize buffered DogStatsD context dump output for target '{}'.",
                target.display()
            )
        })
}

trait SyncAll {
    fn sync_all(&self) -> io::Result<()>;
}

impl SyncAll for File {
    fn sync_all(&self) -> io::Result<()> {
        File::sync_all(self)
    }
}

trait AtomicReplace {
    fn replace(&self, source: &Path, target: &Path) -> io::Result<()>;
}

struct FileSystemAtomicReplace;

impl AtomicReplace for FileSystemAtomicReplace {
    fn replace(&self, source: &Path, target: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            fs::rename(source, target)
        }

        #[cfg(windows)]
        {
            move_file_replace(source, target)
        }
    }
}

#[cfg(windows)]
fn move_file_replace(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;

    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};

    fn nul_terminated(path: &Path) -> io::Result<Vec<u16>> {
        let mut encoded: Vec<_> = path.as_os_str().encode_wide().collect();
        if encoded.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains an interior NUL",
            ));
        }
        encoded.push(0);
        Ok(encoded)
    }

    let source = nul_terminated(source)?;
    let target = nul_terminated(target)?;
    // SAFETY: Both path buffers are NUL-terminated, contain no interior NULs, and remain alive for the duration of the
    // call. The flags request same-filesystem replacement with write-through semantics.
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

trait TemporaryFileCleanup {
    fn remove(&self, path: &Path) -> io::Result<()>;
}

struct FileSystemCleanup;

impl TemporaryFileCleanup for FileSystemCleanup {
    fn remove(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }
}

struct TemporaryPathCleanup<'a, C: TemporaryFileCleanup + ?Sized> {
    path: &'a Path,
    cleanup: &'a C,
    armed: bool,
}

impl<'a, C: TemporaryFileCleanup + ?Sized> TemporaryPathCleanup<'a, C> {
    fn new(path: &'a Path, cleanup: &'a C) -> Self {
        Self {
            path,
            cleanup,
            armed: true,
        }
    }

    fn cleanup(&mut self) -> io::Result<()> {
        self.cleanup.remove(self.path)?;
        self.disarm();
        Ok(())
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<C: TemporaryFileCleanup + ?Sized> Drop for TemporaryPathCleanup<'_, C> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.cleanup.remove(self.path);
        }
    }
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub(super) struct AgentContextRecord {
    pub(super) name: String,
    pub(super) host: String,
    #[serde(rename = "Type")]
    pub(super) metric_type: String,
    pub(super) tagger_tags: Vec<String>,
    pub(super) metric_tags: Vec<String>,
    pub(super) no_index: bool,
    pub(super) source: u32,
}

#[derive(Clone, Copy, Debug)]
enum JsonErrorCategory {
    Io,
    Syntax,
    Data,
    Eof,
}

impl JsonErrorCategory {
    fn from_serde(category: serde_json::error::Category) -> Self {
        match category {
            serde_json::error::Category::Io => Self::Io,
            serde_json::error::Category::Syntax => Self::Syntax,
            serde_json::error::Category::Data => Self::Data,
            serde_json::error::Category::Eof => Self::Eof,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Io => "I/O",
            Self::Syntax => "syntax",
            Self::Data => "data",
            Self::Eof => "end-of-file",
        }
    }
}

#[derive(Debug)]
struct ArtifactDecodeError {
    category: JsonErrorCategory,
    line: usize,
    column: usize,
}

impl ArtifactDecodeError {
    fn from_serde(error: serde_json::Error) -> Self {
        Self {
            category: JsonErrorCategory::from_serde(error.classify()),
            line: error.line(),
            column: error.column(),
        }
    }
}

impl fmt::Display for ArtifactDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "JSON {} error at line {}, column {}",
            self.category.label(),
            self.line,
            self.column
        )
    }
}

impl Error for ArtifactDecodeError {}

#[derive(Debug)]
pub(super) struct ArtifactError {
    path: PathBuf,
    operation: &'static str,
    record_index: usize,
    source: Box<dyn Error + Send + Sync>,
}

impl ArtifactError {
    fn new(
        path: &Path, operation: &'static str, record_index: usize, source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            path: path.to_owned(),
            operation,
            record_index,
            source: Box::new(source),
        }
    }

    fn decode(path: &Path, record_index: usize, source: serde_json::Error) -> Self {
        Self::new(path, "decode", record_index, ArtifactDecodeError::from_serde(source))
    }

    #[cfg(all(test, not(feature = "context-dump-benchmark")))]
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(all(test, not(feature = "context-dump-benchmark")))]
    pub(super) fn operation(&self) -> &'static str {
        self.operation
    }

    #[cfg(all(test, not(feature = "context-dump-benchmark")))]
    pub(super) fn record_index(&self) -> usize {
        self.record_index
    }
}

impl fmt::Display for ArtifactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}: failed to {} record {}: {}",
            self.path.display(),
            self.operation,
            self.record_index,
            self.source
        )
    }
}

impl Error for ArtifactError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

pub(super) fn for_each_record(path: &Path, mut consume: impl FnMut(AgentContextRecord)) -> Result<(), ArtifactError> {
    let file = File::open(path).map_err(|error| ArtifactError::new(path, "open", 0, error))?;
    let mut compressed_input = BufReader::new(file);
    let is_compressed = compressed_input
        .fill_buf()
        .map_err(|error| ArtifactError::new(path, "inspect compression magic for", 0, error))?
        .starts_with(ZSTD_MAGIC);

    if is_compressed {
        let decoder = zstd::stream::read::Decoder::with_buffer(compressed_input)
            .map_err(|error| ArtifactError::new(path, "initialize zstd decoder for", 0, error))?;
        decode_records(path, BufReader::new(decoder), &mut consume)
    } else {
        decode_records(path, compressed_input, &mut consume)
    }
}

fn decode_records<R: Read>(
    path: &Path, reader: R, consume: &mut impl FnMut(AgentContextRecord),
) -> Result<(), ArtifactError> {
    let records = serde_json::Deserializer::from_reader(reader).into_iter::<AgentContextRecord>();
    for (index, record) in records.enumerate() {
        let record_index = index + 1;
        let record = record.map_err(|error| ArtifactError::decode(path, record_index, error))?;
        consume(record);
    }
    Ok(())
}

#[cfg(all(test, not(feature = "context-dump-benchmark")))]
fn collect_records(path: &Path) -> Result<Vec<AgentContextRecord>, ArtifactError> {
    let mut records = Vec::new();
    for_each_record(path, |record| records.push(record))?;
    Ok(records)
}

#[cfg(all(test, not(feature = "context-dump-benchmark")))]
mod tests {
    use std::fs;
    use std::io::{self, BufWriter, Write as _};
    use std::path::PathBuf;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    };

    use saluki_components::transforms::{AggregateContextSnapshotEntry, AggregateMetricType};
    use saluki_context::{
        tags::{Tag, TagSet},
        Context,
    };
    use serde_json::json;
    use stringtheory::MetaString;

    use super::{
        collect_records, finish_buffered_writer, finish_zstd_encoder, publish_buffered_temporary, publish_context_dump,
        publish_context_dump_with, publish_context_dump_with_services, publish_open_temporary,
        set_owner_only_permissions, temporary_context_dump_path, write_snapshot_records, AgentContextRecord,
        AtomicReplace, EntropySource, FileSystemAtomicReplace, FileSystemCleanup, OwnerOnlyPermissions,
        PermissionNormalizer, SyncAll, TemporaryFileCleanup, TemporaryPathCleanup, CONTEXT_DUMP_FILENAME,
    };
    use crate::dogstatsd_contexts::read_report;

    const PLAIN_FIXTURE_BYTES: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/dogstatsd_contexts_agent.ndjson"
    ));
    const COMPRESSED_FIXTURE_BYTES: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/dogstatsd_contexts_agent.ndjson.zstd"
    ));
    const PLAIN_FIXTURE_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/dogstatsd_contexts_agent.ndjson"
    );
    const COMPRESSED_FIXTURE_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/dogstatsd_contexts_agent.ndjson.zstd"
    );

    // These fixtures and expectations mirror ContextDebugRepr and the `dogstatsd top` command at Datadog Agent
    // commit 9de2a8371cf8d95794f238939709491907893288.
    fn expected_fixture_records() -> Vec<AgentContextRecord> {
        vec![
            AgentContextRecord {
                name: "z.metric".to_owned(),
                host: "node-a".to_owned(),
                metric_type: "Gauge".to_owned(),
                tagger_tags: vec!["pod_name:web-a".to_owned()],
                metric_tags: vec![
                    "env:prod".to_owned(),
                    "image:registry/repo:v1".to_owned(),
                    "bare".to_owned(),
                ],
                no_index: false,
                source: 1,
            },
            AgentContextRecord {
                name: "a.metric".to_owned(),
                host: "node-a".to_owned(),
                metric_type: "Counter".to_owned(),
                tagger_tags: vec!["pod_name:web-a".to_owned()],
                metric_tags: vec!["env:prod".to_owned(), "service:web".to_owned()],
                no_index: false,
                source: 1,
            },
            AgentContextRecord {
                name: "a.metric".to_owned(),
                host: "node-b".to_owned(),
                metric_type: "Counter".to_owned(),
                tagger_tags: vec!["pod_name:web-b".to_owned()],
                metric_tags: vec!["env:prod".to_owned(), "service:web".to_owned()],
                no_index: false,
                source: 1,
            },
            AgentContextRecord {
                name: "a.metric".to_owned(),
                host: "node-c".to_owned(),
                metric_type: "Counter".to_owned(),
                tagger_tags: vec!["pod_name:web-c".to_owned()],
                metric_tags: vec!["env:staging".to_owned(), "service:web".to_owned()],
                no_index: false,
                source: 1,
            },
        ]
    }

    #[test]
    fn streams_exact_agent_schema_for_every_metric_type() {
        let cases = [
            (AggregateMetricType::Counter, None, "Counter"),
            (AggregateMetricType::Rate, None, "Rate"),
            (AggregateMetricType::Gauge, None, "Gauge"),
            (AggregateMetricType::Set, None, "Set"),
            (AggregateMetricType::Distribution, None, "Distribution"),
            (AggregateMetricType::Histogram, None, "Histogram"),
            (AggregateMetricType::Histogram, Some("second"), "Histogram"),
            (AggregateMetricType::Histogram, Some("millisecond"), "Historate"),
        ];
        let snapshot: Vec<_> = cases
            .iter()
            .enumerate()
            .map(|(index, (metric_type, unit, _))| {
                snapshot_entry(
                    Box::leak(format!("metric.{index}").into_boxed_str()),
                    (index == 0).then_some("node-a"),
                    *metric_type,
                    unit.unwrap_or_default(),
                    &["client:value", "bare"],
                    &["origin:value"],
                )
            })
            .collect();
        let mut output = Vec::new();

        write_snapshot_records(&mut output, &snapshot, PathBuf::from("plain-stream").as_path())
            .expect("snapshot should serialize");

        let output = String::from_utf8(output).expect("serialized output should be UTF-8");
        let lines: Vec<_> = output.lines().collect();
        assert_eq!(lines.len(), cases.len());
        assert_eq!(output.bytes().filter(|byte| *byte == b'\n').count(), cases.len());
        assert!(output.ends_with('\n'));
        for (index, ((_, _, expected_type), line)) in cases.iter().zip(&lines).enumerate() {
            let expected_host = if index == 0 { "node-a" } else { "" };
            let expected = json!({
                "Name": format!("metric.{index}"),
                "Host": expected_host,
                "Type": expected_type,
                "TaggerTags": ["origin:value"],
                "MetricTags": ["client:value", "bare"],
                "NoIndex": false,
                "Source": 1,
            });
            assert_eq!(serde_json::from_str::<serde_json::Value>(line).unwrap(), expected);
            assert_eq!(
                *line,
                format!(
                    "{{\"Name\":\"metric.{index}\",\"Host\":\"{expected_host}\",\"Type\":\"{expected_type}\",\"TaggerTags\":[\"origin:value\"],\"MetricTags\":[\"client:value\",\"bare\"],\"NoIndex\":false,\"Source\":1}}"
                )
            );
        }
    }

    #[test]
    fn plain_stream_round_trips_through_reader_and_report() {
        let snapshot = vec![
            snapshot_entry(
                "shared.metric",
                Some("node-a"),
                AggregateMetricType::Gauge,
                "",
                &["env:prod", "service:web"],
                &["pod_name:web-a"],
            ),
            snapshot_entry(
                "shared.metric",
                None,
                AggregateMetricType::Counter,
                "",
                &["env:staging", "service:web"],
                &["pod_name:web-b"],
            ),
        ];
        let artifact = tempfile::NamedTempFile::new().expect("temporary artifact should be created");
        let mut file = artifact.reopen().expect("temporary artifact should reopen");

        write_snapshot_records(&mut file, &snapshot, artifact.path()).expect("snapshot should serialize");
        file.flush().expect("plain stream should flush");

        assert_eq!(
            collect_records(artifact.path()).expect("plain stream should decode"),
            vec![
                AgentContextRecord {
                    name: "shared.metric".to_owned(),
                    host: "node-a".to_owned(),
                    metric_type: "Gauge".to_owned(),
                    tagger_tags: vec!["pod_name:web-a".to_owned()],
                    metric_tags: vec!["env:prod".to_owned(), "service:web".to_owned()],
                    no_index: false,
                    source: 1,
                },
                AgentContextRecord {
                    name: "shared.metric".to_owned(),
                    host: String::new(),
                    metric_type: "Counter".to_owned(),
                    tagger_tags: vec!["pod_name:web-b".to_owned()],
                    metric_tags: vec!["env:staging".to_owned(), "service:web".to_owned()],
                    no_index: false,
                    source: 1,
                },
            ]
        );
        assert_eq!(
            read_report(artifact.path())
                .expect("plain stream report should build")
                .render(10, 10),
            concat!(
                "   Contexts\tMetric name\t(number of unique values for each tag)\n",
                "          2\tshared.metric\t(2 env, 1 service)\n",
            )
        );
    }

    #[test]
    fn surfaces_a_partial_writer_failure_with_record_and_path_context() {
        let snapshot = [snapshot_entry(
            "write.failure",
            None,
            AggregateMetricType::Gauge,
            "",
            &["env:test"],
            &[],
        )];
        let path = PathBuf::from("/run/test/dogstatsd_contexts.json.zstd");
        let mut writer = FailAfterBytes::new(12);

        let error = write_snapshot_records(&mut writer, &snapshot, &path).expect_err("write failure should surface");

        assert_eq!(writer.written.len(), 12);
        assert!(error.to_string().contains("serialize DogStatsD context dump record 1"));
        assert!(error.to_string().contains(&path.display().to_string()));
        assert!(format!("{error:#}").contains("injected write failure"));
    }

    #[test]
    fn publishes_a_decodable_mode_0600_dump_and_replaces_the_canonical_artifact() {
        let run_directory = tempfile::tempdir().expect("temporary run directory should be created");
        let target = run_directory.path().join(CONTEXT_DUMP_FILENAME);
        fs::write(&target, b"old canonical artifact").expect("old canonical artifact should be written");
        let snapshot = vec![
            snapshot_entry(
                "published.metric",
                Some("node-a"),
                AggregateMetricType::Histogram,
                "millisecond",
                &["env:prod"],
                &["pod_name:web-a"],
            ),
            snapshot_entry(
                "published.metric",
                None,
                AggregateMetricType::Distribution,
                "",
                &["env:staging"],
                &["pod_name:web-b"],
            ),
        ];

        let published = publish_context_dump(run_directory.path(), &snapshot).expect("snapshot should publish");

        assert_eq!(published, target);
        assert_eq!(
            collect_records(&target)
                .expect("published artifact should decode")
                .len(),
            2
        );
        assert_eq!(
            read_report(&target)
                .expect("published report should build")
                .render(10, 10),
            concat!(
                "   Contexts\tMetric name\t(number of unique values for each tag)\n",
                "          2\tpublished.metric\t(2 env)\n",
            )
        );
        assert_eq!(
            directory_entries(run_directory.path()),
            vec![CONTEXT_DUMP_FILENAME.to_owned()]
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[cfg(unix)]
    #[test]
    fn publication_normalizes_restrictive_temporary_permissions_to_exactly_0600() {
        use std::os::unix::fs::PermissionsExt as _;

        let run_directory = tempfile::tempdir().expect("temporary run directory should be created");
        let normalizer = RestrictiveThenOwnerOnly::default();

        let target = publish_context_dump_with_services(
            run_directory.path(),
            &[],
            &FileSystemAtomicReplace,
            &FixedEntropy,
            &normalizer,
            &FileSystemCleanup,
        )
        .expect("snapshot should publish");

        assert!(normalizer.called.load(Ordering::Relaxed));
        assert_eq!(fs::metadata(target).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn entropy_failure_is_contextual_and_creates_no_temporary_artifact() {
        let run_directory = tempfile::tempdir().expect("temporary run directory should be created");
        let target = run_directory.path().join(CONTEXT_DUMP_FILENAME);
        let original = b"original canonical artifact";
        fs::write(&target, original).expect("canonical fixture should be written");

        let error = publish_context_dump_with_services(
            run_directory.path(),
            &[],
            &FileSystemAtomicReplace,
            &FailingEntropy,
            &OwnerOnlyPermissions,
            &FileSystemCleanup,
        )
        .expect_err("entropy failure should surface");

        let message = format!("{error:#}");
        assert!(message.contains("generate random temporary filename"), "{message}");
        assert!(message.contains(&target.display().to_string()), "{message}");
        assert!(message.contains("injected entropy failure"), "{message}");
        assert_eq!(fs::read(&target).unwrap(), original);
        assert_eq!(
            directory_entries(run_directory.path()),
            vec![CONTEXT_DUMP_FILENAME.to_owned()]
        );
    }

    #[test]
    fn temporary_name_sets_rfc_4122_version_and_variant_bits() {
        let run_path = std::path::Path::new("/run/test");
        let target = run_path.join(CONTEXT_DUMP_FILENAME);

        let temporary_path = temporary_context_dump_path(run_path, &target, &FixedEntropy).unwrap();

        assert_eq!(
            temporary_path.file_name().unwrap(),
            ".dogstatsd_contexts.json.zstd.11111111-1111-4111-9111-111111111111.tmp"
        );
    }

    #[test]
    fn permission_failure_closes_and_removes_the_new_temporary_file() {
        let run_directory = tempfile::tempdir().expect("temporary run directory should be created");
        let target = run_directory.path().join(CONTEXT_DUMP_FILENAME);
        let original = b"original canonical artifact";
        fs::write(&target, original).expect("canonical fixture should be written");

        let error = publish_context_dump_with_services(
            run_directory.path(),
            &[],
            &FileSystemAtomicReplace,
            &FixedEntropy,
            &FailingPermissions,
            &FileSystemCleanup,
        )
        .expect_err("permission failure should surface");

        let message = format!("{error:#}");
        assert!(message.contains("set owner-only permissions"), "{message}");
        assert!(message.contains(&target.display().to_string()), "{message}");
        assert!(message.contains("injected permission failure"), "{message}");
        assert_eq!(fs::read(&target).unwrap(), original);
        assert_eq!(
            directory_entries(run_directory.path()),
            vec![CONTEXT_DUMP_FILENAME.to_owned()]
        );
    }

    #[test]
    fn cleanup_method_surfaces_removal_failure_and_remains_armed() {
        let temporary_directory = tempfile::tempdir().expect("temporary directory should be created");
        let temporary_path = temporary_directory.path().join("staged-contexts.tmp");
        fs::write(&temporary_path, b"staged").unwrap();
        let cleanup = FailingCleanup::default();
        let mut guard = TemporaryPathCleanup::new(&temporary_path, &cleanup);

        let error = guard.cleanup().expect_err("cleanup failure should surface");

        assert!(error.to_string().contains("injected cleanup failure"));
        assert!(temporary_path.exists());
        assert_eq!(cleanup.attempts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn reports_primary_and_cleanup_failures_without_replacing_the_canonical_artifact() {
        let run_directory = tempfile::tempdir().expect("temporary run directory should be created");
        let target = run_directory.path().join(CONTEXT_DUMP_FILENAME);
        let original = b"original canonical artifact";
        fs::write(&target, original).expect("canonical fixture should be written");
        let cleanup = FailingCleanup::default();

        let error = publish_context_dump_with_services(
            run_directory.path(),
            &[],
            &FailingReplace,
            &FixedEntropy,
            &OwnerOnlyPermissions,
            &cleanup,
        )
        .expect_err("replacement and cleanup should fail");

        let message = format!("{error:#}");
        assert!(message.contains("atomically replace"), "{message}");
        assert!(message.contains(&target.display().to_string()), "{message}");
        assert!(message.contains("remove temporary DogStatsD context dump"), "{message}");
        assert!(message.contains("injected cleanup failure"), "{message}");
        assert_eq!(fs::read(&target).unwrap(), original);
        assert_eq!(directory_entries(run_directory.path()).len(), 2);
        assert!(directory_entries(run_directory.path())
            .iter()
            .any(|entry| entry.starts_with(&format!(".{CONTEXT_DUMP_FILENAME}.")) && entry.ends_with(".tmp")));
        assert!(cleanup.attempts.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn rejects_empty_missing_and_non_directory_run_paths_with_target_and_operation_context() {
        let temporary_directory = tempfile::tempdir().expect("temporary directory should be created");
        let missing = temporary_directory.path().join("missing");
        let not_directory = temporary_directory.path().join("not-a-directory");
        fs::write(&not_directory, b"file").expect("non-directory fixture should be written");
        let cases = [
            (PathBuf::new(), "validate run path"),
            (missing, "create temporary"),
            (not_directory, "create temporary"),
        ];

        for (run_path, expected_operation) in cases {
            let error = publish_context_dump(&run_path, &[]).expect_err("invalid run path should fail");
            let message = format!("{error:#}");
            assert!(message.contains(CONTEXT_DUMP_FILENAME), "{message}");
            assert!(message.contains(expected_operation), "{message}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_an_unwritable_run_directory_with_target_and_operation_context() {
        use std::os::unix::fs::PermissionsExt as _;

        let run_directory = tempfile::tempdir().expect("temporary run directory should be created");
        let original_permissions = fs::metadata(run_directory.path()).unwrap().permissions();
        fs::set_permissions(run_directory.path(), fs::Permissions::from_mode(0o500)).unwrap();

        let result = publish_context_dump(run_directory.path(), &[]);
        fs::set_permissions(run_directory.path(), original_permissions).unwrap();

        let error = result.expect_err("unwritable run path should fail");
        let message = format!("{error:#}");
        assert!(message.contains(CONTEXT_DUMP_FILENAME), "{message}");
        assert!(message.contains("create temporary"), "{message}");
        assert_eq!(directory_entries(run_directory.path()), Vec::<String>::new());
    }

    #[test]
    fn injected_replace_failure_preserves_the_canonical_artifact_and_removes_the_temporary_file() {
        let run_directory = tempfile::tempdir().expect("temporary run directory should be created");
        let target = run_directory.path().join(CONTEXT_DUMP_FILENAME);
        let original = b"original canonical artifact";
        fs::write(&target, original).expect("canonical fixture should be written");
        let snapshot = [snapshot_entry(
            "replacement.failure",
            None,
            AggregateMetricType::Gauge,
            "",
            &[],
            &[],
        )];

        let error = publish_context_dump_with(run_directory.path(), &snapshot, &FailingReplace)
            .expect_err("replacement should fail");

        let message = format!("{error:#}");
        assert!(message.contains("atomically replace"), "{message}");
        assert!(message.contains(&target.display().to_string()), "{message}");
        assert_eq!(fs::read(&target).unwrap(), original);
        assert_eq!(
            directory_entries(run_directory.path()),
            vec![CONTEXT_DUMP_FILENAME.to_owned()]
        );
    }

    #[test]
    fn zstd_finalization_failure_preserves_the_canonical_artifact_and_removes_the_temporary_file() {
        let run_directory = tempfile::tempdir().expect("temporary run directory should be created");
        let target = run_directory.path().join(CONTEXT_DUMP_FILENAME);
        let temporary = run_directory.path().join(".injected-zstd-failure.tmp");
        let original = b"original canonical artifact";
        fs::write(&target, original).expect("canonical fixture should be written");
        let file = fs::File::create(&temporary).expect("temporary fixture should be created");
        let writer = InjectableFile {
            file,
            fail_writes: true,
            fail_sync: false,
        };
        let buffer = BufWriter::with_capacity(0, writer);
        let snapshot = [snapshot_entry(
            "zstd.failure",
            None,
            AggregateMetricType::Gauge,
            "",
            &[],
            &[],
        )];

        let error = publish_buffered_temporary(buffer, &temporary, &target, &snapshot, &FailingReplace)
            .expect_err("zstd finalization should fail");

        let message = format!("{error:#}");
        assert!(message.contains("finalize zstd stream"), "{message}");
        assert!(message.contains(&target.display().to_string()), "{message}");
        assert!(message.contains("injected write failure"), "{message}");
        assert_eq!(fs::read(&target).unwrap(), original);
        assert_eq!(
            directory_entries(run_directory.path()),
            vec![CONTEXT_DUMP_FILENAME.to_owned()]
        );
    }

    #[test]
    fn buffered_finalization_failure_preserves_the_canonical_artifact_and_removes_the_temporary_file() {
        let run_directory = tempfile::tempdir().expect("temporary run directory should be created");
        let target = run_directory.path().join(CONTEXT_DUMP_FILENAME);
        let temporary = run_directory.path().join(".injected-buffer-failure.tmp");
        let original = b"original canonical artifact";
        fs::write(&target, original).expect("canonical fixture should be written");
        let file = fs::File::create(&temporary).expect("temporary fixture should be created");
        let writer = InjectableFile {
            file,
            fail_writes: true,
            fail_sync: false,
        };
        let snapshot = [snapshot_entry(
            "buffer.failure",
            None,
            AggregateMetricType::Gauge,
            "",
            &[],
            &[],
        )];

        let error = publish_open_temporary(writer, &temporary, &target, &snapshot, &FailingReplace)
            .expect_err("buffer finalization should fail");

        let message = format!("{error:#}");
        assert!(message.contains("finalize buffered"), "{message}");
        assert!(message.contains(&target.display().to_string()), "{message}");
        assert!(message.contains("injected write failure"), "{message}");
        assert_eq!(fs::read(&target).unwrap(), original);
        assert_eq!(
            directory_entries(run_directory.path()),
            vec![CONTEXT_DUMP_FILENAME.to_owned()]
        );
    }

    #[test]
    fn sync_failure_preserves_the_canonical_artifact_and_removes_the_temporary_file() {
        let run_directory = tempfile::tempdir().expect("temporary run directory should be created");
        let target = run_directory.path().join(CONTEXT_DUMP_FILENAME);
        let temporary = run_directory.path().join(".injected-sync-failure.tmp");
        let original = b"original canonical artifact";
        fs::write(&target, original).expect("canonical fixture should be written");
        let file = fs::File::create(&temporary).expect("temporary fixture should be created");
        let writer = InjectableFile {
            file,
            fail_writes: false,
            fail_sync: true,
        };

        let error =
            publish_open_temporary(writer, &temporary, &target, &[], &FailingReplace).expect_err("sync should fail");

        let message = format!("{error:#}");
        assert!(message.contains("sync temporary"), "{message}");
        assert!(message.contains(&target.display().to_string()), "{message}");
        assert!(message.contains("injected sync failure"), "{message}");
        assert_eq!(fs::read(&target).unwrap(), original);
        assert_eq!(
            directory_entries(run_directory.path()),
            vec![CONTEXT_DUMP_FILENAME.to_owned()]
        );
    }

    #[test]
    fn zstd_and_buffer_finalizers_surface_their_own_error_context() {
        let target = PathBuf::from("/run/test/dogstatsd_contexts.json.zstd");
        let snapshot = [snapshot_entry(
            "finalize.failure",
            None,
            AggregateMetricType::Gauge,
            "",
            &[],
            &[],
        )];

        let zstd_failure = Arc::new(AtomicBool::new(false));
        let mut encoder = zstd::stream::write::Encoder::new(ToggleFailureWriter::new(zstd_failure.clone()), 0)
            .expect("encoder should initialize");
        write_snapshot_records(&mut encoder, &snapshot, &target).expect("records should stream before failure");
        zstd_failure.store(true, Ordering::Relaxed);
        let error = finish_zstd_encoder(encoder, &target).expect_err("zstd finalization should fail");
        let message = format!("{error:#}");
        assert!(message.contains("finalize zstd stream"), "{message}");
        assert!(message.contains(&target.display().to_string()), "{message}");
        assert!(message.contains("injected finalization failure"), "{message}");

        let buffer_failure = Arc::new(AtomicBool::new(false));
        let buffer = BufWriter::new(ToggleFailureWriter::new(buffer_failure.clone()));
        let mut encoder = zstd::stream::write::Encoder::new(buffer, 0).expect("encoder should initialize");
        write_snapshot_records(&mut encoder, &snapshot, &target).expect("records should stream before failure");
        let buffer = finish_zstd_encoder(encoder, &target).expect("zstd stream should finalize");
        buffer_failure.store(true, Ordering::Relaxed);
        let error = finish_buffered_writer(buffer, &target).expect_err("buffer finalization should fail");
        let message = format!("{error:#}");
        assert!(message.contains("finalize buffered"), "{message}");
        assert!(message.contains(&target.display().to_string()), "{message}");
        assert!(message.contains("injected finalization failure"), "{message}");
    }

    struct InjectableFile {
        file: fs::File,
        fail_writes: bool,
        fail_sync: bool,
    }

    impl io::Write for InjectableFile {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.fail_writes {
                Err(io::Error::other("injected write failure"))
            } else {
                self.file.write(bytes)
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.fail_writes {
                Err(io::Error::other("injected write failure"))
            } else {
                self.file.flush()
            }
        }
    }

    impl SyncAll for InjectableFile {
        fn sync_all(&self) -> io::Result<()> {
            if self.fail_sync {
                Err(io::Error::other("injected sync failure"))
            } else {
                self.file.sync_all()
            }
        }
    }

    #[derive(Debug)]
    struct ToggleFailureWriter {
        failure_enabled: Arc<AtomicBool>,
    }

    impl ToggleFailureWriter {
        fn new(failure_enabled: Arc<AtomicBool>) -> Self {
            Self { failure_enabled }
        }
    }

    impl io::Write for ToggleFailureWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.failure_enabled.load(Ordering::Relaxed) {
                Err(io::Error::other("injected finalization failure"))
            } else {
                Ok(bytes.len())
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.failure_enabled.load(Ordering::Relaxed) {
                Err(io::Error::other("injected finalization failure"))
            } else {
                Ok(())
            }
        }
    }

    struct FixedEntropy;

    impl EntropySource for FixedEntropy {
        fn fill(&self, bytes: &mut [u8]) -> io::Result<()> {
            bytes.fill(0x11);
            Ok(())
        }
    }

    struct FailingEntropy;

    impl EntropySource for FailingEntropy {
        fn fill(&self, _bytes: &mut [u8]) -> io::Result<()> {
            Err(io::Error::other("injected entropy failure"))
        }
    }

    struct FailingPermissions;

    impl PermissionNormalizer for FailingPermissions {
        fn normalize(&self, _file: &fs::File) -> io::Result<()> {
            Err(io::Error::other("injected permission failure"))
        }
    }

    #[cfg(unix)]
    #[derive(Default)]
    struct RestrictiveThenOwnerOnly {
        called: AtomicBool,
    }

    #[cfg(unix)]
    impl PermissionNormalizer for RestrictiveThenOwnerOnly {
        fn normalize(&self, file: &fs::File) -> io::Result<()> {
            use std::os::unix::fs::PermissionsExt as _;

            file.set_permissions(fs::Permissions::from_mode(0o000))?;
            set_owner_only_permissions(file)?;
            self.called.store(true, Ordering::Relaxed);
            Ok(())
        }
    }

    #[derive(Default)]
    struct FailingCleanup {
        attempts: AtomicUsize,
    }

    impl TemporaryFileCleanup for FailingCleanup {
        fn remove(&self, _path: &std::path::Path) -> io::Result<()> {
            self.attempts.fetch_add(1, Ordering::Relaxed);
            Err(io::Error::other("injected cleanup failure"))
        }
    }

    struct FailingReplace;

    impl AtomicReplace for FailingReplace {
        fn replace(&self, _source: &std::path::Path, _target: &std::path::Path) -> io::Result<()> {
            Err(io::Error::other("injected replacement failure"))
        }
    }

    fn directory_entries(path: &std::path::Path) -> Vec<String> {
        let mut entries: Vec<_> = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort_unstable();
        entries
    }

    struct FailAfterBytes {
        remaining: usize,
        written: Vec<u8>,
    }

    impl FailAfterBytes {
        fn new(remaining: usize) -> Self {
            Self {
                remaining,
                written: Vec::new(),
            }
        }
    }

    impl io::Write for FailAfterBytes {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::other("injected write failure"));
            }
            let written = self.remaining.min(bytes.len());
            self.written.extend_from_slice(&bytes[..written]);
            self.remaining -= written;
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn snapshot_entry(
        name: &'static str, host: Option<&'static str>, metric_type: AggregateMetricType, unit: &'static str,
        metric_tags: &[&'static str], origin_tags: &[&'static str],
    ) -> AggregateContextSnapshotEntry {
        let context = Context::from_static_parts(name, metric_tags)
            .with_host(host.map(MetaString::from_static))
            .with_origin_tags(origin_tags.iter().map(|tag| Tag::from_static(tag)).collect::<TagSet>());
        AggregateContextSnapshotEntry::for_test(context, metric_type, MetaString::from_static(unit))
    }

    #[test]
    fn detects_plain_and_compressed_artifacts_by_magic_bytes() {
        let temporary_directory = tempfile::tempdir().expect("temporary directory should be created");
        let compressed_without_suffix = temporary_directory.path().join("compressed-contexts");
        let plain_with_compressed_suffix = temporary_directory.path().join("plain-contexts.zstd");
        fs::write(&compressed_without_suffix, COMPRESSED_FIXTURE_BYTES).expect("compressed fixture should be copied");
        fs::write(&plain_with_compressed_suffix, PLAIN_FIXTURE_BYTES).expect("plain fixture should be copied");

        let cases = [
            PathBuf::from(PLAIN_FIXTURE_PATH),
            PathBuf::from(COMPRESSED_FIXTURE_PATH),
            compressed_without_suffix,
            plain_with_compressed_suffix,
        ];
        let expected = expected_fixture_records();

        for path in cases {
            assert_eq!(
                collect_records(&path).expect("artifact should decode"),
                expected,
                "{path:?}"
            );
        }
    }

    #[test]
    fn checked_in_compressed_fixture_expands_to_the_plain_fixture_exactly() {
        let decompressed = zstd::stream::decode_all(COMPRESSED_FIXTURE_BYTES)
            .expect("checked-in compressed fixture should decompress");

        assert_eq!(decompressed, PLAIN_FIXTURE_BYTES);
        assert_eq!(PLAIN_FIXTURE_BYTES.last(), Some(&b'\n'));
    }

    #[test]
    fn rejects_invalid_artifacts_with_path_operation_and_record_index() {
        let temporary_directory = tempfile::tempdir().expect("temporary directory should be created");
        let valid_record = concat!(
            "{\"Name\":\"metric\",\"Host\":\"host\",\"Type\":\"Gauge\",",
            "\"TaggerTags\":[],\"MetricTags\":[],\"NoIndex\":false,\"Source\":1}"
        );
        let truncated_length = COMPRESSED_FIXTURE_BYTES.len() - 4;
        let cases = [
            InvalidArtifactCase {
                filename: "array.ndjson",
                bytes: format!("[{valid_record}]").into_bytes(),
                expected_record_index: 1,
            },
            InvalidArtifactCase {
                filename: "missing-field.ndjson",
                bytes: br#"{"Name":"metric"}"#.to_vec(),
                expected_record_index: 1,
            },
            InvalidArtifactCase {
                filename: "malformed-trailing.ndjson",
                bytes: format!("{valid_record}\nnot-json").into_bytes(),
                expected_record_index: 2,
            },
            InvalidArtifactCase {
                filename: "truncated.zstd",
                bytes: COMPRESSED_FIXTURE_BYTES[..truncated_length].to_vec(),
                expected_record_index: 5,
            },
        ];

        for case in cases {
            let path = temporary_directory.path().join(case.filename);
            fs::write(&path, case.bytes).expect("invalid artifact should be written");

            let error = collect_records(&path).expect_err("invalid artifact should be rejected");

            assert_eq!(error.path(), path.as_path(), "{}", case.filename);
            assert_eq!(error.operation(), "decode", "{}", case.filename);
            assert_eq!(error.record_index(), case.expected_record_index, "{}", case.filename);
            assert!(error.to_string().contains(&path.display().to_string()), "{error}");
            assert!(
                error
                    .to_string()
                    .contains(&format!("decode record {}", case.expected_record_index)),
                "{error}"
            );
            assert!(error.to_string().contains("JSON"), "{error}");
            assert!(error.to_string().contains("line"), "{error}");
            assert!(error.to_string().contains("column"), "{error}");
        }
    }

    struct InvalidArtifactCase {
        filename: &'static str,
        bytes: Vec<u8>,
        expected_record_index: usize,
    }

    #[test]
    fn ignores_unknown_fields_but_requires_every_agent_field() {
        let artifact = tempfile::NamedTempFile::new().expect("temporary artifact should be created");
        let record = concat!(
            "{\"Name\":\"metric\",\"Host\":\"host\",\"Type\":\"Gauge\",",
            "\"TaggerTags\":[],\"MetricTags\":[\"env:prod\"],\"NoIndex\":false,\"Source\":7,",
            "\"FutureField\":\"ignored\"}\n"
        );
        fs::write(artifact.path(), record).expect("artifact should be written");

        assert_eq!(
            collect_records(artifact.path()).expect("record with unknown field should decode"),
            vec![AgentContextRecord {
                name: "metric".to_owned(),
                host: "host".to_owned(),
                metric_type: "Gauge".to_owned(),
                tagger_tags: Vec::new(),
                metric_tags: vec!["env:prod".to_owned()],
                no_index: false,
                source: 7,
            }]
        );
    }
}
