use std::collections::VecDeque;

use saluki_error::{generic_error, GenericError};
use serde::{de::DeserializeOwned, Serialize};
use tracing::{debug, info, warn};

mod persisted;
use self::persisted::PersistedQueue;
pub use self::persisted::{DiskUsageRetriever, DiskUsageRetrieverImpl, PersistedQueueArgs};

const DEFAULT_FLUSH_TO_DISK_MEM_RATIO: f64 = 0.5;

/// A container that holds events.
///
/// This trait is used as an incredibly generic way to expose the number of events within a "container," which we
/// loosely define to be anything that's holding events in some form. This is primarily used to track the number of
/// events dropped by `RetryQueue` (and `PersistedQueue`) when entries have to be dropped due to size limits.
pub trait EventContainer {
    /// Returns the number of events represented by this container.
    fn event_count(&self) -> u64;

    /// Returns the number of metric data points represented by this container.
    fn data_point_count(&self) -> u64 {
        0
    }
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

    /// Total number of metric data points represented by the dropped items.
    pub data_points_dropped: u64,
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
        self.data_points_dropped += other.data_points_dropped;
    }

    /// Tracks a single dropped item.
    pub fn track_dropped_item(&mut self, item: &dyn EventContainer) {
        self.items_dropped += 1;
        self.events_dropped += item.event_count();
        self.data_points_dropped += item.data_point_count();
    }
}

/// A queue for storing requests to be retried.
pub struct RetryQueue<T> {
    queue_name: String,
    pending: VecDeque<T>,
    persisted_pending: Option<PersistedQueue<T>>,
    total_in_memory_bytes: u64,
    max_in_memory_bytes: u64,
    flush_to_disk_mem_ratio: f64,
}

