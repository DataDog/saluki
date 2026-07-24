use std::error::Error;
use std::fmt;
#[cfg(unix)]
use std::fs;
use std::fs::File;
#[cfg(unix)]
use std::io;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use saluki_components::transforms::{AggregateContextSnapshotEntry, AggregateMetricType};
use saluki_context::tags::TagSet;
use saluki_error::{ErrorContext as _, GenericError};
use serde::{ser::SerializeStruct as _, Deserialize, Serialize, Serializer};
use tempfile::Builder as TempFileBuilder;

// Zstandard frame magic from RFC 8878, section 3.1.1.
const ZSTD_MAGIC: &[u8; 4] = b"\x28\xb5\x2f\xfd";
// Matches the fixed Agent output path in demultiplexerendpoint/impl/endpoint.go at the pinned compatibility revision.
pub(crate) const CONTEXT_DUMP_FILENAME: &str = "dogstatsd_contexts.json.zstd";
// `MetricSourceDogstatsd` is the second value after `MetricSourceUnknown` in the Agent's metricsource.go enum.
const AGENT_METRIC_SOURCE_DOGSTATSD: u32 = 1;

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
        record.serialize_field("Source", &AGENT_METRIC_SOURCE_DOGSTATSD)?;
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

// These spellings match `MetricType.String()` in the Agent's pkg/metrics/metric_sample.go.
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
    let target = run_path.join(CONTEXT_DUMP_FILENAME);
    if run_path.as_os_str().is_empty() {
        return Err(saluki_error::generic_error!(
            "Failed to validate run path for DogStatsD context dump target '{}': the run path is empty.",
            target.display()
        ));
    }

    let prefix = format!(".{CONTEXT_DUMP_FILENAME}.");
    let mut temporary = TempFileBuilder::new()
        .prefix(&prefix)
        .suffix(".tmp")
        .tempfile_in(run_path)
        .with_error_context(|| {
            format!(
                "Failed to create temporary DogStatsD context dump for target '{}'.",
                target.display()
            )
        })?;

    #[cfg(unix)]
    set_owner_only_permissions(temporary.as_file()).with_error_context(|| {
        format!(
            "Failed to set owner-only permissions on temporary DogStatsD context dump '{}' for target '{}'.",
            temporary.path().display(),
            target.display()
        )
    })?;

    let file = write_compressed_snapshot(BufWriter::new(temporary.as_file_mut()), snapshot, &target)?;
    file.sync_all().with_error_context(|| {
        format!(
            "Failed to sync temporary DogStatsD context dump for target '{}'.",
            target.display()
        )
    })?;

    temporary
        .persist(&target)
        .map_err(|error| error.error)
        .with_error_context(|| {
            format!(
                "Failed to atomically replace DogStatsD context dump target '{}'.",
                target.display()
            )
        })?;
    Ok(target)
}

#[cfg(unix)]
fn set_owner_only_permissions(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    file.set_permissions(fs::Permissions::from_mode(0o600))
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

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct AgentContextRecord {
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
pub(crate) struct ArtifactError {
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

pub(crate) fn for_each_record(path: &Path, mut consume: impl FnMut(AgentContextRecord)) -> Result<(), ArtifactError> {
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

#[cfg(test)]
#[path = "artifact_tests.rs"]
mod tests;
