use std::{collections::VecDeque, fmt};

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
/// the total number of items dropped, and the number of events represented by those items, as well as the number of
/// items moved from memory to disk.
#[derive(Debug, Default)]
#[must_use = "`PushResult` carries information about potentially dropped items/events and should not be ignored"]
pub struct PushResult {
    /// Total number of items dropped.
    pub items_dropped: u64,

    /// Total number of events represented by the dropped items.
    pub events_dropped: u64,

    /// Total number of metric data points represented by the dropped items.
    pub data_points_dropped: u64,

    /// Total number of in-memory items written to disk.
    pub items_persisted: u64,
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
        self.items_persisted += other.items_persisted;
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
    /// Opening the subdirectory is not read-only, though it reads no file: it creates the subdirectory if it is
    /// missing, lists the retry files in it as entries by their names and sizes, and removes the files whose names are
    /// older than the age limit. Files beyond the size limit stay until an entry is next persisted, which removes the
    /// oldest until the new one fits. Two queues must therefore never be opened on one subdirectory at once.
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

    /// Changes the in-memory bound while the queue is in use.
    ///
    /// A smaller bound evicts in-memory entries, oldest first, until the rest fit, as an overflowing push does: to disk
    /// when disk persistence is enabled, and otherwise dropped, each counted in the returned `PushResult`. A larger
    /// bound admits more from the next push. Either way, a push of an entry larger than the bound is refused.
    pub async fn set_max_size_bytes(&mut self, max_in_memory_bytes: u64) -> PushResult {
        self.max_in_memory_bytes = max_in_memory_bytes;
        let excess_bytes = self.total_in_memory_bytes.saturating_sub(max_in_memory_bytes);
        self.evict_oldest(excess_bytes).await
    }

    /// Changes the ratio of in-memory queue bytes moved to disk when the queue overflows, while the queue is in use.
    ///
    /// See [`with_flush_to_disk_mem_ratio`][Self::with_flush_to_disk_mem_ratio].
    pub fn set_flush_to_disk_mem_ratio(&mut self, flush_to_disk_mem_ratio: f64) {
        self.flush_to_disk_mem_ratio = flush_to_disk_mem_ratio;
    }

    /// Changes the disk limits of a queue with disk persistence while it is in use.
    ///
    /// A smaller limit removes persisted entries, oldest first, until the rest fit, each counted in the returned
    /// `PushResult`; a larger one admits more from the next push. Disk persistence itself cannot be turned on or off
    /// this way, since opening a directory is a step of its own: the queue must be created again.
    ///
    /// # Errors
    ///
    /// If the queue has no disk persistence, or `max_on_disk_bytes` is zero, an error is returned and nothing changes.
    /// If the disk's usage cannot be read, or an entry cannot be removed, an error is returned, and the new limits
    /// apply from the next push.
    pub async fn set_disk_limits(
        &mut self, max_on_disk_bytes: u64, storage_max_disk_ratio: f64,
    ) -> Result<PushResult, GenericError> {
        if max_on_disk_bytes == 0 {
            return Err(generic_error!(
                "A disk limit of zero turns disk persistence off, which needs the retry queue to be created again."
            ));
        }
        let Some(persisted_pending) = self.persisted_pending.as_mut() else {
            return Err(generic_error!(
                "The retry queue has no disk persistence to limit. Turning it on needs the queue to be created again."
            ));
        };

        persisted_pending
            .set_limits(max_on_disk_bytes, storage_max_disk_ratio)
            .await
    }

    /// Returns `true` if the queue persists entries to disk.
    pub const fn has_disk_persistence(&self) -> bool {
        self.persisted_pending.is_some()
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
        let push_result = self.evict_oldest(bytes_to_remove).await;

        self.pending.push_back(entry);
        self.total_in_memory_bytes += current_entry_size;

        // The eviction above guarantees we stay within the in-memory byte cap. Assert the invariant; numeric form hands
        // the search the headroom to the cap as a gradient.
        saluki_antithesis::always_le!(
            self.total_in_memory_bytes,
            self.max_in_memory_bytes,
            "retry queue in-memory bytes within cap",
            { "bytes": self.total_in_memory_bytes, "cap": self.max_in_memory_bytes }
        );

        debug!(entry.len = current_entry_size, "Enqueued in-memory entry.");

        Ok(push_result)
    }

