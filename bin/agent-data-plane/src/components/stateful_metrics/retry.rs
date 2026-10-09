//! Self-contained logical retry entries, independent of any stream's dictionary.

use std::{
    io::ErrorKind,
    iter,
    mem::{size_of, size_of_val, take},
    num::{NonZeroU64, NonZeroUsize},
};

use foldspace_core::{LogicalMetricBatch, LogicalMetricSeries, MetricEndpointId, MetricResource};
use saluki_common::hash::hash_single_stable;
use saluki_components::forwarders::queue::{DeliveryQueueConfiguration, PendingTransaction, PendingTransactions};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::util::retry::{EventContainer, PushResult, RetryQueue, Retryable};
use saluki_metrics::MetricsBuilder;
use serde::{Deserialize, Serialize};
use stringtheory::MetaString;
use tokio::fs;
use tracing::{error, warn};

/// Version the directory separately from the HTTP transaction format.
const QUEUE_FORMAT: &str = "stateful-metrics-v1";
const LANE_INFIX: &str = "-endpoint-";
/// A retry larger than `1 / RETRY_CHUNKS_PER_BUDGET` of the memory budget is split before it is queued.
///
/// A 10,000-series batch estimates about 7 MB, which a single entry would otherwise spend at once. Smaller entries let
/// several retries stay resident and let drop-oldest evict a part of an outage rather than a whole flush.
const RETRY_CHUNKS_PER_BUDGET: u64 = 4;

/// A logical batch and the order it entered this worker's retry memory.
///
/// The order is not persisted, so stored entries remain plain logical batches. Entries read back from disk are handed
/// to the core directly, and a requeue assigns a new order.
#[derive(Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub(super) struct RetryBatch(pub LogicalMetricBatch, #[serde(skip)] pub(super) u64);

impl RetryBatch {
    fn estimated_bytes(series: &[LogicalMetricSeries]) -> u64 {
        (size_of::<Self>() + series.iter().map(series_bytes).sum::<usize>()) as u64
    }
}

impl EventContainer for RetryBatch {
    fn event_count(&self) -> u64 {
        self.0.series().len() as u64
    }
    fn data_point_count(&self) -> u64 {
        self.0.point_count() as u64
    }
}

impl Retryable for RetryBatch {
    fn size_bytes(&self) -> u64 {
        Self::estimated_bytes(self.0.series())
    }
}

/// Estimates owned logical memory, including vector elements and string contents.
fn series_bytes(series: &LogicalMetricSeries) -> usize {
    let mut bytes = size_of::<LogicalMetricSeries>() + series.name().len();
    bytes += series.unit().map_or(0, str::len) + series.source_type_name().map_or(0, str::len);
    bytes += size_of_val(series.points());
    for tag in series.tags().prefix.iter().chain(&series.tags().values) {
        bytes += size_of::<String>() + tag.capacity();
    }
    for resource in series.resources() {
        bytes += size_of::<MetricResource>() + resource.kind.capacity() + resource.name.capacity();
    }
    bytes
}

/// Splits `batch` into consecutive batches estimated at no more than `limit` bytes, preserving series order.
///
/// A series that alone exceeds `limit` becomes its own batch.
fn split(batch: LogicalMetricBatch, limit: u64) -> Vec<LogicalMetricBatch> {
    if batch.series().len() < 2 || RetryBatch::estimated_bytes(batch.series()) <= limit {
        return vec![batch];
    }
    let empty = RetryBatch::estimated_bytes(&[]);
    let mut chunks = Vec::new();
    let mut chunk = Vec::new();
    let mut chunk_bytes = empty;
    for series in batch.into_series() {
        let bytes = series_bytes(&series) as u64;
        if !chunk.is_empty() && chunk_bytes + bytes > limit {
            chunks.push(LogicalMetricBatch::new(take(&mut chunk)));
            chunk_bytes = empty;
        }
        chunk_bytes += bytes;
        chunk.push(series);
    }
    chunks.push(LogicalMetricBatch::new(chunk));
    chunks
}

#[derive(Clone, Copy)]
enum Target {
    Fresh,
    Shared,
    Lane(usize),
}

/// One sender worker's retry storage.
///
/// Fresh input, overflow, and batches bound for every endpoint share `PendingTransactions`. With
/// several endpoints, each endpoint's returned copies wait in that endpoint's own lane, so an
/// endpoint that cannot send never holds up retries for the others.
///
/// The shared retry queue and every lane draw from one in-memory budget: any of them may use all of
/// it, and an entry that does not fit evicts the oldest in-memory entry across all of them, spilling
/// it to disk when persistence is enabled. The high-priority queue stays bounded by count. The disk
/// budget is split evenly between the shared queue and the lanes, because each queue enforces its
/// own disk limit; with one endpoint there are no lanes and the shared queue keeps the whole budget.
pub(super) struct LanedRetryQueue {
    shared: PendingTransactions<RetryBatch>,
    lanes: Vec<RetryQueue<RetryBatch>>,
    memory_budget: u64,
    next_order: u64,
}

