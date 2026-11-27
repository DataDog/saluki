use std::{
    io,
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::Arc,
};

use fs4::{available_space, total_space};
use rand::RngExt as _;
use saluki_common::time::{parse_compact_utc_nanos, Timestamp, TimestampFormat};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::de::DeserializeOwned;
use tracing::{debug, error, info, warn};

use super::{PushResult, Retryable};

/// A persisted entry.
///
/// Represents the high-level metadata of a persisted entry, including the path to and size of the entry.
struct PersistedEntry {
    path: PathBuf,
    timestamp: u128,
    size_bytes: u64,
}

impl PersistedEntry {
    /// Attempts to create a `PersistedEntry` from the given path.
    ///
    /// If the given path isn't recognized as the path to a valid persisted entry, `None` is returned.
    fn try_from_path(path: PathBuf, size_bytes: u64) -> Option<Self> {
        let timestamp = decode_timestamped_filename(&path)?;
        Some(Self {
            path,
            timestamp,
            size_bytes,
        })
    }

    fn from_parts(path: PathBuf, timestamp: u128, size_bytes: u64) -> Self {
        Self {
            path,
            timestamp,
            size_bytes,
        }
    }
}

pub trait DiskUsageRetriever {
    fn total_space(&self) -> Result<u64, GenericError>;
    fn available_space(&self) -> Result<u64, GenericError>;
}

pub struct DiskUsageRetrieverImpl {
    root_path: PathBuf,
}

impl DiskUsageRetrieverImpl {
    pub fn new(root_path: PathBuf) -> Self {
        Self { root_path }
    }
}

impl DiskUsageRetriever for DiskUsageRetrieverImpl {
    fn total_space(&self) -> Result<u64, GenericError> {
        total_space(&self.root_path)
            .with_error_context(|| format!("Failed to get total space for '{}'.", self.root_path.display()))
    }

    fn available_space(&self) -> Result<u64, GenericError> {
        available_space(&self.root_path)
            .with_error_context(|| format!("Failed to get available space for '{}'.", self.root_path.display()))
    }
}

#[derive(Clone)]
pub struct DiskUsageRetrieverWrapper {
    inner: Arc<dyn DiskUsageRetriever + Send + Sync>,
}

impl DiskUsageRetrieverWrapper {
    pub fn new(disk_usage_retriever: Arc<dyn DiskUsageRetriever + Send + Sync>) -> Self {
        Self {
            inner: disk_usage_retriever,
        }
    }
}

/// Arguments for constructing a persisted retry queue.
pub struct PersistedQueueArgs {
    /// Root path under which the queue directory is created.
    pub root_path: PathBuf,
    /// Maximum total bytes the queue may occupy on disk.
    pub max_on_disk_bytes: u64,
    /// Maximum fraction of the disk that may be used before writes stop.
    pub storage_max_disk_ratio: f64,
    /// Provider for total- and available-disk-space queries.
    pub disk_usage_retriever: Arc<dyn DiskUsageRetriever + Send + Sync>,
    /// Maximum age of retry files in days; files older than this are removed on startup.
    ///
    /// Setting this to `0` removes all retry files (cutoff = now), matching the behavior of the
    /// core Agent's `FileRemovalPolicy` with `outdatedFileDayCount = 0`.
    pub max_age_days: u32,
}

/// Point-in-time statistics about the entries a persisted queue holds on disk.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PersistedQueueStats {
    /// Number of entries (files) on disk.
    pub files: usize,

    /// Total size of the entries on disk, in bytes.
    pub size_bytes: u64,
}

/// Entries a persisted queue removed from disk to stay within its on-disk size limit.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PersistedQueueEvictions {
    /// Number of entries (files) removed.
    pub files_removed: u64,

    /// Number of metric data points represented by the removed entries.
    pub data_points_dropped: u64,
}

/// Which setting imposes the on-disk size limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OnDiskLimitReason {
    /// The configured maximum on-disk size of the queue.
    MaxSize,

    /// The configured maximum disk usage ratio of the underlying volume.
    MaxDiskRatio,
}

impl OnDiskLimitReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::MaxSize => "max_size",
            Self::MaxDiskRatio => "max_disk_ratio",
        }
    }
}

/// The on-disk size limit in force.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OnDiskLimit {
    bytes: u64,
    reason: OnDiskLimitReason,
}

pub struct PersistedQueue<T> {
    root_path: PathBuf,
    entries: Vec<PersistedEntry>,
    total_on_disk_bytes: u64,
    max_on_disk_bytes: u64,
    storage_max_disk_ratio: f64,
    disk_usage_retriever: DiskUsageRetrieverWrapper,
    max_age_days: u32,
    entries_dropped: u64,
    evictions: PersistedQueueEvictions,
    _entry: PhantomData<T>,
}

