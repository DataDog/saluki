use std::{
    io,
    marker::PhantomData,
    path::{Path, PathBuf},
};

use chrono::{NaiveDateTime, Utc};
use rand::Rng;
use saluki_error::{ErrorContext as _, GenericError};
use serde::{de::DeserializeOwned, Serialize};
use tracing::warn;

/// A persisted entry.
///
/// Represents the high-level metadata of a persisted entry, including the path to and size of the entry.
struct PersistedEntry {
    path: PathBuf,
    timestamp: u64,
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

    fn from_parts(path: PathBuf, timestamp: u64, size_bytes: u64) -> Self {
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
    max_size_bytes: u64,
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
    pub async fn from_root_path(root_path: PathBuf, max_size_bytes: u64) -> Result<Self, GenericError> {
        // Make sure the directory exists first.
        tokio::fs::create_dir_all(&root_path)
            .await
            .with_error_context(|| format!("Failed to create retry directory '{}'.", root_path.display()))?;

        let mut persisted_requests = Self {
            root_path,
            entries: Vec::new(),
            max_size_bytes,
            _entry: PhantomData,
        };
        persisted_requests.refresh_entry_state().await?;

        Ok(persisted_requests)
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
            let serialized = match tokio::fs::read(&entry.path).await {
                Ok(serialized) => serialized,
                Err(e) => match e.kind() {
                    io::ErrorKind::NotFound => {
                        // We tried to delete an entry that no longer exists on disk, which means our internal entry state
                        // is corrupted for some reason. We'll refresh our entry state and then try again.
                        self.refresh_entry_state().await?;
                        continue;
                    }
                    _ => {
                        return Err(e).with_error_context(|| {
                            format!("Failed to read persisted entry '{}'.", entry.path.display())
                        })
                    }
                },
            };

            let deserialized = serde_json::from_slice(&serialized)
                .with_error_context(|| format!("Failed to deserialize persisted entry '{}'.", entry.path.display()))?;

            return Ok(Some(deserialized));
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
        let mut total_size_bytes = self.entries.iter().map(|entry| entry.size_bytes).sum::<u64>();
        while total_size_bytes + required_bytes > self.max_size_bytes {
            let entry = self.entries.remove(0);
            tokio::fs::remove_file(&entry.path).await?;

            total_size_bytes -= entry.size_bytes;

            // TODO: Log that we're dropping an entry on the floor.
        }

        Ok(())
    }
}

fn generate_timestamped_filename() -> (PathBuf, u64) {
    let now = Utc::now();
    let now_ts = now.timestamp() as u64;
    let nonce = rand::thread_rng().gen_range(100000000..999999999);

    let filename = format!("retry-{}-{}.json", now.format("%Y%m%d%H%M%S"), nonce).into();

    (filename, now_ts)
}

fn decode_timestamped_filename(path: &Path) -> Option<u64> {
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
    NaiveDateTime::parse_from_str(timestamp_str, "%Y%m%d%H%M%S")
        .map(|dt| dt.and_utc().timestamp() as u64)
        .ok()
}