    /// Evicts in-memory entries, oldest first, until at least `bytes_to_remove` bytes are evicted or none is left.
    ///
    /// With disk persistence, evicted entries are moved to disk; an entry that cannot be persisted is dropped. Without
    /// it, they are dropped. The returned `PushResult` counts both.
    async fn evict_oldest(&mut self, bytes_to_remove: u64) -> PushResult {
        let mut push_result = PushResult::default();
        let using_disk = self.persisted_pending.is_some();
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
                        push_result.items_persisted += 1;
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

        push_result
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
    /// An entry that cannot be persisted is dropped, and the flush goes on with the next one, so that one entry the
    /// disk refuses does not lose those after it. Every entry is counted in the returned `PushResult`, as persisted or
    /// as dropped.
    ///
    /// # Errors
    ///
    /// If an entry cannot be persisted, the first such error is returned, with what the flush did.
    pub async fn flush(mut self) -> Result<PushResult, FlushError> {
        let mut push_result = PushResult::default();
        let mut first_error = None;

        while let Some(entry) = self.pending.pop_front() {
            let entry_size = entry.size_bytes();

            if let Some(persisted_pending) = &mut self.persisted_pending {
                let mut unpersisted = PushResult::default();
                unpersisted.track_dropped_item(&entry);
                match persisted_pending.push(entry).await {
                    Ok(persist_result) => {
                        push_result.merge(persist_result);
                        push_result.items_persisted += 1;

                        debug!(entry.len = entry_size, "Flushed in-memory entry to disk.");
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            entry.len = entry_size,
                            "Failed to persist in-memory entry to disk during flush; dropping entry."
                        );
                        push_result.merge(unpersisted);
                        first_error.get_or_insert(e);
                    }
                }
            } else {
                debug!(entry.len = entry_size, "Dropped in-memory entry during flush.");

                push_result.track_dropped_item(&entry);
            }
        }

        match first_error {
            None => Ok(push_result),
            Some(error) => Err(FlushError {
                error,
                result: push_result,
            }),
        }
    }
}

/// The error of a flush that could not keep every entry: the first error, with what the flush did.
///
/// The flush went on past each entry it could not keep, dropping it, so `result` counts every entry, as persisted or
/// as dropped.
#[derive(Debug)]
pub struct FlushError {
    /// The first error the flush met.
    pub error: GenericError,

    /// What the flush did: the entries it persisted, and those it dropped, the ones it could not keep included.
    pub result: PushResult,
}

impl fmt::Display for FlushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.error)
    }
}

impl std::error::Error for FlushError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.source()
    }
}