impl<T> PersistedQueue<T>
where
    T: Retryable,
{
    /// Creates a new `PersistedQueue` instance from the given arguments.
    ///
    /// The root path is created if it doesn't already exist, and is scanned for existing persisted entries. Entries
    /// are removed (oldest first) until the total size of all scanned entries is within the given maximum size.
    ///
    /// To remove stale retry files on startup, call [`remove_stale_files`][Self::remove_stale_files] after construction.
    ///
    /// # Errors
    ///
    /// If there is an error creating the root directory, or scanning it for existing entries, or deleting entries to
    /// shrink the directory to fit the given maximum size, an error is returned.
    pub async fn from_root_path(args: PersistedQueueArgs) -> Result<Self, GenericError> {
        let PersistedQueueArgs {
            root_path,
            max_on_disk_bytes,
            storage_max_disk_ratio,
            disk_usage_retriever,
            max_age_days,
        } = args;

        // Make sure the directory exists first.
        create_directory_recursive(root_path.clone())
            .await
            .with_error_context(|| format!("Failed to create retry directory '{}'.", root_path.display()))?;

        let mut persisted_requests = Self {
            root_path: root_path.clone(),
            entries: Vec::new(),
            total_on_disk_bytes: 0,
            max_on_disk_bytes,
            storage_max_disk_ratio,
            disk_usage_retriever: DiskUsageRetrieverWrapper::new(disk_usage_retriever),
            max_age_days,
            entries_dropped: 0,
            evictions: PersistedQueueEvictions::default(),
            _entry: PhantomData,
        };

        persisted_requests.refresh_entry_state().await?;

        info!(
            "Persisted retry queue initialized. Transactions will be stored in '{}'.",
            root_path.display()
        );

        Ok(persisted_requests)
    }

    /// Returns `true` if the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the number of entries in the queue.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns statistics about the entries currently on disk.
    pub fn stats(&self) -> PersistedQueueStats {
        PersistedQueueStats {
            files: self.entries.len(),
            size_bytes: self.total_on_disk_bytes,
        }
    }

    /// Returns the available on-disk capacity, in bytes.
    ///
    /// This reflects the lower of the configured queue limit and disk-usage-ratio limit, minus the bytes currently used
    /// by persisted entries.
    ///
    /// # Errors
    ///
    /// If there is an error while retrieving the total or available space of the underlying volume, an error is returned.
    pub async fn available_capacity_bytes(&self) -> Result<u64, GenericError> {
        let disk_usage_retriever = self.disk_usage_retriever.clone();
        let storage_max_disk_ratio = self.storage_max_disk_ratio;
        let max_on_disk_bytes = self.max_on_disk_bytes;
        let total_on_disk_bytes = self.total_on_disk_bytes;

        let limit = tokio::task::spawn_blocking(move || {
            on_disk_bytes_limit(
                disk_usage_retriever,
                storage_max_disk_ratio,
                max_on_disk_bytes,
                total_on_disk_bytes,
            )
        })
        .await
        .error_context("Failed to run disk size limit check to completion.")??;

        Ok(limit.bytes.saturating_sub(total_on_disk_bytes))
    }

    /// Returns the number of entries that have been permanently dropped due to errors since the last call to this
    /// method, resetting the counter.
    pub fn take_entries_dropped(&mut self) -> u64 {
        std::mem::take(&mut self.entries_dropped)
    }

    /// Returns the entries removed from disk to stay within the on-disk size limit since the last call to this
    /// method, resetting the counts.
    ///
    /// An entry that couldn't be consumed while being evicted is counted here, with no data points, and also by
    /// [`take_entries_dropped`][Self::take_entries_dropped].
    pub fn take_evictions(&mut self) -> PersistedQueueEvictions {
        std::mem::take(&mut self.evictions)
    }

    /// Removes retry files older than `max_age_days` (from [`PersistedQueueArgs`]) from the queue directory and
    /// reloads entry state.
    ///
    /// # Errors
    ///
    /// Returns an error if the queue directory cannot be opened or scanned. Individual file removal failures are
    /// logged as warnings and do not stop the cleanup.
    pub async fn remove_stale_files(&mut self) -> Result<u32, GenericError> {
        let removed = remove_outdated_retry_files(&self.root_path, self.max_age_days).await?;
        self.refresh_entry_state().await.map_err(GenericError::from)?;
        Ok(removed)
    }

    /// Enqueues an entry and persists it to disk.
    ///
    /// # Errors
    ///
    /// If there is an error serializing the entry, or writing it to disk, or removing older entries to make space for
    /// the new entry, an error is returned.
    pub async fn push(&mut self, entry: T) -> Result<PushResult, GenericError> {
        // Serialize the entry to a temporary file.
        let (filename, timestamp) = generate_timestamped_filename()?;
        let entry_path = self.root_path.join(filename);
        let serialized = serde_json::to_vec(&entry)
            .with_error_context(|| format!("Failed to serialize entry for '{}'.", entry_path.display()))?;

        if serialized.len() as u64 > self.max_on_disk_bytes {
            return Err(generic_error!("Entry is too large to persist."));
        }

        // Make sure we have enough space to persist the entry.
        let push_result = self
            .remove_until_available_space(serialized.len() as u64)
            .await
            .error_context(
                "Failed to remove older persisted entries to make space for the incoming persisted entry.",
            )?;

        // Actually persist it.
        tokio::fs::write(&entry_path, &serialized)
            .await
            .with_error_context(|| format!("Failed to write entry to '{}'.", entry_path.display()))?;

        // Add a new persisted entry to our state.
        self.entries.push(PersistedEntry::from_parts(
            entry_path,
            timestamp,
            serialized.len() as u64,
        ));
        self.total_on_disk_bytes += serialized.len() as u64;

        debug!(entry.len = serialized.len(), "Enqueued persisted entry.");

        Ok(push_result)
    }

    /// Consumes the oldest persisted entry on disk, if one exists.
    ///
    /// # Errors
    ///
    /// If there is an error reading or deserializing the entry, an error is returned.
    pub async fn pop(&mut self) -> Result<Option<T>, GenericError> {
        loop {
            if self.entries.is_empty() {
                return Ok(None);
            }

            let entry = self.entries.remove(0);
            match try_deserialize_entry(&entry).await {
                Ok(Some(deserialized)) => {
                    // We got the deserialized entry, so remove it from our state and return it.
                    self.total_on_disk_bytes -= entry.size_bytes;
                    debug!(entry.len = entry.size_bytes, "Dequeued persisted entry.");

                    return Ok(Some(deserialized));
                }
                Ok(None) => {
                    // We couldn't read the entry from disk, which points to us potentially having invalid state about
                    // what entries _are_ on disk, so we'll refresh our entry state and try again.
                    self.refresh_entry_state().await?;
                    continue;
                }
                Err(e) => {
                    // The entry is corrupt or unreadable. Drop it permanently to avoid a poison pill scenario
                    // where the same entry is retried indefinitely, blocking all other work.
                    warn!(
                        entry.path = %entry.path.display(),
                        entry.len = entry.size_bytes,
                        error = %e,
                        "Permanently dropping persisted entry that could not be consumed.",
                    );

                    self.total_on_disk_bytes -= entry.size_bytes;
                    self.entries_dropped += 1;

                    continue;
                }
            }
        }
    }

    async fn refresh_entry_state(&mut self) -> io::Result<()> {
        // Scan the root path for persisted entries.
        let mut entries = Vec::new();

        let mut dir_reader = tokio::fs::read_dir(&self.root_path).await?;
        while let Some(entry) = dir_reader.next_entry().await? {
            let metadata = entry.metadata().await?;
            if metadata.is_file() {
                match PersistedEntry::try_from_path(entry.path(), metadata.len()) {
                    Some(entry) => entries.push(entry),
                    None => {
                        warn!(
                            file_size = metadata.len(),
                            "Ignoring unrecognized file '{}' in retry directory.",
                            entry.path().display()
                        );
                        continue;
                    }
                }
            }
        }

        // Sort the entries by their inherent timestamp.
        entries.sort_by_key(|entry| entry.timestamp);
        self.total_on_disk_bytes = entries.iter().map(|entry| entry.size_bytes).sum();
        self.entries = entries;

        Ok(())
    }

    /// Removes persisted entries (oldest first) until there is at least the required number of bytes in free space
    /// (maximum - total).
    ///
    /// # Errors
    ///
    /// If there is an error while deleting persisted entries, an error is returned.
    async fn remove_until_available_space(&mut self, required_bytes: u64) -> Result<PushResult, GenericError> {
        let mut push_result = PushResult::default();

        let disk_usage_retriever = self.disk_usage_retriever.clone();
        let storage_max_disk_ratio = self.storage_max_disk_ratio;
        let max_on_disk_bytes = self.max_on_disk_bytes;
        let total_on_disk_bytes = self.total_on_disk_bytes;

        // TODO: Evaluate the possible failures scenarios a little more thoroughly, and see if we can improve
        // how we handle them instead of just bailing out.
        //
        // Essentially, it's not clear to me if we would expect this to fail in a way where we could actually
        // still write the persistent entries to disk, and if it's worth it to do something like trying to
        // cache the last known good value we get here to use if we fail to get a new value, etc.
        let limit = tokio::task::spawn_blocking(move || {
            on_disk_bytes_limit(
                disk_usage_retriever,
                storage_max_disk_ratio,
                max_on_disk_bytes,
                total_on_disk_bytes,
            )
        })
        .await
        .error_context("Failed to run disk size limit check to completion.")??;

        while !self.entries.is_empty() && self.total_on_disk_bytes + required_bytes > limit.bytes {
            let on_disk_bytes = self.total_on_disk_bytes;
            let entry = self.entries.remove(0);

            // Deserialize the entry, which gives us back the original event and removes the file from disk.
            let deserialized = match try_deserialize_entry::<T>(&entry).await {
                Ok(Some(deserialized)) => deserialized,
                Ok(None) => {
                    warn!(entry.path = %entry.path.display(), "Failed to find entry on disk. Persisted entry state may be inconsistent.");
                    continue;
                }
                Err(e) => {
                    // The entry is corrupt or unreadable. Drop it permanently to avoid blocking subsequent
                    // entries from being evicted.
                    warn!(
                        entry.path = %entry.path.display(),
                        entry.len = entry.size_bytes,
                        error = %e,
                        "Permanently dropping persisted entry that could not be consumed during eviction.",
                    );

                    self.total_on_disk_bytes -= entry.size_bytes;
                    self.entries_dropped += 1;

                    // When the file is gone, it was removed to make room, so count it as evicted, as the core Agent
                    // does, but with no known data points. A read or delete failure can leave the file in place.
                    if matches!(tokio::fs::try_exists(&entry.path).await, Ok(false)) {
                        self.evictions.files_removed += 1;
                    }

                    continue;
                }
            };

            // Update our statistics.
            self.total_on_disk_bytes -= entry.size_bytes;
            push_result.track_dropped_item(&deserialized);
            self.evictions.files_removed += 1;
            self.evictions.data_points_dropped += deserialized.data_point_count();

            // The message text here is kept in sync with an equivalent log emitted by another implementation of
            // this queue, so that log-based alerting on this condition matches regardless of which implementation
            // is deployed.
            error!(
                entry.path = %entry.path.display(),
                entry.len = entry.size_bytes,
                limit_reason = limit.reason.as_str(),
                limit_bytes = limit.bytes,
                on_disk_bytes,
                "Maximum disk space for retry transactions is reached. Removing {}.",
                entry.path.display()
            );
        }

        Ok(push_result)
    }
}

