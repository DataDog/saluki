#![allow(dead_code)]

mod persisted;
use std::{collections::VecDeque, path::PathBuf};

use saluki_error::{generic_error, GenericError};
use serde::{de::DeserializeOwned, Serialize};
use tracing::debug;

use self::persisted::PersistedQueue;

/// A container that holds events.
///
/// This trait is used as an incredibly generic way to expose the number of events within a "container", which we
/// loosely define to be anything that is holding events in some form. This is primarily used to track the number of
/// events dropped by `RetryQueue` (and `PersistedQueue`) when entries have to be dropped due to size limits.
pub trait EventContainer {
    /// Returns the number of events represented by this container.
    fn event_count(&self) -> u64;
}

/// A value that can be retried.
pub trait Retryable: EventContainer + DeserializeOwned + Serialize {
    /// Returns the in-memory size of this value, in bytes.
    fn size_bytes(&self) -> u64;
}

impl EventContainer for String {
    fn event_count(&self) -> u64 {
        1
    }
}

impl Retryable for String {
    fn size_bytes(&self) -> u64 {
        self.len() as u64
    }
}

/// Result of a push operation.
///
/// As pushing items to `RetryQueue` may result in dropping older items to make room for new ones, this struct tracks
/// the total number of items dropped, and the number of events represented by those items.
#[derive(Default)]
#[must_use = "`PushResult` carries information about potentially dropped items/events and should not be ignored"]
pub struct PushResult {
    /// Total number of items dropped.
    pub items_dropped: u64,

    /// Total number of events represented by the dropped items.
    pub events_dropped: u64,
}

impl PushResult {
    /// Returns `true` if any items were dropped.
    pub fn had_drops(&self) -> bool {
        self.items_dropped > 0
    }

    /// Merges `other` into `Self`.
    pub fn merge(&mut self, other: Self) {
        self.items_dropped += other.items_dropped;
        self.events_dropped += other.events_dropped;
    }

    /// Tracks a single dropped item.
    pub fn track_dropped_item(&mut self, event_count: u64) {
        self.items_dropped += 1;
        self.events_dropped += event_count;
    }
}

/// A queue for storing requests to be retried.
pub struct RetryQueue<T> {
    queue_name: String,
    pending: VecDeque<T>,
    persisted_pending: Option<PersistedQueue<T>>,
    total_in_memory_bytes: u64,
    max_in_memory_bytes: u64,
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
    pub fn new(queue_name: String, max_in_memory_bytes: u64) -> Self {
        Self {
            queue_name,
            pending: VecDeque::new(),
            persisted_pending: None,
            total_in_memory_bytes: 0,
            max_in_memory_bytes,
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
        let persisted_pending = PersistedQueue::from_root_path(root_path, max_disk_size_bytes).await?;
        self.persisted_pending = Some(persisted_pending);
        Ok(self)
    }

    /// Returns `true` if the queue is empty.
    ///
    /// This includes both in-memory and persisted entries.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty() && self.persisted_pending.as_ref().is_none_or(|p| p.is_empty())
    }

    /// Returns the number of entries in the queue
    ///
    /// This includes both in-memory and persisted entries.
    pub fn len(&self) -> usize {
        self.pending.len() + self.persisted_pending.as_ref().map_or(0, |p| p.len())
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
    pub async fn push(&mut self, entry: T) -> Result<PushResult, GenericError> {
        let mut push_result = PushResult::default();

        // Make sure the entry, by itself, isn't too big to ever fit into the queue.
        let current_entry_size = entry.size_bytes();
        if current_entry_size > self.max_in_memory_bytes {
            return Err(generic_error!(
                "Entry too large to fit into retry queue. ({} > {})",
                current_entry_size,
                self.max_in_memory_bytes
            ));
        }

        // Make sure we have enough room for this incoming entry, either by persisting older entries to disk or by
        // simply dropping them.
        while !self.pending.is_empty() && self.total_in_memory_bytes + current_entry_size > self.max_in_memory_bytes {
            let oldest_entry = self.pending.pop_front().expect("queue is not empty");
            let oldest_entry_size = oldest_entry.size_bytes();

            if let Some(persisted_pending) = &mut self.persisted_pending {
                let persist_result = persisted_pending.push(oldest_entry).await?;
                push_result.merge(persist_result);

                debug!(entry.len = oldest_entry_size, "Moved in-memory entry to disk.");
            } else {
                debug!(
                    entry.len = oldest_entry_size,
                    "Dropped in-memory entry to increase available capacity."
                );

                push_result.track_dropped_item(oldest_entry.event_count());
            }

            self.total_in_memory_bytes -= oldest_entry_size;
        }

        self.pending.push_back(entry);
        self.total_in_memory_bytes += current_entry_size;
        debug!(entry.len = current_entry_size, "Enqueued in-memory entry.");

        Ok(push_result)
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
            self.total_in_memory_bytes -= entry.size_bytes();
            debug!(entry.len = entry.size_bytes(), "Dequeued in-memory entry.");

            return Ok(Some(entry));
        }

        // If we have disk persistence enabled, pull from disk next.
        if let Some(persisted_pending) = &mut self.persisted_pending {
            if let Some(entry) = persisted_pending.pop().await? {
                return Ok(Some(entry));
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use rand::{distributions::Alphanumeric, Rng as _};
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

    impl EventContainer for FakeData {
        fn event_count(&self) -> u64 {
            1
        }
    }

    impl Retryable for FakeData {
        fn size_bytes(&self) -> u64 {
            (self.name.len() + std::mem::size_of::<String>() + 4) as u64
        }
    }

    #[tokio::test]
    async fn basic_push_pop() {
        let data = FakeData::random();

        let mut retry_queue = RetryQueue::<FakeData>::new("test".to_string(), 1024);

        // Push our data to the queue.
        let push_result = retry_queue
            .push(data.clone())
            .await
            .expect("should not fail to push data");
        assert_eq!(0, push_result.items_dropped);
        assert_eq!(0, push_result.events_dropped);

        // Now pop the data back out and ensure it matches what we pushed, and that the file has been removed from disk.
        let actual = retry_queue
            .pop()
            .await
            .expect("should not fail to pop data")
            .expect("should not be empty");
        assert_eq!(data, actual);
    }

    #[tokio::test]
    async fn entry_too_large() {
        let data = FakeData::random();

        let mut retry_queue = RetryQueue::<FakeData>::new("test".to_string(), 1);

        // Attempt to push our data into the queue, which should fail because it's too large.
        assert!(retry_queue.push(data).await.is_err());
    }

    #[tokio::test]
    async fn remove_oldest_entry_on_push() {
        let data1 = FakeData::random();
        let data2 = FakeData::random();

        // Create our retry queue such that it is sized to only fit one entry at a time.
        let mut retry_queue = RetryQueue::<FakeData>::new("test".to_string(), 36);

        // Push our data to the queue.
        let push_result = retry_queue.push(data1).await.expect("should not fail to push data");
        assert_eq!(0, push_result.items_dropped);
        assert_eq!(0, push_result.events_dropped);

        // Push a second data entry, which should cause the first entry to be removed.
        let push_result = retry_queue
            .push(data2.clone())
            .await
            .expect("should not fail to push data");
        assert_eq!(1, push_result.items_dropped);
        assert_eq!(1, push_result.events_dropped);

        // Now pop the data back out and ensure it matches the second item we pushed, indicating the first item was
        // removed from the queue to make room.
        let actual = retry_queue
            .pop()
            .await
            .expect("should not fail to pop data")
            .expect("should not be empty");
        assert_eq!(data2, actual);
    }
}