impl LanedRetryQueue {
    pub async fn build(
        config: &DeliveryQueueConfiguration, endpoints: &[MetaString], worker_id: usize, builder: &MetricsBuilder,
    ) -> Result<Self, GenericError> {
        let primary = endpoints
            .first()
            .ok_or_else(|| generic_error!("no stateful endpoint"))?;
        let prefix = worker_prefix(primary, worker_id);
        let lane_count = if endpoints.len() > 1 { endpoints.len() } else { 0 };
        let parts = NonZeroU64::new(lane_count as u64 + 1).expect("at least one queue");
        let config = config.with_storage_budget_share(parts);
        let shared = config.build(prefix.clone(), primary, builder).await?;
        let mut lanes = Vec::with_capacity(lane_count);
        for endpoint in &endpoints[..lane_count] {
            lanes.push(config.build_retry_queue(lane_name(&prefix, endpoint)).await?);
        }
        Ok(Self {
            memory_budget: shared.retry_queue().max_in_memory_bytes(),
            shared,
            lanes,
            next_order: 0,
        })
    }

    /// Queues fresh input, which overflows into the shared retry queue once the high-priority queue is full.
    pub async fn push_fresh(&mut self, batch: LogicalMetricBatch) -> PushResult {
        self.push(Target::Fresh, batch).await
    }

    /// Queues a batch for every endpoint, or only for `endpoint` when it has a lane.
    pub async fn push_retry(&mut self, endpoint: Option<MetricEndpointId>, batch: LogicalMetricBatch) -> PushResult {
        let target = match endpoint {
            Some(endpoint) if endpoint.get() < self.lanes.len() => Target::Lane(endpoint.get()),
            _ => Target::Shared,
        };
        self.push(target, batch).await
    }

    /// Queues `batch`, split into entries that fit the memory budget unless it enters the high-priority queue.
    ///
    /// Entries the retry queues reject are logged and counted as dropped.
    async fn push(&mut self, target: Target, batch: LogicalMetricBatch) -> PushResult {
        let mut result = PushResult::default();
        let bounded = !matches!(target, Target::Fresh) || self.shared.high_priority_is_full();
        let chunks = if bounded && self.memory_budget > 0 {
            split(batch, (self.memory_budget / RETRY_CHUNKS_PER_BUDGET).max(1))
        } else {
            vec![batch]
        };
        for chunk in chunks {
            let entry = RetryBatch(chunk, self.next_order);
            self.next_order += 1;
            let size = entry.size_bytes();
            let (events, points) = (entry.event_count(), entry.data_point_count());
            if bounded && size <= self.memory_budget {
                result.merge(self.make_room(size).await);
            }
            let pushed = match target {
                Target::Fresh => self.shared.push_high_priority(entry).await,
                Target::Shared => self.shared.push_low_priority(entry).await,
                Target::Lane(lane) => self.lanes[lane].push(entry).await,
            };
            match pushed {
                Ok(pushed) => result.merge(pushed),
                Err(error) => {
                    warn!(%error, points, "Stateful metrics batch could not enter retry storage.");
                    result.items_dropped += 1;
                    result.events_dropped += events;
                    result.data_points_dropped += points;
                }
            }
        }
        result
    }

    fn in_memory_bytes(&self) -> u64 {
        self.shared.retry_queue().in_memory_bytes() + self.lanes.iter().map(RetryQueue::in_memory_bytes).sum::<u64>()
    }

    /// Evicts the oldest in-memory retries across the shared queue and every lane until `size` more bytes fit.
    ///
    /// Each queue's own limit is the whole budget, so once the pooled total has room, the push itself evicts nothing.
    async fn make_room(&mut self, size: u64) -> PushResult {
        let mut result = PushResult::default();
        let required = (self.in_memory_bytes() + size).saturating_sub(self.memory_budget);
        let target = self.shared.retry_queue().overflow_eviction_bytes(required);
        let mut removed = 0;
        while removed < target {
            let Some((lane, bytes)) = self.oldest_in_memory() else {
                break;
            };
            let evicted = match lane {
                None => self.shared.evict_oldest_low_priority().await,
                Some(lane) => self.lanes[lane].evict_oldest_in_memory().await,
            };
            result.merge(evicted.unwrap_or_default());
            removed += bytes;
        }
        result
    }