/// Determines the total number of bytes the queue may occupy on disk without causing the underlying volume to end up
/// with more than `storage_max_disk_ratio` in terms of used space. The minimum of `max_on_disk_bytes` and the result
/// of this calculation is returned, along with which of the two it is.
///
/// The available space reported by the volume already excludes the queue's own files, so `total_on_disk_bytes` is
/// added back in: the result is the queue's total allowance, not the space left on top of what it already uses.
///
/// # Errors
///
/// If there is an error while retrieving the total or available space of the underlying volume, an error is returned.
fn on_disk_bytes_limit(
    disk_usage_retriever: DiskUsageRetrieverWrapper, storage_max_disk_ratio: f64, max_on_disk_bytes: u64,
    total_on_disk_bytes: u64,
) -> Result<OnDiskLimit, GenericError> {
    let total_space = disk_usage_retriever.inner.total_space()? as f64;
    let available_space = disk_usage_retriever.inner.available_space()? as f64;
    let disk_reserved = total_space * (1.0 - storage_max_disk_ratio);

    // When the volume is already past the ratio, this goes negative and pulls the allowance below what the queue
    // currently uses, so that existing entries get evicted. Casting to `u64` saturates the final result at zero.
    let available_disk_usage = (total_on_disk_bytes as f64 + available_space - disk_reserved).ceil() as u64;
    Ok(if max_on_disk_bytes <= available_disk_usage {
        OnDiskLimit {
            bytes: max_on_disk_bytes,
            reason: OnDiskLimitReason::MaxSize,
        }
    } else {
        OnDiskLimit {
            bytes: available_disk_usage,
            reason: OnDiskLimitReason::MaxDiskRatio,
        }
    })
}

async fn try_deserialize_entry<T: DeserializeOwned>(entry: &PersistedEntry) -> Result<Option<T>, GenericError> {
    let serialized = match tokio::fs::read(&entry.path).await {
        Ok(serialized) => serialized,
        Err(e) => match e.kind() {
            io::ErrorKind::NotFound => {
                // We tried to delete an entry that no longer exists on disk, which means our internal entry state
                // is corrupted for some reason.
                //
                // Tell the caller that we couldn't find the entry on disk, so that they need to refresh the entry state
                // to make sure it's up-to-date before trying again.
                return Ok(None);
            }
            _ => {
                return Err(e)
                    .with_error_context(|| format!("Failed to read persisted entry '{}'.", entry.path.display()))
            }
        },
    };

    let deserialized = match serde_json::from_slice(&serialized) {
        Ok(deserialized) => deserialized,
        Err(e) => {
            // Deserialization failed, which means the payload is corrupt or invalid. Attempt to clean up the
            // file from disk so it doesn't accumulate, but don't fail if we can't.
            if let Err(remove_err) = tokio::fs::remove_file(&entry.path).await {
                warn!(
                    entry.path = %entry.path.display(),
                    error = %remove_err,
                    "Failed to remove corrupt persisted entry from disk.",
                );
            }

            return Err(e)
                .with_error_context(|| format!("Failed to deserialize persisted entry '{}'.", entry.path.display()));
        }
    };

    // Delete the entry from disk before returning, so that we don't risk sending duplicates.
    tokio::fs::remove_file(&entry.path)
        .await
        .with_error_context(|| format!("Failed to delete persisted entry '{}'.", entry.path.display()))?;

    debug!(entry.path = %entry.path.display(), entry.len = entry.size_bytes, "Consumed persisted entry and removed from disk.");
    Ok(Some(deserialized))
}