impl<T> RetryQueue<T>
where
    T: EventContainer,
{
    /// Drops every in-memory entry without persisting it, counting each in the returned `PushResult`.
    ///
    /// Entries already persisted stay on disk, where the next queue opened on the same directory finds them.
    pub fn discard(self) -> PushResult {
        let mut push_result = PushResult::default();
        for entry in &self.pending {
            push_result.track_dropped_item(entry);
        }

        push_result
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
        assert_eq!(0, flush_result.items_persisted);
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
        assert_eq!(2, flush_result.items_persisted);
        assert_eq!(2, file_count_recursive(&root_path));
    }

    #[tokio::test]
    async fn a_flush_persists_the_entries_after_one_it_cannot_persist_and_counts_that_one_as_dropped() {
        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let root_path = temp_dir.path().to_path_buf();
        let mut retry_queue = RetryQueue::<FakeData>::new("test".to_string(), u64::MAX)
            .with_disk_persistence(PersistedQueueArgs {
                root_path: root_path.clone(),
                max_on_disk_bytes: 1024,
                storage_max_disk_ratio: 1.0,
                disk_usage_retriever: Arc::new(DiskUsageRetrieverImpl::new(root_path.clone())),
                max_age_days: 10,
            })
            .await
            .expect("should not fail to create retry queue with disk persistence");

        // The middle entry serializes to more than the disk limit, so it alone cannot be persisted.
        let too_large = FakeData {
            name: "x".repeat(2048),
            value: 0,
        };
        for entry in [FakeData::random(), too_large, FakeData::random()] {
            let push_result = retry_queue.push(entry).await.expect("the in-memory queue has room");
            assert!(!push_result.had_drops());
        }

        let FlushError { error, result } = retry_queue
            .flush()
            .await
            .expect_err("the flush reports the entry it could not persist");
        assert!(error.to_string().contains("Entry is too large to persist"), "{error}");
        assert_eq!(
            (result.items_persisted, result.items_dropped, result.events_dropped),
            (2, 1, 1)
        );
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

    /// Opens a retry queue named `test` under `root_path`, as a forwarder's endpoint loop does.
    async fn open_disk_queue(
        root_path: &Path, max_in_memory_bytes: u64, max_on_disk_bytes: u64,
    ) -> RetryQueue<FakeData> {
        RetryQueue::<FakeData>::new("test".to_string(), max_in_memory_bytes)
            .with_disk_persistence(PersistedQueueArgs {
                root_path: root_path.to_path_buf(),
                max_on_disk_bytes,
                storage_max_disk_ratio: 1.0,
                disk_usage_retriever: Arc::new(DiskUsageRetrieverImpl::new(root_path.to_path_buf())),
                max_age_days: 10,
            })
            .await
            .expect("should not fail to create retry queue with disk persistence")
    }

    /// Writes a retry file holding `contents` into `dir`, named as one persisted at `written`, as an earlier run would.
    fn write_retry_file(
        dir: &Path, written: chrono::DateTime<chrono::Utc>, nonce: u32, contents: &[u8],
    ) -> std::path::PathBuf {
        let path = dir.join(format!("retry-{}-{nonce}.json", written.format("%Y%m%d%H%M%S%f")));
        std::fs::write(&path, contents).expect("should not fail to write a retry file");
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn opening_a_persisted_queue_creates_its_directory_for_its_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let _retry_queue = open_disk_queue(temp_dir.path(), 1024, 1024).await;

        let metadata = std::fs::metadata(temp_dir.path().join("test")).expect("opening creates the queue's directory");
        assert!(metadata.is_dir());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn opening_a_persisted_queue_removes_outdated_files_and_lists_the_rest_without_reading_or_trimming_them() {
        // Pins what opening a queue on disk does today (plan 4.7 and 4.9, and its risk 12): it removes the retry files
        // whose names are older than the age limit, and lists the rest as entries by name and size alone. It reads no
        // file, so a file it cannot read does not fail it, and it does not trim the directory to its size limit: the
        // next persisted entry does that.
        use std::os::unix::fs::PermissionsExt as _;

        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let queue_dir = temp_dir.path().join("test");
        std::fs::create_dir(&queue_dir).expect("should not fail to create the queue's directory");
        let now = chrono::Utc::now();
        let serialized = |data: &FakeData| serde_json::to_vec(data).expect("should not fail to serialize");
        let outdated = write_retry_file(
            &queue_dir,
            now - chrono::Duration::days(11),
            100_000_000,
            &serialized(&FakeData::random()),
        );
        let unreadable = write_retry_file(&queue_dir, now - chrono::Duration::days(1), 100_000_001, b"never read");
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000))
            .expect("should not fail to make the file unreadable");
        let newest_data = FakeData::random();
        let newest = write_retry_file(&queue_dir, now, 100_000_002, &serialized(&newest_data));
        let unrelated = queue_dir.join("notes.txt");
        std::fs::write(&unrelated, b"not a retry file").expect("should not fail to write a file");

        // The size limit is smaller than either remaining file.
        let mut retry_queue = open_disk_queue(temp_dir.path(), 1024, 8).await;

        assert!(!outdated.exists(), "the outdated file is removed by its name");
        assert!(unreadable.exists() && newest.exists() && unrelated.exists());
        assert_eq!(
            2,
            retry_queue.len(),
            "both remaining retry files are entries, beyond the size limit"
        );

        // Popping is what reads a file: the unreadable one is dropped and counted, and the next one is returned.
        let actual = retry_queue
            .pop()
            .await
            .expect("should not fail to pop data")
            .expect("should not be empty");
        assert_eq!(newest_data, actual);
        assert_eq!(1, retry_queue.take_persisted_entries_dropped());
    }

    #[tokio::test]
    async fn a_smaller_in_memory_bound_drops_the_oldest_entries_without_disk_persistence() {
        let entries = [FakeData::random(), FakeData::random(), FakeData::random()];
        let mut retry_queue = RetryQueue::<FakeData>::new("test".to_string(), 3 * 36);
        for entry in &entries {
            let push_result = retry_queue
                .push(entry.clone())
                .await
                .expect("should not fail to push data");
            assert!(!push_result.had_drops());
        }

        let push_result = retry_queue.set_max_size_bytes(40).await;
        assert_eq!(
            (2, 2, 0),
            (
                push_result.items_dropped,
                push_result.events_dropped,
                push_result.items_persisted
            )
        );
        assert_eq!(1, retry_queue.len());
        assert_eq!(4, retry_queue.available_in_memory_capacity_bytes());

        // The bound holds for later pushes, and a larger one admits more again.
        let push_result = retry_queue
            .push(FakeData::random())
            .await
            .expect("should not fail to push data");
        assert_eq!(1, push_result.items_dropped);
        let push_result = retry_queue.set_max_size_bytes(2 * 36).await;
        assert!(!push_result.had_drops());
        let push_result = retry_queue
            .push(FakeData::random())
            .await
            .expect("should not fail to push data");
        assert!(!push_result.had_drops());
        assert_eq!(2, retry_queue.len());
    }

    #[tokio::test]
    async fn a_smaller_in_memory_bound_moves_the_oldest_entries_to_disk_with_disk_persistence() {
        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let entries = [FakeData::random(), FakeData::random(), FakeData::random()];
        let mut retry_queue = open_disk_queue(temp_dir.path(), 3 * 36, 1024).await;
        for entry in &entries {
            let push_result = retry_queue
                .push(entry.clone())
                .await
                .expect("should not fail to push data");
            assert!(!push_result.had_drops());
        }

        let push_result = retry_queue.set_max_size_bytes(40).await;
        assert_eq!((0, 2), (push_result.items_dropped, push_result.items_persisted));
        assert_eq!(2, file_count_recursive(temp_dir.path()));

        // The newest entry stays in memory and is read first; the persisted ones follow, oldest first.
        let mut popped = Vec::new();
        while let Some(entry) = retry_queue.pop().await.expect("should not fail to pop data") {
            popped.push(entry);
        }
        assert_eq!(vec![entries[2].clone(), entries[0].clone(), entries[1].clone()], popped);
    }

    #[tokio::test]
    async fn a_smaller_disk_limit_removes_the_oldest_persisted_entries() {
        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        // One entry fits in memory, so each push moves the one before it to disk.
        let mut retry_queue = open_disk_queue(temp_dir.path(), 36, 1024).await;
        let entries = [FakeData::random(), FakeData::random(), FakeData::random()];
        for entry in &entries {
            let push_result = retry_queue
                .push(entry.clone())
                .await
                .expect("should not fail to push data");
            assert!(!push_result.had_drops());
        }
        assert_eq!(2, file_count_recursive(temp_dir.path()));

        // A serialized entry is at most 30 bytes, so 40 bytes holds one of the two.
        let push_result = retry_queue
            .set_disk_limits(40, 1.0)
            .await
            .expect("should not fail to change the disk limits");
        assert_eq!((1, 1), (push_result.items_dropped, push_result.events_dropped));
        assert_eq!(1, file_count_recursive(temp_dir.path()));

        let mut popped = Vec::new();
        while let Some(entry) = retry_queue.pop().await.expect("should not fail to pop data") {
            popped.push(entry);
        }
        assert_eq!(vec![entries[2].clone(), entries[1].clone()], popped);
    }

    #[tokio::test]
    async fn disk_persistence_cannot_be_turned_on_or_off_while_a_queue_is_in_use() {
        let mut in_memory = RetryQueue::<FakeData>::new("test".to_string(), 1024);
        assert!(!in_memory.has_disk_persistence());
        assert!(in_memory.set_disk_limits(1024, 1.0).await.is_err());
        assert!(!in_memory.has_disk_persistence());

        let temp_dir = tempfile::tempdir().expect("should not fail to create temporary directory");
        let mut on_disk = open_disk_queue(temp_dir.path(), 36, 1024).await;
        assert!(on_disk.set_disk_limits(0, 1.0).await.is_err());
        // The refused change left the limit in place: two entries still fit on disk.
        for _ in 0..3 {
            let push_result = on_disk
                .push(FakeData::random())
                .await
                .expect("should not fail to push data");
            assert!(!push_result.had_drops());
        }
        assert_eq!(2, file_count_recursive(temp_dir.path()));
    }

    /// One operation on a retry queue, for the model-based property test.
    #[derive(Clone, Debug)]
    enum Operation {
        /// Pushes an entry of at least this many bytes.
        Push(usize),

        /// Pops the next entry.
        Pop,

        /// Changes the in-memory bound.
        SetMaxSizeBytes(u64),
    }

    fn operation() -> impl proptest::strategy::Strategy<Value = Operation> {
        use proptest::prelude::*;

        prop_oneof![
            4 => (2usize..48).prop_map(Operation::Push),
            2 => Just(Operation::Pop),
            1 => (0u64..160).prop_map(Operation::SetMaxSizeBytes),
        ]
    }

    proptest::proptest! {
        #[test]
        fn property_test_an_in_memory_retry_queue_keeps_its_newest_entries_within_a_bound_changed_live(
            initial_max in 0u64..160,
            operations in proptest::collection::vec(operation(), 1..64),
        ) {
            use proptest::prelude::*;

            // The model is the queue's rule stated plainly: entries leave oldest first, and the queue holds at most
            // its current bound, dropping the oldest entries to stay within it.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("should not fail to build a runtime");
            runtime.block_on(async {
                let mut retry_queue = RetryQueue::<String>::new("test".to_string(), initial_max);
                let mut model = VecDeque::<String>::new();
                let mut model_max = initial_max;
                let model_bytes = |model: &VecDeque<String>| model.iter().map(|entry| entry.len() as u64).sum::<u64>();

                for (id, operation) in operations.into_iter().enumerate() {
                    match operation {
                        Operation::Push(size) => {
                            let entry = format!("{id:0>size$}");
                            let result = retry_queue.push(entry.clone()).await;
                            if entry.len() as u64 > model_max {
                                prop_assert!(result.is_err());
                                continue;
                            }
                            let mut dropped = 0;
                            while model_bytes(&model) + entry.len() as u64 > model_max {
                                model.pop_front();
                                dropped += 1;
                            }
                            model.push_back(entry);
                            let push_result = result.expect("an entry within the bound is accepted");
                            prop_assert_eq!(dropped, push_result.items_dropped);
                            prop_assert_eq!(dropped, push_result.events_dropped);
                        }
                        Operation::Pop => {
                            let popped = retry_queue.pop().await.expect("an in-memory queue pops without errors");
                            prop_assert_eq!(model.pop_front(), popped);
                        }
                        Operation::SetMaxSizeBytes(max) => {
                            model_max = max;
                            let mut dropped = 0;
                            while model_bytes(&model) > model_max {
                                model.pop_front();
                                dropped += 1;
                            }
                            let push_result = retry_queue.set_max_size_bytes(max).await;
                            prop_assert_eq!(dropped, push_result.items_dropped);
                            prop_assert_eq!(0, push_result.items_persisted);
                        }
                    }

                    prop_assert!(model_bytes(&model) <= model_max);
                    prop_assert_eq!(model.len(), retry_queue.len());
                    prop_assert_eq!(model_max - model_bytes(&model), retry_queue.available_in_memory_capacity_bytes());
                }

                Ok(())
            })?;
        }
    }
}
