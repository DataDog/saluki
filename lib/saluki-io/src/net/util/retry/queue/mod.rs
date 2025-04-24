#![allow(dead_code)]

mod persisted;
use std::{collections::VecDeque, path::PathBuf};

use persisted::DiskUsageRetrieverWrapper;
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
    /// Files are stored in a subdirectory, with the same name as the given queue name, within the given `root_path`.
    ///
    /// # Errors
    ///
    /// If there is an error initializing the disk persistence layer, an error is returned.
    pub async fn with_disk_persistence(
        mut self, root_path: PathBuf, max_disk_size_bytes: u64, storage_max_disk_ratio: f64,
        disk_usage_retriever: DiskUsageRetrieverWrapper,
    ) -> Result<Self, GenericError> {
        let queue_root_path = root_path.join(&self.queue_name);
        let persisted_pending = PersistedQueue::from_root_path(
            queue_root_path,
            max_disk_size_bytes,
            storage_max_disk_ratio,
            disk_usage_retriever,
        )
        .await?;
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

    /// Flushes all entries, potentially persisting them to disk.
    ///
    /// When disk persistence is configured, this will flush all in-memory entries to disk. Flushing to disk still obeys
    /// the the normal limiting behavior in terms of maximum on-disk size. When disk persistence is not enabled, all
    /// in-memory entries will be dropped.
    ///
    /// # Errors
    ///
    /// If an error occurs while persisting an entry to disk, an error is returned.
    pub async fn flush(mut self) -> Result<PushResult, GenericError> {
        let mut push_result = PushResult::default();

        while let Some(entry) = self.pending.pop_front() {
            let entry_size = entry.size_bytes();

            if let Some(persisted_pending) = &mut self.persisted_pending {
                let persist_result = persisted_pending.push(entry).await?;
                push_result.merge(persist_result);

                debug!(entry.len = entry_size, "Flushed in-memory entry to disk.");
            } else {
                debug!(entry.len = entry_size, "Dropped in-memory entry during flush.");

                push_result.track_dropped_item(entry.event_count());
            }
        }

        Ok(push_result)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rand::{distributions::Alphanumeric, Rng as _};
    use serde::Deserialize;

    use super::{persisted::DiskUsageRetriever, *};

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

    fn file_count_recursive<P: AsRef<Path>>(path: P) -> u64 {
        let mut count = 0;
        let entries = std::fs::read_dir(path).expect("should not fail to read directory");
        for maybe_entry in entries {
            let entry = maybe_entry.expect("should not fail to read directory entry");
            if entry.file_type().expect("should not fail to get file type").is_file() {
                count += 1;
            } else if entry.file_type().expect("should not fail to get file type").is_dir() {
                count += file_count_recursive(entry.path());
            }
        }
        count
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

    #[tokio::test]
    async fn flush_no_disk() {
        let data1 = FakeData::random();
        let data2 = FakeData::random();

        // Create our retry queue such that it can hold both items.
        let mut retry_queue = RetryQueue::<FakeData>::new("test".to_string(), u64::MAX);

        // Push our data to the queue.
        let push_result1 = retry_queue.push(data1).await.expect("should not fail to push data");
        assert_eq!(0, push_result1.items_dropped);
        assert_eq!(0, push_result1.events_dropped);
        let push_result2 = retry_queue.push(data2).await.expect("should not fail to push data");
        assert_eq!(0, push_result2.items_dropped);
        assert_eq!(0, push_result2.events_dropped);

        // Flush the queue, which should drop all entries as we have no disk persistence layer configured.
        let flush_result = retry_queue.flush().await.expect("should not fail to flush");
        assert_eq!(2, flush_result.items_dropped);
        assert_eq!(2, flush_result.events_dropped);
    }

    #[tokio::test]
    async fn flush_disk() {
        let data1 = FakeData::random();
        let data2 = FakeData::random();

        // Create our retry queue such that it can hold both items, and enable disk persistence.
        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();

        // Just a sanity check to ensure our temp directory is empty.
        assert_eq!(0, file_count_recursive(&root_path));

        let mut retry_queue = RetryQueue::<FakeData>::new("test".to_string(), u64::MAX)
            .with_disk_persistence(
                root_path.clone(),
                u64::MAX,
                0.8,
                DiskUsageRetrieverWrapper::new(root_path.clone()),
            )
            .await
            .expect("should not fail to create retry queue with disk persistence");

        // Push our data to the queue.
        let push_result1 = retry_queue.push(data1).await.expect("should not fail to push data");
        assert_eq!(0, push_result1.items_dropped);
        assert_eq!(0, push_result1.events_dropped);
        let push_result2 = retry_queue.push(data2).await.expect("should not fail to push data");
        assert_eq!(0, push_result2.items_dropped);
        assert_eq!(0, push_result2.events_dropped);

        // Flush the queue, which should push all entries to disk.
        let flush_result = retry_queue.flush().await.expect("should not fail to flush");
        assert_eq!(0, flush_result.items_dropped);
        assert_eq!(0, flush_result.events_dropped);

        // We should now have two files on disk after flushing.
        assert_eq!(2, file_count_recursive(&root_path));
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

    #[tokio::test]
    async fn disk_usage_retriever() {
        let data1 = FakeData::random();
        let data2 = FakeData::random();

        // Create our retry queue such that it can hold one item, and enable disk persistence.
        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();

        // Just a sanity check to ensure our temp directory is empty.
        assert_eq!(0, file_count_recursive(&root_path));

        // With the storage_max_disk_ratio set to 0.35 and with the `MockDiskUsageRetriever` returning 100 for both
        // total and available space on every call, the available disk usage is 35 bytes.
        let mut retry_queue = RetryQueue::<FakeData>::new("test".to_string(), u64::MAX)
            .with_disk_persistence(
                root_path.clone(),
                80,
                0.35,
                DiskUsageRetrieverWrapper::new_with_disk_usage_retriever(Box::new(MockDiskUsageRetriever {})),
            )
            .await
            .expect("should not fail to create retry queue with disk persistence");

        // Push our data to the queue.
        let push_result1 = retry_queue.push(data1).await.expect("should not fail to push data");
        assert_eq!(0, push_result1.items_dropped);
        assert_eq!(0, push_result1.events_dropped);
        let push_result2 = retry_queue.push(data2).await.expect("should not fail to push data");
        assert_eq!(0, push_result2.items_dropped);
        assert_eq!(0, push_result2.events_dropped);

        // The `storage_max_disk_ratio` is 0.35, and our `MockDiskUsageRetriever` returns 100 for both `total_space` and
        // `available_space`, so `on_disk_bytes_limit()` returns min(80, 35) = 35.
        //
        // First entry: total_on_disk_bytes(0) + required_bytes(30) < on_disk_bytes_limit(35)
        // Second entry: total_on_disk_bytes(30) + required_bytes(30) > on_disk_bytes_limit(35) so the first entry is dropped.
        let flush_result = retry_queue.flush().await.expect("should not fail to flush");
        assert_eq!(1, flush_result.items_dropped);
        assert_eq!(1, flush_result.events_dropped);

        // We should now have one file on disk.
        assert_eq!(1, file_count_recursive(&root_path));
    }
}