fn generate_timestamped_filename() -> Result<(PathBuf, u128), GenericError> {
    let now = Timestamp::now()?;
    let now_ts = u128::try_from(now.as_unix_nanos())
        .map_err(|_| generic_error!("System clock is set to before the Unix epoch."))?;
    let nonce = rand::rng().random_range(100000000..999999999);

    let filename = format!("retry-{}-{}.json", now.format(TimestampFormat::CompactUtcNanos), nonce).into();

    Ok((filename, now_ts))
}

fn decode_timestamped_filename(path: &Path) -> Option<u128> {
    let filename = path.file_stem()?.to_str()?;
    let mut filename_parts = filename.split('-');

    let prefix = filename_parts.next()?;
    let timestamp_str = filename_parts.next()?;
    let nonce = filename_parts.next()?;

    // Make sure the filename matches our expected format by first checking the prefix and nonce portions.
    if prefix != "retry" || nonce.parse::<u64>().is_err() {
        return None;
    }

    // Every version has written the timestamp in `TimestampFormat::CompactUtcNanos`. Dates before the Unix epoch aren't
    // recognized.
    parse_compact_utc_nanos(timestamp_str).and_then(|nanos| u128::try_from(nanos).ok())
}

async fn create_directory_recursive(path: PathBuf) -> Result<(), GenericError> {
    let mut dir_builder = std::fs::DirBuilder::new();
    dir_builder.recursive(true);

    // When on Unix platforms, adjust the permissions of the directory to be RWX for the owner only, and nothing for
    // group/world.
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        dir_builder.mode(0o700);
    }

    tokio::task::spawn_blocking(move || {
        dir_builder
            .create(&path)
            .with_error_context(|| format!("Failed to create directory '{}'.", path.display()))
    })
    .await
    .error_context("Failed to spawn directory creation blocking task.")?
}

