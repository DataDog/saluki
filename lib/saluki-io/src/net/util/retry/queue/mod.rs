#![allow(dead_code)]

mod persisted;
use std::{collections::VecDeque, path::PathBuf};

use saluki_error::{generic_error, GenericError};
use serde::{de::DeserializeOwned, Serialize};
use tracing::debug;

use self::persisted::PersistedQueue;

/// A value that can be retried.
pub trait Retryable: DeserializeOwned + Serialize {
    /// Returns the in-memory size of this value, in bytes.
    fn size_bytes(&self) -> u64;
}

pub struct RetryQueue<T> {
    queue_name: String,
    pending: VecDeque<T>,
    persisted_pending: Option<PersistedQueue<T>>,
    max_size_bytes: u64,
}

impl<T> RetryQueue<T>
where
    T: Retryable,
{
    /// Creates a new `RetryQueue` instance with the given name and maximum size.
    ///
    /// The queue will only hold as many entries as can fit within the given maximum size. If the queue is full, the
    /// oldest entries will be removed (or potentially persisted to disk, see [`with_disk_persistence`]) to make room
    /// for new entries.
    pub fn new(queue_name: String, max_size_bytes: u64) -> Self {
        Self {
            queue_name,
            pending: VecDeque::new(),
            persisted_pending: None,
            max_size_bytes,
        }
    }

    /// Configures the queue to persist pending entries to disk.
    ///
    /// Disk persistence is used as a fallback to in-memory storage when the queue is full. When attempting to add a new
    /// entry to the queue, and the queue cannot fit the entry in-memory, in-memory entries will be persisted to disk,
    /// oldest first.
    ///
    /// When reading entries from the queue, in-memory entries are read first, followed by persisted entries. This
    /// provides priority to the most recent entries added to the queue, but allows for bursting over the configured
    /// in-memory size limit without having to immediately discard entries.
    ///
    /// # Errors
    ///
    /// If there is an error initializing the disk persistence layer, an error is returned.
    pub async fn with_disk_persistence(
        mut self, root_path: PathBuf, max_disk_size_bytes: u64,
    ) -> Result<Self, GenericError> {
        let named_root_path = root_path.join(&self.queue_name);
        let persisted_pending = PersistedQueue::from_root_path(named_root_path, max_disk_size_bytes).await?;
        self.persisted_pending = Some(persisted_pending);
        Ok(self)
    }

    /// Enqueues an entry.
    ///
    /// If the queue is full and the entry cannot be enqueue in-memory, and disk persistence is enabled, in-memory
    /// entries will be moved to disk (oldest first) until enough capacity is available to enqueue the new entry
    /// in-memory.
    ///
    /// # Errors
    ///
    /// If the entry is too large to fit into the queue, or if there is an error when persisting entries to disk, an
    /// error is returned.
    pub async fn push(&mut self, entry: T) -> Result<(), GenericError> {
        // Make sure the entry, by itself, isn't too big to ever fit into the queue.
        let current_entry_size = entry.size_bytes();
        if current_entry_size > self.max_size_bytes {
            return Err(generic_error!(
                "Entry too large to fit into retry queue. ({} > {})",
                current_entry_size,
                self.max_size_bytes
            ));
        }

        // Make sure we have enough room for this incoming entry, either by persisting older entries to disk or by
        // simply dropping them.
        let mut total_in_memory_bytes = self.pending.iter().map(|entry| entry.size_bytes()).sum::<u64>();
        while total_in_memory_bytes + current_entry_size > self.max_size_bytes {
            if let Some(persisted_pending) = &mut self.persisted_pending {
                let oldest_entry = self.pending.pop_front().unwrap();
                let oldest_entry_size = oldest_entry.size_bytes();

                persisted_pending.push(oldest_entry).await?;

                debug!(entry_size_bytes = oldest_entry_size, "Moved in-memory entry to disk.");
                total_in_memory_bytes -= oldest_entry_size;
            } else {
                self.pending.pop_front();

                // TODO: Log that we're dropping an entry on the floor.
            }
        }

        self.pending.push_back(entry);
        debug!(entry_size_bytes = current_entry_size, "Enqueued entry in-memory.");

        Ok(())
    }

    /// Consumes an entry.
    ///
    /// In-memory entries are consumed first, followed by persisted entries if disk persistence is enabled.
    ///
    /// If no entries are available, `None` is returned.
    ///
    /// # Errors
    ///
    /// If there is an error when consuming an entry from disk, whether due to reading or deserializing the entry, an
    /// error is returned.
    pub async fn pop(&mut self) -> Result<Option<T>, GenericError> {
        // Pull from in-memory first to prioritize the most recent entries.
        if let Some(entry) = self.pending.pop_front() {
            debug!(entry_size_bytes = entry.size_bytes(), "Dequeued entry in-memory.");
            return Ok(Some(entry));
        }

        // If we have disk persistence enabled, pull from disk next.
        if let Some(persisted_pending) = &mut self.persisted_pending {
            if let Some(entry) = persisted_pending.pop().await? {
                debug!(entry_size_bytes = entry.size_bytes(), "Dequeued entry from disk.");
                return Ok(Some(entry));
            }
        }

        Ok(None)
    }
}