    /// Returns the lane holding the oldest in-memory retry (`None` for the shared queue) and the size of that entry.
    fn oldest_in_memory(&self) -> Option<(Option<usize>, u64)> {
        let shared = iter::once((None, self.shared.retry_queue()));
        let lanes = self.lanes.iter().enumerate().map(|(lane, queue)| (Some(lane), queue));
        shared
            .chain(lanes)
            .filter_map(|(lane, queue)| {
                let entry = queue.oldest_in_memory()?;
                Some((entry.1, lane, entry.size_bytes()))
            })
            .min_by_key(|(order, ..)| *order)
            .map(|(_, lane, bytes)| (lane, bytes))
    }

    pub async fn pop_shared(&mut self) -> Option<PendingTransaction<RetryBatch>> {
        self.shared.pop().await
    }

    pub async fn pop_lane(&mut self, endpoint: MetricEndpointId) -> Option<RetryBatch> {
        let lane = self.lanes.get_mut(endpoint.get())?;
        loop {
            match lane.pop().await {
                Ok(batch) => return batch,
                Err(e) => error!(error = %e, "Failed to pop stateful metrics retry from endpoint lane."),
            }
        }
    }

    pub fn lane_count(&self) -> usize {
        self.lanes.len()
    }

    pub fn shared_is_empty(&self) -> bool {
        self.shared.is_empty()
    }

    pub fn lane_is_empty(&self, endpoint: MetricEndpointId) -> bool {
        self.lanes.get(endpoint.get()).is_none_or(RetryQueue::is_empty)
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.shared.is_empty() && self.lanes.iter().all(RetryQueue::is_empty)
    }

    pub async fn flush(self) -> Result<PushResult, GenericError> {
        let mut result = self.shared.flush().await?;
        for lane in self.lanes {
            result.merge(lane.flush().await?);
        }
        Ok(result)
    }
}

fn storage_prefix(primary: &str) -> String {
    format!("{QUEUE_FORMAT}-{:016x}", hash_single_stable(primary))
}

fn worker_prefix(primary: &str, worker_id: usize) -> String {
    format!("{}-{worker_id}", storage_prefix(primary))
}

fn lane_name(worker_prefix: &str, endpoint: &str) -> String {
    format!("{worker_prefix}{LANE_INFIX}{:016x}", hash_single_stable(endpoint))
}

/// Check the layout before opening queues, which can expire or consume stored entries.
///
/// Storage is namespaced by the primary endpoint. Each worker owns `{prefix}-{worker}`, and with
/// several endpoints also `{prefix}-{worker}-endpoint-{hash}` per endpoint.
pub(super) async fn prepare_storage(
    config: &DeliveryQueueConfiguration, endpoints: &[MetaString], workers: NonZeroUsize,
) -> Result<(), GenericError> {
    let Some(root) = config.storage_path() else {
        return Ok(());
    };
    let primary = endpoints
        .first()
        .ok_or_else(|| generic_error!("no stateful endpoint"))?;
    fs::create_dir_all(root).await?;
    let prefix = storage_prefix(primary);
    let manifest = root.join(format!("{prefix}-workers"));
    let previous: NonZeroUsize = match fs::read_to_string(&manifest).await {
        Ok(value) => value
            .trim()
            .parse()
            .error_context("Invalid stateful metrics worker-count manifest")?,
        // The pre-sharding sender used worker zero with no manifest. Keep that namespace compatible.
        Err(error) if error.kind() == ErrorKind::NotFound => NonZeroUsize::new(1).unwrap(),
        Err(error) => return Err(error.into()),
    };
    let lanes: Vec<String> = if endpoints.len() > 1 {
        endpoints
            .iter()
            .map(|endpoint| format!("{:016x}", hash_single_stable(endpoint.as_ref())))
            .collect()
    } else {
        Vec::new()
    };
    let mut entries = fs::read_dir(root).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(rest) = name.strip_prefix(&format!("{prefix}-")) else {
            continue;
        };
        let (worker, lane) = match rest.split_once(LANE_INFIX) {
            Some((worker, lane)) => (worker, Some(lane)),
            None => (rest, None),
        };
        let changed_workers = previous != workers;
        let removed_lane = lane.is_some_and(|lane| !lanes.iter().any(|current| current == lane));
        if worker.parse::<usize>().is_err()
            || !(changed_workers || removed_lane)
            || !entry.file_type().await?.is_dir()
            || fs::read_dir(entry.path()).await?.next_entry().await?.is_none()
        {
            continue;
        }
        return Err(if changed_workers {
            generic_error!(
                "Cannot change data_plane.stateful_metrics_workers from {previous} to {workers} while persisted retries remain in '{}'. Restart with {previous} workers and drain retries before changing the count.",
                entry.path().display()
            )
        } else {
            generic_error!(
                "Cannot drop the stateful metrics retry lane '{}' while persisted retries remain in it. Restart with the previous endpoints and drain retries before changing them.",
                entry.path().display()
            )
        });
    }
    let temporary = manifest.with_extension("tmp");
    fs::write(&temporary, workers.to_string()).await?;
    fs::rename(temporary, manifest).await?;
    Ok(())
}