impl<T> RetryQueue<T>
where
    T: Retryable,
{
    /// Creates a new `RetryQueue` instance with the given name and maximum size.
    ///
    /// The queue will only hold as many entries as can fit within the given maximum size. If the queue is full, the
    /// oldest entries will be removed (or potentially persisted to disk, see
    /// [`with_disk_persistence`][Self::with_disk_persistence]) to make room for new entries.
    pub fn new(queue_name: String, max_in_memory_bytes: u64) -> Self {
        Self {
            queue_name,
            pending: VecDeque::new(),
            persisted_pending: None,
            total_in_memory_bytes: 0,
            max_in_memory_bytes,
            flush_to_disk_mem_ratio: DEFAULT_FLUSH_TO_DISK_MEM_RATIO,
        }
    }

    /// Configures the ratio of in-memory queue bytes to flush to disk when the queue is full.
    ///
    /// When disk persistence is enabled and the queue does not have enough room for a new entry, this ratio controls how
    /// much in-memory data is moved to disk. For example, a value of `0.5` moves at least half of
    /// `max_in_memory_bytes` to disk when the queue overflows. Values less than or equal to zero disable extra batch
    /// flushing, but entries evicted to make room are still persisted when disk persistence is enabled.
    pub fn with_flush_to_disk_mem_ratio(mut self, flush_to_disk_mem_ratio: f64) -> Self {
        self.flush_to_disk_mem_ratio = flush_to_disk_mem_ratio;
        self
    }

    /// Configures the queue to persist pending entries to disk.
    ///
    /// Disk persistence is used as a fallback to in-memory storage when the queue is full. When attempting to add a new
    /// entry to the queue, and the queue can't fit the entry in-memory, in-memory entries will be persisted to disk,
    /// oldest first.
    ///
    /// When reading entries from the queue, in-memory entries are read first, followed by persisted entries. This
    /// provides priority to the most recent entries added to the queue, but allows for bursting over the configured
    /// in-memory size limit without having to immediately discard entries.
    ///
    /// Files are stored in a subdirectory, with the same name as the given queue name, within `args.root_path`.
    ///
    /// # Errors
    ///
    /// If there is an error initializing the disk persistence layer, an error is returned.
    pub async fn with_disk_persistence(mut self, mut args: PersistedQueueArgs) -> Result<Self, GenericError> {
        // Make sure the root storage path is non-empty, as otherwise we can't generate a valid path
        // for the persisted entries in this retry queue.
        if args.root_path.as_os_str().is_empty() {
            return Err(generic_error!("Storage path cannot be empty."));
        }

        args.root_path = args.root_path.join(&self.queue_name);
        let mut persisted_pending = PersistedQueue::from_root_path(args).await?;
        match persisted_pending.remove_stale_files().await {
            Ok(removed) if removed > 0 => {
                info!(count = removed, "Removed outdated retry files from disk.");
            }
            Ok(_) => {}
            Err(e) => warn!(error = %e, "Failed to remove stale retry files."),
        }
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

    /// Returns the maximum in-memory capacity, in bytes.
    pub const fn max_in_memory_bytes(&self) -> u64 {
        self.max_in_memory_bytes
    }

    /// Returns the available in-memory capacity, in bytes.
    pub const fn available_in_memory_capacity_bytes(&self) -> u64 {
        self.max_in_memory_bytes.saturating_sub(self.total_in_memory_bytes)
    }

    /// Returns the available on-disk capacity, in bytes.
    ///
    /// Returns `0` when disk persistence is not enabled.
    ///
    /// # Errors
    ///
    /// If disk persistence is enabled and there is an error while retrieving the underlying disk capacity, an error is
    /// returned.
    pub async fn available_on_disk_capacity_bytes(&self) -> Result<u64, GenericError> {
        match &self.persisted_pending {
            Some(persisted_pending) => persisted_pending.available_capacity_bytes().await,
            None => Ok(0),
        }
    }

    /// Returns the number of persisted entries that have been permanently dropped due to errors since the last call
    /// to this method, resetting the counter.
    ///
    /// Always returns 0 if disk persistence isn't enabled.
    pub fn take_persisted_entries_dropped(&mut self) -> u64 {
        self.persisted_pending.as_mut().map_or(0, |p| p.take_entries_dropped())
    }

    /// Enqueues an entry.
    ///
    /// If the queue is full and the entry can't be enqueued in-memory, in-memory entries (oldest first) are evicted
    /// until there is room for the new entry. When disk persistence is enabled, evicted entries are moved to disk. If the
    /// flush-to-disk ratio is greater than zero, eviction moves at least
    /// `max_in_memory_bytes * flush_to_disk_mem_ratio` bytes of in-memory data to disk before admitting the new entry. If
    /// disk persistence is disabled, evicted entries are dropped instead. If an in-memory entry can't be persisted due to
    /// a disk error, that entry is dropped and counted in the returned `PushResult`; the new entry is still enqueued.
    ///
    /// # Errors
    ///
    /// If the entry is too large to fit into the queue, an error is returned.
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
        let required_bytes = self
            .total_in_memory_bytes
            .saturating_add(current_entry_size)
            .saturating_sub(self.max_in_memory_bytes);
        let using_disk = self.persisted_pending.is_some();
        let bytes_to_remove = if using_disk && required_bytes > 0 {
            required_bytes.max(flush_to_disk_bytes(
                self.max_in_memory_bytes,
                self.flush_to_disk_mem_ratio,
            ))
        } else {
            required_bytes
        };
        let mut bytes_removed = 0;

        while !self.pending.is_empty() && bytes_removed < bytes_to_remove {
            let oldest_entry = self.pending.pop_front().expect("queue is not empty");
            let oldest_entry_size = oldest_entry.size_bytes();

            if using_disk {
                // Capture the dropped-event counts before moving `oldest_entry` into the persist call, so we can still
                // record drop telemetry if the disk write fails.
                let oldest_entry_events = oldest_entry.event_count();
                let oldest_entry_data_points = oldest_entry.data_point_count();
                let persisted_pending = self.persisted_pending.as_mut().expect("disk persistence is enabled");
                match persisted_pending.push(oldest_entry).await {
                    Ok(persist_result) => {
                        push_result.merge(persist_result);
                        debug!(entry.len = oldest_entry_size, "Moved in-memory entry to disk.");
                    }
                    Err(e) => {
                        // Match the upstream Agent: on disk persistence failure, drop this entry and continue evicting
                        // so the new entry can still be admitted to the queue. Propagating the error here would
                        // permanently lose the incoming transaction at the caller, which the Agent does not do.
                        warn!(
                            error = %e,
                            entry.len = oldest_entry_size,
                            "Failed to persist in-memory entry to disk; dropping entry to make room."
                        );
                        push_result.items_dropped += 1;
                        push_result.events_dropped += oldest_entry_events;
                        push_result.data_points_dropped += oldest_entry_data_points;
                    }
                }
            } else {
                debug!(
                    entry.len = oldest_entry_size,
                    "Dropped in-memory entry to increase available capacity."
                );

                push_result.track_dropped_item(&oldest_entry);

                // Anchor the overflow-drop path: a prolonged outage saturates the queue and sheds the oldest entry
                // (bounded memory at the cost of counted data loss).
                saluki_antithesis::sometimes!(true, "retry queue dropped oldest in-memory entry on overflow");
            }

            self.total_in_memory_bytes -= oldest_entry_size;
            bytes_removed += oldest_entry_size;
        }

        self.pending.push_back(entry);
        self.total_in_memory_bytes += current_entry_size;

        // The eviction loop above guarantees we stay within the in-memory byte cap. Assert the invariant; numeric form
        // hands the search the headroom to the cap as a gradient.
        saluki_antithesis::always_le!(
            self.total_in_memory_bytes,
            self.max_in_memory_bytes,
            "retry queue in-memory bytes within cap",
            { "bytes": self.total_in_memory_bytes, "cap": self.max_in_memory_bytes }
        );

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
    /// the normal limiting behavior in terms of maximum on-disk size. When disk persistence isn't enabled, all
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

                push_result.track_dropped_item(&entry);
            }
        }

        Ok(push_result)
    }
}