/// Deletes files in `queue_path` whose filename-embedded creation timestamp is older than
/// `max_age_days`. Does nothing if the directory does not exist.
///
/// Setting `max_age_days` to `0` deletes all retry files (cutoff = now), matching the behavior
/// of the core Agent's `FileRemovalPolicy` with `outdatedFileDayCount = 0`.
async fn remove_outdated_retry_files(queue_path: &Path, max_age_days: u32) -> Result<u32, GenericError> {
    let mut dir = match tokio::fs::read_dir(queue_path).await {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => {
            return Err(e).with_error_context(|| {
                format!(
                    "Failed to open retry queue directory '{}' for age-based cleanup.",
                    queue_path.display()
                )
            });
        }
    };
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default() // clock before epoch: treat cutoff as 0, skipping all deletions
        .as_nanos();
    let cutoff_ns = now_ns.saturating_sub(max_age_days as u128 * 24 * 3600 * 1_000_000_000);
    let mut removed = 0u32;
    loop {
        let entry = match dir.next_entry().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => {
                return Err(e).with_error_context(|| "Error reading retry queue directory during age-based cleanup.");
            }
        };
        let file_ts = match decode_timestamped_filename(&entry.path()) {
            Some(ts) => ts,
            None => continue,
        };
        if file_ts < cutoff_ns {
            let name_str = entry.file_name();
            let name = name_str.to_string_lossy();
            match tokio::fs::remove_file(entry.path()).await {
                Ok(()) => {
                    debug!(file = %name, "Removed outdated retry file.");
                    removed += 1;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    debug!(file = %name, "Retry file already removed by concurrent cleanup.");
                }
                Err(e) => {
                    warn!(file = %name, error = %e, "Failed to remove outdated retry file.");
                }
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use rand::RngExt as _;
    use rand_distr::Alphanumeric;
    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::net::util::retry::EventContainer;

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    struct FakeData {
        name: String,
        value: u32,
    }

    impl FakeData {
        fn random() -> Self {
            Self {
                name: rand::rng().sample_iter(&Alphanumeric).take(8).map(char::from).collect(),
                value: rand::rng().random_range(0..100),
            }
        }
    }

    impl EventContainer for FakeData {
        fn event_count(&self) -> u64 {
            1
        }

        fn data_point_count(&self) -> u64 {
            2
        }
    }

    impl Retryable for FakeData {
        fn size_bytes(&self) -> u64 {
            (self.name.len() + std::mem::size_of::<String>() + 4) as u64
        }
    }

    struct MockDiskUsageRetriever {}

    impl DiskUsageRetriever for MockDiskUsageRetriever {
        fn total_space(&self) -> Result<u64, GenericError> {
            Ok(100)
        }
        fn available_space(&self) -> Result<u64, GenericError> {
            Ok(100)
        }
    }

    /// A fake volume holding `other_used_bytes` of unrelated data plus whatever files are in `dir`.
    ///
    /// Like a real volume, the available space it reports shrinks as the queue writes entries.
    struct DirBackedDiskUsageRetriever {
        dir: PathBuf,
        total_bytes: u64,
        other_used_bytes: u64,
    }

    impl DiskUsageRetriever for DirBackedDiskUsageRetriever {
        fn total_space(&self) -> Result<u64, GenericError> {
            Ok(self.total_bytes)
        }

        fn available_space(&self) -> Result<u64, GenericError> {
            let mut queue_bytes = 0;
            for entry in std::fs::read_dir(&self.dir)? {
                let metadata = entry?.metadata()?;
                if metadata.is_file() {
                    queue_bytes += metadata.len();
                }
            }
            Ok(self.total_bytes - self.other_used_bytes - queue_bytes)
        }
    }

    async fn files_in_dir(path: &Path) -> usize {
        let mut file_count = 0;
        let mut dir_reader = tokio::fs::read_dir(path).await.unwrap();
        while let Some(entry) = dir_reader.next_entry().await.unwrap() {
            if entry.metadata().await.unwrap().is_file() {
                file_count += 1;
            }
        }
        file_count
    }

    #[tokio::test]
    async fn basic_push_pop() {
        let data = FakeData::random();

        // Create our temporary directory and point our persisted queue at it.
        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();

        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(PersistedQueueArgs {
            root_path: root_path.clone(),
            max_on_disk_bytes: 1024,
            storage_max_disk_ratio: 0.8,
            disk_usage_retriever: Arc::new(DiskUsageRetrieverImpl::new(root_path.clone())),
            max_age_days: 10,
        })
        .await
        .expect("should not fail to create persisted queue");

        // Ensure the directory is empty.
        assert_eq!(0, files_in_dir(&root_path).await);

        // Push our data to the queue and ensure it persisted it to disk.
        let push_result = persisted_queue
            .push(data.clone())
            .await
            .expect("should not fail to push data");
        assert_eq!(1, files_in_dir(&root_path).await);
        assert_eq!(0, push_result.items_dropped);
        assert_eq!(0, push_result.events_dropped);

        // Now pop the data back out and ensure it matches what we pushed, and that the file has been removed from disk.
        let actual = persisted_queue
            .pop()
            .await
            .expect("should not fail to pop data")
            .expect("should not be empty");
        assert_eq!(data, actual);
        assert_eq!(0, files_in_dir(&root_path).await);
    }

    #[tokio::test]
    async fn entry_too_large() {
        let data = FakeData::random();

        // Create our temporary directory and point our persisted queue at it.
        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();

        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(PersistedQueueArgs {
            root_path: root_path.clone(),
            max_on_disk_bytes: 1,
            storage_max_disk_ratio: 0.8,
            disk_usage_retriever: Arc::new(DiskUsageRetrieverImpl::new(root_path.clone())),
            max_age_days: 10,
        })
        .await
        .expect("should not fail to create persisted queue");

        // Ensure the directory is empty.
        assert_eq!(0, files_in_dir(&root_path).await);

        // Attempt to push our data into the queue, which should fail because it's too large.
        assert!(persisted_queue.push(data).await.is_err());

        // Ensure the directory is (still) empty.
        assert_eq!(0, files_in_dir(&root_path).await);
    }

    #[tokio::test]
    async fn remove_oldest_entry_on_push() {
        let data1 = FakeData::random();
        let data2 = FakeData::random();

        // Create our temporary directory and point our persisted queue at it.
        //
        // Our queue is sized such that only one entry can be persisted at a time.
        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();

        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(PersistedQueueArgs {
            root_path: root_path.clone(),
            max_on_disk_bytes: 32,
            storage_max_disk_ratio: 0.8,
            disk_usage_retriever: Arc::new(DiskUsageRetrieverImpl::new(root_path.clone())),
            max_age_days: 10,
        })
        .await
        .expect("should not fail to create persisted queue");

        // Ensure the directory is empty.
        assert_eq!(0, files_in_dir(&root_path).await);

        // Push our data to the queue and ensure it persisted it to disk.
        let data1_size = data1.size_bytes();
        let push_result = persisted_queue.push(data1).await.expect("should not fail to push data");
        assert_eq!(1, files_in_dir(&root_path).await);
        assert_eq!(0, push_result.items_dropped);
        assert_eq!(0, push_result.events_dropped);
        assert_eq!(0, push_result.bytes_dropped);
        let first_stats = persisted_queue.stats();
        assert_eq!(1, first_stats.files);
        assert!(first_stats.size_bytes > 0);
        assert_eq!(PersistedQueueEvictions::default(), persisted_queue.take_evictions());

        // Push a second data entry, which should cause the first entry to be removed.
        let push_result = persisted_queue
            .push(data2.clone())
            .await
            .expect("should not fail to push data");
        assert_eq!(1, files_in_dir(&root_path).await);
        assert_eq!(1, push_result.items_dropped);
        assert_eq!(1, push_result.events_dropped);
        assert_eq!(data1_size, push_result.bytes_dropped);
        assert_eq!(1, persisted_queue.stats().files);
        assert_eq!(
            PersistedQueueEvictions {
                files_removed: 1,
                data_points_dropped: 2,
            },
            persisted_queue.take_evictions()
        );

        // Taking the evictions resets them.
        assert_eq!(PersistedQueueEvictions::default(), persisted_queue.take_evictions());

        // Now pop the data back out and ensure it matches the second item we pushed -- indicating the first item was
        // removed -- and that we've consumed it, leaving no files on disk.
        let actual = persisted_queue
            .pop()
            .await
            .expect("should not fail to pop data")
            .expect("should not be empty");
        assert_eq!(data2, actual);
        assert_eq!(0, files_in_dir(&root_path).await);
        assert_eq!(PersistedQueueStats::default(), persisted_queue.stats());
    }

    #[test]
    fn on_disk_bytes_limit_reports_which_setting_applies() {
        // `MockDiskUsageRetriever` reports 100 bytes total and available, so a ratio of 0.35 allows 35 bytes.
        let retriever = || DiskUsageRetrieverWrapper::new(Arc::new(MockDiskUsageRetriever {}));

        assert_eq!(
            OnDiskLimit {
                bytes: 35,
                reason: OnDiskLimitReason::MaxDiskRatio,
            },
            on_disk_bytes_limit(retriever(), 0.35, 80, 0).unwrap()
        );
        assert_eq!(
            OnDiskLimit {
                bytes: 20,
                reason: OnDiskLimitReason::MaxSize,
            },
            on_disk_bytes_limit(retriever(), 0.35, 20, 0).unwrap()
        );
    }

    #[tokio::test]
    async fn storage_ratio_exceeded() {
        let data1 = FakeData::random();
        let data2 = FakeData::random();

        // Create our temporary directory and point our persisted queue at it.
        //
        // Our queue is sized such that two entries can be persisted at a time.
        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();

        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(PersistedQueueArgs {
            root_path: root_path.clone(),
            max_on_disk_bytes: 80,
            storage_max_disk_ratio: 0.35,
            disk_usage_retriever: Arc::new(DirBackedDiskUsageRetriever {
                dir: root_path.clone(),
                total_bytes: 100,
                other_used_bytes: 0,
            }),
            max_age_days: 10,
        })
        .await
        .expect("should not fail to create persisted queue");

        // Ensure the directory is empty.
        assert_eq!(0, files_in_dir(&root_path).await);

        // The `storage_max_disk_ratio` is 0.35 and the volume is 100 bytes with nothing else on it, so
        // `on_disk_bytes_limit()` returns min(80, 35) = 35 regardless of how much the queue has written.
        //
        // First entry: total_on_disk_bytes(0) + required_bytes(30) < on_disk_bytes_limit(35)
        let push_result = persisted_queue.push(data1).await.expect("should not fail to push data");

        assert_eq!(1, files_in_dir(&root_path).await);
        assert_eq!(0, push_result.items_dropped);
        assert_eq!(0, push_result.events_dropped);

        // Second entry: total_on_disk_bytes(30) + required_bytes(30) > on_disk_bytes_limit(35) so the first entry is dropped.
        let push_result = persisted_queue
            .push(data2.clone())
            .await
            .expect("should not fail to push data");
        assert_eq!(1, files_in_dir(&root_path).await);
        assert_eq!(1, push_result.items_dropped);
        assert_eq!(1, push_result.events_dropped);

        // Now pop the data back out and ensure it matches the second item we pushed -- indicating the first item was
        // removed -- and that we've consumed it, leaving no files on disk.
        let actual = persisted_queue
            .pop()
            .await
            .expect("should not fail to pop data")
            .expect("should not be empty");
        assert_eq!(data2, actual);
        assert_eq!(0, files_in_dir(&root_path).await);
    }

    #[tokio::test]
    async fn storage_ratio_does_not_double_count_queue_bytes() {
        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();

        // The volume is 1000 bytes with 400 bytes of unrelated data, and the ratio reserves 500 bytes, so the queue may
        // occupy 100 bytes in total. Entries serialize to about 30 bytes each, so three fit.
        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(PersistedQueueArgs {
            root_path: root_path.clone(),
            max_on_disk_bytes: 1024,
            storage_max_disk_ratio: 0.5,
            disk_usage_retriever: Arc::new(DirBackedDiskUsageRetriever {
                dir: root_path.clone(),
                total_bytes: 1000,
                other_used_bytes: 400,
            }),
            max_age_days: 10,
        })
        .await
        .expect("should not fail to create persisted queue");

        // Counting the queue's files twice would leave room for only two entries, evicting one on the third push.
        for _ in 0..3 {
            let push_result = persisted_queue
                .push(FakeData::random())
                .await
                .expect("should not fail to push data");
            assert_eq!(0, push_result.items_dropped);

            let available = persisted_queue
                .available_capacity_bytes()
                .await
                .expect("should not fail to get available capacity");
            assert_eq!(100 - persisted_queue.total_on_disk_bytes, available);
        }
        assert_eq!(3, files_in_dir(&root_path).await);

        // A fourth entry exceeds the 100-byte allowance, so the oldest entry is evicted.
        let push_result = persisted_queue
            .push(FakeData::random())
            .await
            .expect("should not fail to push data");
        assert_eq!(1, push_result.items_dropped);
        assert_eq!(3, files_in_dir(&root_path).await);
    }

    /// Writes a corrupt (non-JSON) file with a valid retry filename to the given directory, using a timestamp
    /// that sorts before any real entries (so it will be popped first).
    async fn write_corrupt_entry(dir: &Path) -> PathBuf {
        let filename = "retry-20000101000000000000000-100000000.json";
        let path = dir.join(filename);
        tokio::fs::write(&path, b"this is not valid json").await.unwrap();
        path
    }

    #[tokio::test]
    async fn corrupt_entry_is_skipped_on_pop() {
        let data = FakeData::random();

        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();

        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(PersistedQueueArgs {
            root_path: root_path.clone(),
            max_on_disk_bytes: 1024,
            storage_max_disk_ratio: 0.8,
            disk_usage_retriever: Arc::new(MockDiskUsageRetriever {}),
            max_age_days: 10,
        })
        .await
        .expect("should not fail to create persisted queue");

        // Write a corrupt file before pushing valid data, so it sorts first.
        let corrupt_path = write_corrupt_entry(&root_path).await;

        // Push a valid entry.
        let _ = persisted_queue
            .push(data.clone())
            .await
            .expect("should not fail to push data");

        // Refresh state so the queue picks up the corrupt file.
        persisted_queue.refresh_entry_state().await.unwrap();

        // Pop should skip the corrupt entry and return the valid one.
        let actual = persisted_queue
            .pop()
            .await
            .expect("should not fail to pop data")
            .expect("should have a valid entry");
        assert_eq!(data, actual);

        // The corrupt file should have been cleaned up from disk.
        assert!(!corrupt_path.exists());

        // The dropped counter should reflect the corrupt entry.
        assert_eq!(1, persisted_queue.take_entries_dropped());

        // No files should remain.
        assert_eq!(0, files_in_dir(&root_path).await);
    }

    #[tokio::test]
    async fn corrupt_entry_does_not_block_queue() {
        let data1 = FakeData::random();
        let data2 = FakeData::random();

        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();

        // Use MockDiskUsageRetriever to avoid disk space ratio causing eviction during push.
        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(PersistedQueueArgs {
            root_path: root_path.clone(),
            max_on_disk_bytes: 1024,
            storage_max_disk_ratio: 0.8,
            disk_usage_retriever: Arc::new(MockDiskUsageRetriever {}),
            max_age_days: 10,
        })
        .await
        .expect("should not fail to create persisted queue");

        // Push two valid entries, then corrupt the first one on disk.
        let _ = persisted_queue.push(data1).await.expect("should not fail to push data");
        let _ = persisted_queue
            .push(data2.clone())
            .await
            .expect("should not fail to push data");
        assert_eq!(2, persisted_queue.entries.len());

        // Corrupt the oldest entry file on disk.
        let oldest_path = persisted_queue.entries[0].path.clone();
        tokio::fs::write(&oldest_path, b"corrupted").await.unwrap();

        // Pop should skip the corrupt entry and return the second valid one.
        let actual = persisted_queue
            .pop()
            .await
            .expect("should not fail to pop data")
            .expect("should have a valid entry");
        assert_eq!(data2, actual);

        assert_eq!(1, persisted_queue.take_entries_dropped());
        assert_eq!(0, files_in_dir(&root_path).await);
    }

    #[tokio::test]
    async fn pop_returns_none_when_all_entries_corrupt() {
        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();

        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(PersistedQueueArgs {
            root_path: root_path.clone(),
            max_on_disk_bytes: 1024,
            storage_max_disk_ratio: 0.8,
            disk_usage_retriever: Arc::new(MockDiskUsageRetriever {}),
            max_age_days: 10,
        })
        .await
        .expect("should not fail to create persisted queue");

        // Write a corrupt entry and refresh state.
        write_corrupt_entry(&root_path).await;
        persisted_queue.refresh_entry_state().await.unwrap();

        // Pop should skip the corrupt entry and return None (no valid entries).
        let result = persisted_queue.pop().await.expect("should not fail to pop data");
        assert!(result.is_none());

        assert_eq!(1, persisted_queue.take_entries_dropped());
        assert_eq!(0, files_in_dir(&root_path).await);
    }

    #[tokio::test]
    async fn corrupt_entry_dropped_during_eviction() {
        let data = FakeData::random();

        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();

        // Queue sized to hold only one entry.
        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(PersistedQueueArgs {
            root_path: root_path.clone(),
            max_on_disk_bytes: 32,
            storage_max_disk_ratio: 0.8,
            disk_usage_retriever: Arc::new(MockDiskUsageRetriever {}),
            max_age_days: 10,
        })
        .await
        .expect("should not fail to create persisted queue");

        // Push a valid entry, then corrupt it on disk.
        let _ = persisted_queue
            .push(FakeData::random())
            .await
            .expect("should not fail to push data");
        let first_path = persisted_queue.entries[0].path.clone();
        tokio::fs::write(&first_path, b"corrupted").await.unwrap();

        // Push another entry, which needs to evict the first (corrupt) one to make space.
        // This should succeed without error -- the corrupt entry is dropped during eviction.
        let _ = persisted_queue
            .push(data.clone())
            .await
            .expect("should not fail to push data");

        // The corrupt entry was dropped during eviction, not via normal eviction tracking, but still counts as a
        // removed file. Its data points are unknown.
        assert_eq!(1, persisted_queue.take_entries_dropped());
        assert_eq!(
            PersistedQueueEvictions {
                files_removed: 1,
                data_points_dropped: 0,
            },
            persisted_queue.take_evictions()
        );

        // The valid entry should be poppable.
        let actual = persisted_queue
            .pop()
            .await
            .expect("should not fail to pop data")
            .expect("should have a valid entry");
        assert_eq!(data, actual);
        assert_eq!(0, files_in_dir(&root_path).await);
    }

    #[tokio::test]
    async fn unreadable_entry_left_on_disk_is_not_counted_as_evicted() {
        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();

        // Queue sized to hold only one entry.
        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(PersistedQueueArgs {
            root_path: root_path.clone(),
            max_on_disk_bytes: 32,
            storage_max_disk_ratio: 0.8,
            disk_usage_retriever: Arc::new(MockDiskUsageRetriever {}),
            max_age_days: 10,
        })
        .await
        .expect("should not fail to create persisted queue");

        // Replace the first entry's file with a directory, which can be neither read nor removed as a file.
        let _ = persisted_queue
            .push(FakeData::random())
            .await
            .expect("should not fail to push data");
        let first_path = persisted_queue.entries[0].path.clone();
        tokio::fs::remove_file(&first_path).await.unwrap();
        tokio::fs::create_dir(&first_path).await.unwrap();

        // Evicting it fails, so it's dropped from the queue but not counted as a removed file.
        let _ = persisted_queue
            .push(FakeData::random())
            .await
            .expect("should not fail to push data");
        assert_eq!(1, persisted_queue.take_entries_dropped());
        assert_eq!(PersistedQueueEvictions::default(), persisted_queue.take_evictions());
        assert!(tokio::fs::try_exists(&first_path).await.unwrap());
    }

    #[tokio::test]
    async fn persisted_queue_removes_outdated_files_on_initialization() {
        let data = FakeData::random();

        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();

        // Pre-seed a year-2000 retry file containing valid data that would be loaded as an entry
        // if it were not cleaned up first.
        let stale_content = serde_json::to_vec(&data).unwrap();
        tokio::fs::write(
            root_path.join("retry-20000101000000000000000-100000000.json"),
            &stale_content,
        )
        .await
        .unwrap();

        assert_eq!(1, files_in_dir(&root_path).await);

        // Initialize the queue and remove stale files with a 10-day age limit.
        let mut queue = PersistedQueue::<FakeData>::from_root_path(PersistedQueueArgs {
            root_path: root_path.clone(),
            max_on_disk_bytes: 1024 * 1024,
            storage_max_disk_ratio: 0.8,
            disk_usage_retriever: Arc::new(DiskUsageRetrieverImpl::new(root_path.clone())),
            max_age_days: 10,
        })
        .await
        .expect("should not fail to create persisted queue");
        queue
            .remove_stale_files()
            .await
            .expect("should not fail to remove stale files");

        assert_eq!(0, files_in_dir(&root_path).await);
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn persisted_queue_zero_age_removes_all_retry_files_on_initialization() {
        // max_age_days=0 sets cutoff=now, matching the core Agent's FileRemovalPolicy behavior
        // with outdatedFileDayCount=0 — all retry files are removed on startup.
        let data = FakeData::random();

        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();

        // Seed with a freshly-written entry via a normal queue.
        let mut seeding_queue = PersistedQueue::<FakeData>::from_root_path(PersistedQueueArgs {
            root_path: root_path.clone(),
            max_on_disk_bytes: 1024 * 1024,
            storage_max_disk_ratio: 0.8,
            disk_usage_retriever: Arc::new(DiskUsageRetrieverImpl::new(root_path.clone())),
            max_age_days: 10,
        })
        .await
        .expect("should not fail to create persisted queue");
        let _ = seeding_queue.push(data).await.expect("should not fail to push data");
        assert_eq!(1, files_in_dir(&root_path).await);

        // Re-open and remove stale files with max_age_days=0: the just-written file must also be deleted.
        let mut queue = PersistedQueue::<FakeData>::from_root_path(PersistedQueueArgs {
            root_path: root_path.clone(),
            max_on_disk_bytes: 1024 * 1024,
            storage_max_disk_ratio: 0.8,
            disk_usage_retriever: Arc::new(DiskUsageRetrieverImpl::new(root_path.clone())),
            max_age_days: 0,
        })
        .await
        .expect("should not fail to create persisted queue");
        queue
            .remove_stale_files()
            .await
            .expect("should not fail to remove stale files");

        assert_eq!(0, files_in_dir(&root_path).await);
        assert!(queue.is_empty());
    }

    fn at(secs: i64, nanos: u32) -> Timestamp {
        Timestamp::from_unix_nanos(i128::from(secs) * 1_000_000_000 + i128::from(nanos))
            .expect("test timestamp should be valid")
    }

    fn filename_timestamp(timestamp: Timestamp) -> String {
        timestamp.format(TimestampFormat::CompactUtcNanos).to_string()
    }

    // Retry files persist across upgrades and downgrades, so these names and values must never change: every version
    // has to read the names that every other version writes.
    const FILENAME_TIMESTAMPS: &[(&str, u128)] = &[
        ("19700101000000000000000", 0),
        ("20000101000000000000000", 946_684_800_000_000_000),
        ("20240229000000000000000", 1_709_164_800_000_000_000),
        ("20261118134530000000000", 1_795_009_530_000_000_000),
        ("20261118134530000000001", 1_795_009_530_000_000_001),
        ("20261118134530000007000", 1_795_009_530_000_007_000),
        ("20261118134530100000000", 1_795_009_530_100_000_000),
        ("20261118134530123456789", 1_795_009_530_123_456_789),
        ("20261118134530999999999", 1_795_009_530_999_999_999),
        ("99991231235959999999999", 253_402_300_799_999_999_999),
    ];

    #[test]
    fn filename_timestamps_decode_to_unix_nanoseconds() {
        for (timestamp, expected) in FILENAME_TIMESTAMPS {
            let path = PathBuf::from(format!("/queue/retry-{}-123456789.json", timestamp));
            assert_eq!(decode_timestamped_filename(&path), Some(*expected), "{}", timestamp);
        }
    }

    #[test]
    fn filename_timestamps_are_written_with_nine_fraction_digits() {
        let cases = [
            (at(0, 0), "19700101000000000000000"),
            (at(946_684_800, 0), "20000101000000000000000"),
            (at(1_795_009_530, 0), "20261118134530000000000"),
            (at(1_795_009_530, 1), "20261118134530000000001"),
            (at(1_795_009_530, 7_000), "20261118134530000007000"),
            (at(1_795_009_530, 100_000_000), "20261118134530100000000"),
            (at(1_795_009_530, 123_456_789), "20261118134530123456789"),
            (at(1_795_009_530, 999_999_999), "20261118134530999999999"),
        ];

        for (timestamp, expected) in cases {
            let formatted = filename_timestamp(timestamp);
            assert_eq!(formatted, expected);

            let path = PathBuf::from(format!("retry-{}-123456789.json", formatted));
            assert_eq!(
                decode_timestamped_filename(&path),
                u128::try_from(timestamp.as_unix_nanos()).ok()
            );
        }
    }

    #[test]
    fn filenames_that_dont_match_the_retry_format_are_ignored() {
        for filename in [
            "other-20261118134530000000000-123456789.json",
            "retry-20261118134530000000000-nonce.json",
            "retry-20261118134530000000000.json",
            "retry--123456789.json",
            "retry-2026111813453000000000a-123456789.json",
            // February 30th.
            "retry-20260230000000000000000-123456789.json",
        ] {
            assert_eq!(decode_timestamped_filename(Path::new(filename)), None, "{}", filename);
        }
    }

    #[tokio::test]
    async fn existing_entries_are_ordered_by_their_filename_timestamp() {
        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();

        let mut expected = Vec::new();
        for (timestamp, value) in FILENAME_TIMESTAMPS.iter().rev() {
            let path = root_path.join(format!("retry-{}-123456789.json", timestamp));
            tokio::fs::write(&path, b"{}").await.unwrap();
            expected.push(*value);
        }
        expected.sort_unstable();

        let queue = PersistedQueue::<FakeData>::from_root_path(PersistedQueueArgs {
            root_path: root_path.clone(),
            max_on_disk_bytes: 1024 * 1024,
            storage_max_disk_ratio: 0.8,
            disk_usage_retriever: Arc::new(MockDiskUsageRetriever {}),
            max_age_days: 10,
        })
        .await
        .expect("should not fail to create persisted queue");

        let actual = queue.entries.iter().map(|entry| entry.timestamp).collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    fn filename_timestamps_that_no_version_writes_are_ignored() {
        // chrono, which earlier versions used, read some of these differently: fewer than nine fraction digits as
        // whole nanoseconds, a leap second as the following second, and a date before the epoch not at all (it
        // panicked or overflowed).
        for timestamp in [
            "2026111813453012345678",
            "202611181345301234567890",
            "20161231235960000000000",
            "19691231235959000000000",
            " 2026111813453000000000",
            "+2026111813453000000000",
        ] {
            let path = PathBuf::from(format!("retry-{}-123456789.json", timestamp));
            assert_eq!(decode_timestamped_filename(&path), None, "{}", timestamp);
        }
    }

    #[tokio::test]
    async fn files_dated_before_the_epoch_are_left_alone() {
        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();
        let before_epoch = root_path.join("retry-19691231235959000000000-123456789.json");
        tokio::fs::write(&before_epoch, b"{}").await.unwrap();
        tokio::fs::write(root_path.join("retry-20000101000000000000000-123456789.json"), b"{}")
            .await
            .unwrap();

        let queue = PersistedQueue::<FakeData>::from_root_path(PersistedQueueArgs {
            root_path: root_path.clone(),
            max_on_disk_bytes: 1024 * 1024,
            storage_max_disk_ratio: 0.8,
            disk_usage_retriever: Arc::new(MockDiskUsageRetriever {}),
            max_age_days: 10,
        })
        .await
        .expect("should not fail to create persisted queue");
        assert_eq!(queue.len(), 1);

        let removed = remove_outdated_retry_files(&root_path, 0)
            .await
            .expect("should not fail to remove outdated files");
        assert_eq!(removed, 1);
        assert!(before_epoch.exists());
    }

    proptest::proptest! {
        #[test]
        // Up to 9999-12-30T22:00:00.999999999Z, the latest `Timestamp`.
        fn property_test_filename_timestamps_round_trip(nanos in 0..=253_402_207_200_999_999_999i128) {
            let timestamp = Timestamp::from_unix_nanos(nanos).expect("timestamp should be valid");
            let formatted = filename_timestamp(timestamp);

            proptest::prop_assert_eq!(formatted.len(), 23);
            proptest::prop_assert!(formatted.bytes().all(|b| b.is_ascii_digit()));
            let path = PathBuf::from(format!("retry-{}-123456789.json", formatted));
            proptest::prop_assert_eq!(decode_timestamped_filename(&path), u128::try_from(nanos).ok());
        }
    }
}
