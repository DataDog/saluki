use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
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
#[cfg(windows)]
// A protected DACL granting full control through Owner Rights, LocalSystem, and Builtin Administrators only.
const WINDOWS_CONTEXT_DUMP_SDDL: &str = "D:P(A;;FA;;;OW)(A;;FA;;;SY)(A;;FA;;;BA)";

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
    publish_context_dump_with(run_path, snapshot, &FileSystemAtomicReplace)
}

fn publish_context_dump_with<R: AtomicReplace>(
    run_path: &Path, snapshot: &[AggregateContextSnapshotEntry], replacer: &R,
) -> Result<PathBuf, GenericError> {
    publish_context_dump_with_services(
        run_path,
        snapshot,
        replacer,
        &SecureTemporaryFileFactory,
        &OwnerOnlyPermissions,
        &FileSystemCleanup,
    )
}

fn publish_context_dump_with_services<R, F, P, C>(
    run_path: &Path, snapshot: &[AggregateContextSnapshotEntry], replacer: &R, temporary_files: &F, _permissions: &P,
    cleanup: &C,
) -> Result<PathBuf, GenericError>
where
    R: AtomicReplace,
    F: TemporaryFileFactory,
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

    let (file, temporary_path) = temporary_files.create(run_path).with_error_context(|| {
        format!(
            "Failed to create temporary DogStatsD context dump for target '{}'.",
            target.display()
        )
    })?;

    run_with_temporary_cleanup(&temporary_path, &target, cleanup, || {
        #[cfg(unix)]
        _permissions.normalize(&file).with_error_context(|| {
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

trait TemporaryFileFactory {
    fn create(&self, run_path: &Path) -> io::Result<(File, PathBuf)>;
}

struct SecureTemporaryFileFactory;

impl TemporaryFileFactory for SecureTemporaryFileFactory {
    fn create(&self, run_path: &Path) -> io::Result<(File, PathBuf)> {
        let prefix = format!(".{CONTEXT_DUMP_FILENAME}.");
        let mut builder = TempFileBuilder::new();
        builder.prefix(&prefix).suffix(".tmp");

        #[cfg(windows)]
        let temporary = builder.make_in(run_path, open_temporary_file)?;
        #[cfg(not(windows))]
        let temporary = builder.tempfile_in(run_path)?;

        temporary.keep().map_err(|error| error.error)
    }
}

trait PermissionNormalizer {
    #[cfg(unix)]
    fn normalize(&self, file: &File) -> io::Result<()>;
}

struct OwnerOnlyPermissions;

impl PermissionNormalizer for OwnerOnlyPermissions {
    #[cfg(unix)]
    fn normalize(&self, file: &File) -> io::Result<()> {
        set_owner_only_permissions(file)
    }
}

#[cfg(unix)]
fn set_owner_only_permissions(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(windows)]
fn open_temporary_file(path: &Path) -> io::Result<File> {
    use std::os::windows::{
        ffi::OsStrExt as _,
        io::{FromRawHandle as _, OwnedHandle},
    };
    use std::ptr;

    use windows_sys::Win32::{
        Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        },
    };

    let mut wide_path: Vec<_> = path.as_os_str().encode_wide().collect();
    if wide_path.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains an interior NUL",
        ));
    }
    wide_path.push(0);

    let security_attributes = WindowsSecurityAttributes::from_sddl(WINDOWS_CONTEXT_DUMP_SDDL)?;
    // SAFETY: The path is a live NUL-terminated UTF-16 buffer, the security attributes refer to a live self-relative
    // descriptor, and the null template handle is valid for a new file. A successful call transfers one owned handle.
    let handle = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            security_attributes.as_ptr(),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: `CreateFileW` returned a valid handle whose ownership has not been transferred elsewhere.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    Ok(File::from(handle))
}

#[cfg(windows)]
struct WindowsSecurityAttributes {
    descriptor: *mut std::ffi::c_void,
    attributes: windows_sys::Win32::Security::SECURITY_ATTRIBUTES,
}

#[cfg(windows)]
impl WindowsSecurityAttributes {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        use std::ptr;

        use windows_sys::Win32::{
            Foundation::FALSE,
            Security::{
                Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
                SECURITY_ATTRIBUTES,
            },
        };

        let wide_sddl: Vec<_> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut descriptor = ptr::null_mut();
        // SAFETY: The SDDL is a live NUL-terminated UTF-16 buffer, `descriptor` is a valid output pointer, and the
        // optional descriptor-size output is null. A successful call transfers ownership of the descriptor.
        let result = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide_sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if result == FALSE {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            descriptor,
            attributes: SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: FALSE,
            },
        })
    }

    fn as_ptr(&self) -> *const windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        &self.attributes
    }
}

#[cfg(windows)]
impl Drop for WindowsSecurityAttributes {
    fn drop(&mut self) {
        if !self.descriptor.is_null() {
            use windows_sys::Win32::Foundation::{LocalFree, HLOCAL};

            // SAFETY: The descriptor was allocated by the SDDL conversion API and remains owned by this value.
            unsafe {
                let _ = LocalFree(self.descriptor as HLOCAL);
            }
        }
    }
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