fn flush_to_disk_bytes(max_in_memory_bytes: u64, flush_to_disk_mem_ratio: f64) -> u64 {
    if flush_to_disk_mem_ratio <= 0.0 || flush_to_disk_mem_ratio.is_nan() {
        0
    } else if flush_to_disk_mem_ratio.is_infinite() {
        u64::MAX
    } else {
        // Truncate toward zero to match the upstream Agent's `int(maxMemSizeInBytes * flushToStorageRatio)` semantics.
        ((max_in_memory_bytes as f64) * flush_to_disk_mem_ratio) as u64
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

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
    async fn capacity_accessors_report_memory_and_disk_capacity() {
        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();
        let mut retry_queue = RetryQueue::<FakeData>::new("test".to_string(), 36)
            .with_disk_persistence(PersistedQueueArgs {
                root_path: root_path.clone(),
                max_on_disk_bytes: 1024,
                storage_max_disk_ratio: 1.0,
                disk_usage_retriever: Arc::new(DiskUsageRetrieverImpl::new(root_path)),
                max_age_days: 10,
            })
            .await
            .expect("should not fail to create retry queue with disk persistence");

        assert_eq!(retry_queue.max_in_memory_bytes(), 36);
        assert_eq!(retry_queue.available_in_memory_capacity_bytes(), 36);
        assert_eq!(
            retry_queue
                .available_on_disk_capacity_bytes()
                .await
                .expect("should not fail to calculate disk capacity"),
            1024
        );

        let push_result = retry_queue
            .push(FakeData::random())
            .await
            .expect("first push should succeed");
        assert!(!push_result.had_drops());
        assert_eq!(retry_queue.available_in_memory_capacity_bytes(), 0);
        let push_result = retry_queue
            .push(FakeData::random())
            .await
            .expect("second push should persist the oldest entry");
        assert!(!push_result.had_drops());
        assert_eq!(retry_queue.available_in_memory_capacity_bytes(), 0);

        assert!(
            retry_queue
                .available_on_disk_capacity_bytes()
                .await
                .expect("should not fail to calculate disk capacity")
                < 1024
        );

        let _ = retry_queue.pop().await.expect("pop should succeed");
        assert_eq!(retry_queue.available_in_memory_capacity_bytes(), 36);
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
            .with_disk_persistence(PersistedQueueArgs {
                root_path: root_path.clone(),
                max_on_disk_bytes: u64::MAX,
                storage_max_disk_ratio: 1.0,
                disk_usage_retriever: Arc::new(DiskUsageRetrieverImpl::new(root_path.clone())),
                max_age_days: 10,
            })
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

    #[tokio::test]
    async fn disk_overflow_flushes_configured_memory_ratio() {
        let data1 = FakeData::random();
        let data2 = FakeData::random();
        let data3 = FakeData::random();
        let data4 = FakeData::random();

        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();

        let mut retry_queue = RetryQueue::<FakeData>::new("test".to_string(), 120)
            .with_flush_to_disk_mem_ratio(0.5)
            .with_disk_persistence(PersistedQueueArgs {
                root_path: root_path.clone(),
                max_on_disk_bytes: u64::MAX,
                storage_max_disk_ratio: 1.0,
                disk_usage_retriever: Arc::new(DiskUsageRetrieverImpl::new(root_path.clone())),
                max_age_days: 10,
            })
            .await
            .expect("should not fail to create retry queue with disk persistence");

        let push_result = retry_queue
            .push(data1.clone())
            .await
            .expect("should not fail to push data");
        assert_eq!(0, push_result.items_dropped);
        assert_eq!(0, push_result.events_dropped);
        let push_result = retry_queue
            .push(data2.clone())
            .await
            .expect("should not fail to push data");
        assert_eq!(0, push_result.items_dropped);
        assert_eq!(0, push_result.events_dropped);
        let push_result = retry_queue
            .push(data3.clone())
            .await
            .expect("should not fail to push data");
        assert_eq!(0, push_result.items_dropped);
        assert_eq!(0, push_result.events_dropped);

        let push_result = retry_queue
            .push(data4.clone())
            .await
            .expect("should not fail to push data");
        assert_eq!(0, push_result.items_dropped);
        assert_eq!(0, push_result.events_dropped);
        assert!(file_count_recursive(&root_path) >= 2);

        // In-memory entries are popped first (data3, data4), followed by the entries that were flushed to disk
        // (data1, data2 in FIFO order).
        let actual = retry_queue
            .pop()
            .await
            .expect("should not fail to pop data")
            .expect("should not be empty");
        assert_eq!(data3, actual);

        let actual = retry_queue
            .pop()
            .await
            .expect("should not fail to pop data")
            .expect("should not be empty");
        assert_eq!(data4, actual);

        let actual = retry_queue
            .pop()
            .await
            .expect("should not fail to pop data")
            .expect("should not be empty");
        assert_eq!(data1, actual);

        let actual = retry_queue
            .pop()
            .await
            .expect("should not fail to pop data")
            .expect("should not be empty");
        assert_eq!(data2, actual);
    }

    #[tokio::test]
    async fn zero_disk_flush_ratio_persists_required_entries() {
        let data1 = FakeData::random();
        let data2 = FakeData::random();
        let data3 = FakeData::random();

        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();

        let mut retry_queue = RetryQueue::<FakeData>::new("test".to_string(), 72)
            .with_flush_to_disk_mem_ratio(0.0)
            .with_disk_persistence(PersistedQueueArgs {
                root_path: root_path.clone(),
                max_on_disk_bytes: u64::MAX,
                storage_max_disk_ratio: 1.0,
                disk_usage_retriever: Arc::new(DiskUsageRetrieverImpl::new(root_path.clone())),
                max_age_days: 10,
            })
            .await
            .expect("should not fail to create retry queue with disk persistence");

        let push_result = retry_queue
            .push(data1.clone())
            .await
            .expect("should not fail to push data");
        assert_eq!(0, push_result.items_dropped);
        assert_eq!(0, push_result.events_dropped);
        let push_result = retry_queue
            .push(data2.clone())
            .await
            .expect("should not fail to push data");
        assert_eq!(0, push_result.items_dropped);
        assert_eq!(0, push_result.events_dropped);

        let push_result = retry_queue
            .push(data3.clone())
            .await
            .expect("should not fail to push data");
        assert_eq!(0, push_result.items_dropped);
        assert_eq!(0, push_result.events_dropped);
        assert_eq!(1, file_count_recursive(&root_path));

        let actual = retry_queue
            .pop()
            .await
            .expect("should not fail to pop data")
            .expect("should not be empty");
        assert_eq!(data2, actual);

        let actual = retry_queue
            .pop()
            .await
            .expect("should not fail to pop data")
            .expect("should not be empty");
        assert_eq!(data3, actual);

        let actual = retry_queue
            .pop()
            .await
            .expect("should not fail to pop data")
            .expect("should not be empty");
        assert_eq!(data1, actual);
    }
}
