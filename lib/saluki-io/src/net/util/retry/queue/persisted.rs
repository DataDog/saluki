use std::{
    io,
    marker::PhantomData,
    path::{Path, PathBuf},
};

use chrono::{DateTime, NaiveDateTime, Utc};
use rand::Rng;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::{de::DeserializeOwned, Serialize};
use tracing::{debug, warn};

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
    /// If the given path is not recognized as the path to a valid persisted entry, `None` is returned.
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

pub struct PersistedQueue<T> {
    root_path: PathBuf,
    entries: Vec<PersistedEntry>,
    total_on_disk_bytes: u64,
    max_on_disk_bytes: u64,
    _entry: PhantomData<T>,
}

impl<T> PersistedQueue<T>
where
    T: DeserializeOwned + Serialize,
{
    /// Creates a new `PersistedQueue` instance from the given root path and maximum size.
    ///
    /// The root path is created if it does not already exist, and is scanned for existing persisted entries. Entries
    /// are removed (oldest first) until the total size of all scanned entries is within the given maximum size.
    ///
    /// # Errors
    ///
    /// If there is an error creating the root directory, or scanning it for existing entries, or deleting entries to
    /// shrink the directory to fit the given maximum size, an error is returned.
    pub async fn from_root_path(root_path: PathBuf, max_on_disk_bytes: u64) -> Result<Self, GenericError> {
        // Make sure the directory exists first.
        create_directory_recursive(root_path.clone())
            .await
            .error_context("Failed to create retry directory.")?;

        let mut persisted_requests = Self {
            root_path,
            entries: Vec::new(),
            total_on_disk_bytes: 0,
            max_on_disk_bytes,
            _entry: PhantomData,
        };
        persisted_requests.refresh_entry_state().await?;

        Ok(persisted_requests)
    }

    /// Returns `true` if the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Enqueues an entry and persists it to disk.
    ///
    /// # Errors
    ///
    /// If there is an error serializing the entry, or writing it to disk, or removing older entries to make space for
    /// the new entry, an error is returned.
    pub async fn push(&mut self, entry: T) -> Result<(), GenericError> {
        // Serialize the entry to a temporary file.
        let (filename, timestamp) = generate_timestamped_filename();
        let entry_path = self.root_path.join(filename);
        let serialized = serde_json::to_vec(&entry)
            .with_error_context(|| format!("Failed to serialize entry for '{}'.", entry_path.display()))?;

        if serialized.len() as u64 > self.max_on_disk_bytes {
            return Err(generic_error!("Entry is too large to persist."));
        }

        // Make sure we have enough space to persist the entry.
        self.remove_until_available_space(serialized.len() as u64)
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

        Ok(())
    }

    /// Consumes the oldest persisted entry on disk, if one exists.
    ///
    /// # Errors
    ///
    /// If there is an error reading or deserializing the entry, an error is returned.
    pub async fn pop(&mut self) -> Result<Option<T>, GenericError> {
        if self.entries.is_empty() {
            return Ok(None);
        }

        loop {
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
                    // We couldn't read the file, so add it back to our entries list and return the error.
                    self.entries.insert(0, entry);
                    return Err(e);
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

        // Sort the entries by their inherent timestamp, and then do any necessary trimming to ensure we're within our
        // configured maximum size bound.
        entries.sort_by_key(|entry| entry.timestamp);
        self.total_on_disk_bytes = entries.iter().map(|entry| entry.size_bytes).sum();
        self.entries = entries;

        self.ensure_within_size_limit().await?;

        Ok(())
    }

    async fn ensure_within_size_limit(&mut self) -> io::Result<()> {
        // We just use `remove_until_available_space` with a value of 0 to remove entries until we're within the
        // configured maximum size.
        self.remove_until_available_space(0).await
    }

    /// Removes persisted entries (oldest first) until there is at least the required number of bytes in free space
    /// (maximum - total).
    ///
    /// # Errors
    ///
    /// If there is an error while deleting persisted entries, an error is returned.
    async fn remove_until_available_space(&mut self, required_bytes: u64) -> io::Result<()> {
        while !self.entries.is_empty() && self.total_on_disk_bytes + required_bytes > self.max_on_disk_bytes {
            let entry = self.entries.remove(0);

            match tokio::fs::remove_file(&entry.path).await {
                Ok(_) => {
                    self.total_on_disk_bytes -= entry.size_bytes;
                    debug!(entry.path = %entry.path.display(), entry.len = entry.size_bytes, "Dropped persisted entry.");
                }
                Err(e) => {
                    // We didn't delete the file, so add it back to our entries list and return the error.
                    self.entries.insert(0, entry);
                    return Err(e);
                }
            }
        }

        Ok(())
    }
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

    let deserialized = serde_json::from_slice(&serialized)
        .with_error_context(|| format!("Failed to deserialize persisted entry '{}'.", entry.path.display()))?;

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
    let nonce = rand::thread_rng().gen_range(100000000..999999999);

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

#[cfg(test)]
mod tests {
    use rand::distributions::Alphanumeric;
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
                name: rand::thread_rng()
                    .sample_iter(&Alphanumeric)
                    .take(8)
                    .map(char::from)
                    .collect(),
                value: rand::thread_rng().gen_range(0..100),
            }
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

        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(root_path.clone(), 1024)
            .await
            .expect("should not fail to create persisted queue");

        // Ensure the directory is empty.
        assert_eq!(0, files_in_dir(&root_path).await);

        // Push our data to the queue and ensure it persisted it to disk.
        persisted_queue
            .push(data.clone())
            .await
            .expect("should not fail to push data");
        assert_eq!(1, files_in_dir(&root_path).await);

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

        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(root_path.clone(), 1)
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

        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(root_path.clone(), 32)
            .await
            .expect("should not fail to create persisted queue");

        // Ensure the directory is empty.
        assert_eq!(0, files_in_dir(&root_path).await);

        // Push our data to the queue and ensure it persisted it to disk.
        persisted_queue.push(data1).await.expect("should not fail to push data");
        assert_eq!(1, files_in_dir(&root_path).await);

        // Push a second data entry, which should cause the first entry to be removed.
        persisted_queue
            .push(data2.clone())
            .await
            .expect("should not fail to push data");
        assert_eq!(1, files_in_dir(&root_path).await);

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
    async fn trim_excess_entries_on_create() {
        let data1 = FakeData::random();
        let data2 = FakeData::random();
        let data3 = FakeData::random();

        // Create our temporary directory and point our persisted queue at it.
        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();

        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(root_path.clone(), 1024)
            .await
            .expect("should not fail to create persisted queue");

        // Ensure the directory is empty.
        assert_eq!(0, files_in_dir(&root_path).await);

        // Push all our data to the queue.
        persisted_queue.push(data1).await.expect("should not fail to push data");
        persisted_queue
            .push(data2.clone())
            .await
            .expect("should not fail to push data");
        persisted_queue
            .push(data3.clone())
            .await
            .expect("should not fail to push data");
        assert_eq!(3, files_in_dir(&root_path).await);

        // Now recreate the persisted queue with a smaller size, which should cause the oldest entry to be removed, as
        // we only have room for two entries based on our reduced maximum size.
        drop(persisted_queue);

        let mut persisted_queue = PersistedQueue::<FakeData>::from_root_path(root_path.clone(), 64)
            .await
            .expect("should not fail to create persisted queue");

        // Ensure we only have two entries left, and that the oldest entry was removed.
        assert_eq!(2, files_in_dir(&root_path).await);

        let actual = persisted_queue
            .pop()
            .await
            .expect("should not fail to pop data")
            .expect("should not be empty");
        assert_eq!(data2, actual);

        assert_eq!(1, files_in_dir(&root_path).await);

        let actual = persisted_queue
            .pop()
            .await
            .expect("should not fail to pop data")
            .expect("should not be empty");
        assert_eq!(data3, actual);

        assert_eq!(0, files_in_dir(&root_path).await);
    }
}
