//! Self-contained logical retry entries, independent of any stream's dictionary.

use std::{
    io::ErrorKind,
    mem::{size_of, size_of_val},
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
use tracing::error;

/// Version the directory separately from the HTTP transaction format.
const QUEUE_FORMAT: &str = "stateful-metrics-v1";
const LANE_INFIX: &str = "-endpoint-";

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct RetryBatch(pub LogicalMetricBatch);

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
        // Estimate owned logical memory, including vector elements and string contents.
        let mut bytes = size_of::<Self>();
        for series in self.0.series() {
            bytes += size_of::<LogicalMetricSeries>() + series.name().len();
            bytes += series.unit().map_or(0, str::len) + series.source_type_name().map_or(0, str::len);
            bytes += size_of_val(series.points());
            for tag in series.tags().prefix.iter().chain(&series.tags().values) {
                bytes += size_of::<String>() + tag.capacity();
            }
            for resource in series.resources() {
                bytes += size_of::<MetricResource>() + resource.kind.capacity() + resource.name.capacity();
            }
        }
        bytes as u64
    }
}

/// One sender worker's retry storage.
///
/// Fresh input, overflow, and batches bound for every endpoint share `PendingTransactions`. With
/// several endpoints, each endpoint's returned copies wait in that endpoint's own lane, so an
/// endpoint that cannot send never holds up retries for the others. The worker's retry memory and
/// disk budgets are split evenly between the shared queue and the lanes; with one endpoint there
/// are no lanes and the shared queue keeps the whole budget.
pub(super) struct LanedRetryQueue {
    shared: PendingTransactions<RetryBatch>,
    lanes: Vec<RetryQueue<RetryBatch>>,
}

impl LanedRetryQueue {
    pub async fn build(
        config: &DeliveryQueueConfiguration, endpoints: &[MetaString], worker_id: usize, builder: &MetricsBuilder,
    ) -> Result<Self, GenericError> {
        let primary = endpoints
            .first()
            .ok_or_else(|| generic_error!("no stateful endpoint"))?;
        let prefix = worker_prefix(primary, worker_id);
        if endpoints.len() == 1 {
            return Ok(Self {
                shared: config.build(prefix, primary, builder).await?,
                lanes: Vec::new(),
            });
        }
        let parts = NonZeroU64::new(endpoints.len() as u64 + 1).expect("at least two queues");
        let config = config.with_budget_share(parts);
        let shared = config.build(prefix.clone(), primary, builder).await?;
        let mut lanes = Vec::with_capacity(endpoints.len());
        for endpoint in endpoints {
            lanes.push(config.build_retry_queue(lane_name(&prefix, endpoint)).await?);
        }
        Ok(Self { shared, lanes })
    }

    pub async fn push_fresh(&mut self, batch: RetryBatch) -> Result<PushResult, GenericError> {
        self.shared.push_high_priority(batch).await
    }

    /// Queues a batch for every endpoint, or only for `endpoint` when it has a lane.
    pub async fn push_retry(
        &mut self, endpoint: Option<MetricEndpointId>, batch: RetryBatch,
    ) -> Result<PushResult, GenericError> {
        match endpoint.and_then(|endpoint| self.lanes.get_mut(endpoint.get())) {
            Some(lane) => lane.push(batch).await,
            None => self.shared.push_low_priority(batch).await,
        }
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
