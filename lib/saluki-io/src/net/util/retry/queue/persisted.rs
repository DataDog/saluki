use std::{
    io,
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::{DateTime, NaiveDateTime, Utc};
use fs4::{available_space, total_space};
use rand::RngExt as _;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::{de::DeserializeOwned, Serialize};
use tracing::{debug, info, warn};

use super::{EventContainer, PushResult};

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

pub struct PersistedQueue<T> {
    root_path: PathBuf,
    entries: Vec<PersistedEntry>,
    total_on_disk_bytes: u64,
    max_on_disk_bytes: u64,
    storage_max_disk_ratio: f64,
    disk_usage_retriever: DiskUsageRetrieverWrapper,
    entries_dropped: u64,
    _entry: PhantomData<T>,
}

impl<T> PersistedQueue<T>
where
    T: EventContainer + DeserializeOwned + Serialize,
{
    /// Creates a new `PersistedQueue` instance from the given root path and maximum size.
    ///
    /// The root path is created if it doesn't already exist, and is scanned for existing persisted entries. Entries
    /// are removed (oldest first) until the total size of all scanned entries is within the given maximum size.
    ///
    /// # Errors
    ///
    /// If there is an error creating the root directory, or scanning it for existing entries, or deleting entries to
    /// shrink the directory to fit the given maximum size, an error is returned.
    pub async fn from_root_path(
        root_path: PathBuf, max_on_disk_bytes: u64, storage_max_disk_ratio: f64,
        disk_usage_retriever: DiskUsageRetrieverWrapper, max_age_days: u32,
    ) -> Result<Self, GenericError> {
        // Make sure the directory exists first.
        create_directory_recursive(root_path.clone())
            .await
            .with_error_context(|| format!("Failed to create retry directory '{}'.", root_path.display()))?;

        // Remove stale retry files before loading state. This must run after directory creation
        // but before refresh_entry_state so the queue doesn't load files that are about to be
        // removed.
        remove_outdated_retry_files(&root_path, max_age_days).await;

        let mut persisted_requests = Self {
            root_path: root_path.clone(),
            entries: Vec::new(),
            total_on_disk_bytes: 0,
            max_on_disk_bytes,
            storage_max_disk_ratio,
            disk_usage_retriever,
            entries_dropped: 0,
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

    /// Returns the number of entries that have been permanently dropped due to errors since the last call to this
    /// method, resetting the counter.
    pub fn take_entries_dropped(&mut self) -> u64 {
        std::mem::take(&mut self.entries_dropped)
    }

    /// Enqueues an entry and persists it to disk.
    ///
    /// # Errors
    ///
    /// If there is an error serializing the entry, or writing it to disk, or removing older entries to make space for
    /// the new entry, an error is returned.
    pub async fn push(&mut self, entry: T) -> Result<PushResult, GenericError> {
        // Serialize the entry to a temporary file.
        let (filename, timestamp) = generate_timestamped_filename();
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

        // TODO: Evaluate the possible failures scenarios a little more thoroughly, and see if we can improve
        // how we handle them instead of just bailing out.
        //
        // Essentially, it's not clear to me if we would expect this to fail in a way where we could actually
        // still write the persistent entries to disk, and if it's worth it to do something like trying to
        // cache the last known good value we get here to use if we fail to get a new value, etc.
        let limit = tokio::task::spawn_blocking(move || {
            on_disk_bytes_limit(disk_usage_retriever, storage_max_disk_ratio, max_on_disk_bytes)
        })
        .await
        .error_context("Failed to run disk size limit check to completion.")??;

        while !self.entries.is_empty() && self.total_on_disk_bytes + required_bytes > limit {
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
                    continue;
                }
            };

            // Update our statistics.
            self.total_on_disk_bytes -= entry.size_bytes;
            push_result.track_dropped_item(&deserialized);

            warn!(entry.path = %entry.path.display(), entry.len = entry.size_bytes, "Dropped persisted entry.");
        }

        Ok(push_result)
    }
}

/// Determines the total number of bytes that can be written to disk without causing the underlying volume to end up
/// with more than `storage_max_disk_ratio` in terms of used space. The minimum of `max_on_disk_bytes` and the result
/// of this calculation is returned.
///
/// # Errors
///
/// If there is an error while retrieving the total or available space of the underlying volume, an error is returned.
fn on_disk_bytes_limit(
    disk_usage_retriever: DiskUsageRetrieverWrapper, storage_max_disk_ratio: f64, max_on_disk_bytes: u64,
) -> Result<u64, GenericError> {
    let total_space = disk_usage_retriever.inner.total_space()? as f64;
    let available_space = disk_usage_retriever.inner.available_space()? as f64;
    let disk_reserved = total_space * (1.0 - storage_max_disk_ratio);
    let available_disk_usage = (available_space - disk_reserved).ceil() as u64;
    Ok(max_on_disk_bytes.min(available_disk_usage))
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

fn generate_timestamped_filename() -> (PathBuf, u128) {
    let now = Utc::now();
    let now_ts = datetime_to_timestamp(now);
    let nonce = rand::rng().random_range(100000000..999999999);

    let filename = format!("retry-{}-{}.json", now.format("%Y%m%d%H%M%S%f"), nonce).into();

    (filename, now_ts)
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

    // Try and decode the timestamp portion.
    NaiveDateTime::parse_from_str(timestamp_str, "%Y%m%d%H%M%S%f")
        .map(|dt| datetime_to_timestamp(dt.and_utc()))
        .ok()
}

fn datetime_to_timestamp(dt: DateTime<Utc>) -> u128 {
    let secs = (dt.timestamp() as u128) * 1_000_000_000;
    let ns = dt.timestamp_subsec_nanos() as u128;

    secs + ns
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
/// `max_age_days`. Does nothing if `max_age_days` is 0 or the directory does not exist.
async fn remove_outdated_retry_files(queue_path: &Path, max_age_days: u32) {
    if max_age_days == 0 {
        return;
    }
    let mut dir = match tokio::fs::read_dir(queue_path).await {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            warn!(path = %queue_path.display(), error = %e, "Failed to open retry queue directory for age-based cleanup.");
            return;
        }
    };
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let cutoff_ns = now_ns.saturating_sub(max_age_days as u128 * 24 * 3600 * 1_000_000_000);
    let mut removed = 0u32;
    loop {
        let entry = match dir.next_entry().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => {
                warn!(error = %e, "Error reading retry queue directory during age-based cleanup.");
                break;
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
    if removed > 0 {
        info!(count = removed, max_age_days, "Removed outdated retry files from disk.");
    }
}

#[cfg(test)]
mod tests {
    use rand::RngExt as _;
    use rand_distr::Alphanumeric;
    use serde::Deserialize;

    use super::*;

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

        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(
            root_path.clone(),
            1024,
            0.8,
            DiskUsageRetrieverWrapper::new(Arc::new(DiskUsageRetrieverImpl::new(root_path.clone()))),
            0,
        )
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

        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(
            root_path.clone(),
            1,
            0.8,
            DiskUsageRetrieverWrapper::new(Arc::new(DiskUsageRetrieverImpl::new(root_path.clone()))),
            0,
        )
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

        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(
            root_path.clone(),
            32,
            0.8,
            DiskUsageRetrieverWrapper::new(Arc::new(DiskUsageRetrieverImpl::new(root_path.clone()))),
            0,
        )
        .await
        .expect("should not fail to create persisted queue");

        // Ensure the directory is empty.
        assert_eq!(0, files_in_dir(&root_path).await);

        // Push our data to the queue and ensure it persisted it to disk.
        let push_result = persisted_queue.push(data1).await.expect("should not fail to push data");
        assert_eq!(1, files_in_dir(&root_path).await);
        assert_eq!(0, push_result.items_dropped);
        assert_eq!(0, push_result.events_dropped);

        // Push a second data entry, which should cause the first entry to be removed.
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
    async fn storage_ratio_exceeded() {
        let data1 = FakeData::random();
        let data2 = FakeData::random();

        // Create our temporary directory and point our persisted queue at it.
        //
        // Our queue is sized such that two entries can be persisted at a time.
        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();

        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(
            root_path.clone(),
            80,
            0.35,
            DiskUsageRetrieverWrapper::new(Arc::new(MockDiskUsageRetriever {})),
            0,
        )
        .await
        .expect("should not fail to create persisted queue");

        // Ensure the directory is empty.
        assert_eq!(0, files_in_dir(&root_path).await);

        // The `storage_max_disk_ratio` is 0.35, and our `MockDiskUsageRetriever` returns 100 for both `total_space` and
        // `available_space`, so `on_disk_bytes_limit()` returns min(80, 35) = 35.
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

    /// Writes a corrupt (non-JSON) file with a valid retry filename to the given directory, using a timestamp
    /// that sorts before any real entries (so it will be popped first).
    async fn write_corrupt_entry(dir: &Path) -> PathBuf {
        let filename = "retry-20000101000000000000-100000000.json";
        let path = dir.join(filename);
        tokio::fs::write(&path, b"this is not valid json").await.unwrap();
        path
    }

    #[tokio::test]
    async fn corrupt_entry_is_skipped_on_pop() {
        let data = FakeData::random();

        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();

        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(
            root_path.clone(),
            1024,
            0.8,
            DiskUsageRetrieverWrapper::new(Arc::new(MockDiskUsageRetriever {})),
            0,
        )
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
        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(
            root_path.clone(),
            1024,
            0.8,
            DiskUsageRetrieverWrapper::new(Arc::new(MockDiskUsageRetriever {})),
            0,
        )
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

        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(
            root_path.clone(),
            1024,
            0.8,
            DiskUsageRetrieverWrapper::new(Arc::new(MockDiskUsageRetriever {})),
            0,
        )
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
        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(
            root_path.clone(),
            32,
            0.8,
            DiskUsageRetrieverWrapper::new(Arc::new(MockDiskUsageRetriever {})),
            0,
        )
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

        // The corrupt entry was dropped during eviction, not via normal eviction tracking.
        assert_eq!(1, persisted_queue.take_entries_dropped());

        // The valid entry should be poppable.
        let actual = persisted_queue
            .pop()
            .await
            .expect("should not fail to pop data")
            .expect("should have a valid entry");
        assert_eq!(data, actual);
        assert_eq!(0, files_in_dir(&root_path).await);
    }

    mod outdated_file_cleanup {
        use std::path::Path;

        use chrono::{Duration, Utc};
        use tempfile::TempDir;
        use tokio::fs;

        use super::super::remove_outdated_retry_files;

        fn retry_filename_days_old(days_old: i64, nonce: u64) -> String {
            let ts = Utc::now() - Duration::days(days_old);
            format!("retry-{}-{}.json", ts.format("%Y%m%d%H%M%S%f"), nonce)
        }

        async fn write_file(dir: &Path, name: &str) {
            fs::write(dir.join(name), b"{}").await.unwrap();
        }

        async fn names_in(dir: &Path) -> Vec<String> {
            let mut entries = fs::read_dir(dir).await.unwrap();
            let mut names = Vec::new();
            while let Some(e) = entries.next_entry().await.unwrap() {
                names.push(e.file_name().to_string_lossy().into_owned());
            }
            names.sort();
            names
        }

        #[tokio::test]
        async fn removes_old_retry_files_only() {
            let dir = TempDir::new().unwrap();
            let path = dir.path();

            let old_1 = retry_filename_days_old(15, 100000001);
            let old_2 = retry_filename_days_old(11, 100000002);
            let recent = retry_filename_days_old(1, 100000003);

            write_file(path, &old_1).await;
            write_file(path, &old_2).await;
            write_file(path, &recent).await;
            write_file(path, "other-file.json").await; // not a valid retry filename, must not be touched

            remove_outdated_retry_files(path, 10).await;

            let remaining = names_in(path).await;
            assert!(!remaining.contains(&old_1), "15-day-old file should be removed");
            assert!(!remaining.contains(&old_2), "11-day-old file should be removed");
            assert!(remaining.contains(&recent), "1-day-old file should be kept");
            assert!(
                remaining.contains(&"other-file.json".to_string()),
                "non-retry file must not be touched"
            );
        }

        #[tokio::test]
        async fn zero_days_disables_cleanup() {
            let dir = TempDir::new().unwrap();
            let path = dir.path();

            let old = retry_filename_days_old(100, 100000004);
            write_file(path, &old).await;

            remove_outdated_retry_files(path, 0).await;

            let remaining = names_in(path).await;
            assert!(remaining.contains(&old), "cleanup disabled; file should survive");
        }

        #[tokio::test]
        async fn nonexistent_directory_is_noop() {
            let dir = TempDir::new().unwrap();
            let missing = dir.path().join("does-not-exist");
            remove_outdated_retry_files(&missing, 10).await;
        }
    }
}
