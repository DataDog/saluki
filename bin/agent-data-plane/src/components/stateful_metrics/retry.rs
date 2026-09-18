//! Self-contained logical retry entries, independent of any stream's dictionary.

use std::{
    io::ErrorKind,
    mem::{size_of, size_of_val},
    num::NonZeroUsize,
};

use foldspace_core::{LogicalMetricBatch, LogicalMetricSeries, MetricResource};
use saluki_common::hash::hash_single_stable;
use saluki_components::forwarders::queue::{DeliveryQueueConfiguration, PendingTransactions};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::util::retry::{EventContainer, Retryable};
use saluki_metrics::MetricsBuilder;
use serde::{Deserialize, Serialize};
use tokio::fs;

/// Version the directory separately from the HTTP transaction format.
const QUEUE_FORMAT: &str = "stateful-metrics-v1";

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

pub(super) async fn build_queue(
    config: &DeliveryQueueConfiguration, endpoint: &str, worker_id: usize, builder: &MetricsBuilder,
) -> Result<PendingTransactions<RetryBatch>, GenericError> {
    let queue_id = format!("{QUEUE_FORMAT}-{:016x}-{worker_id}", hash_single_stable(endpoint));
    config.build(queue_id, endpoint, builder).await
}

/// Check the layout before opening queues, which can expire or consume stored entries.
pub(super) async fn prepare_storage(
    config: &DeliveryQueueConfiguration, endpoint: &str, workers: NonZeroUsize,
) -> Result<(), GenericError> {
    let Some(root) = config.storage_path() else {
        return Ok(());
    };
    fs::create_dir_all(root).await?;
    let prefix = format!("{QUEUE_FORMAT}-{:016x}", hash_single_stable(endpoint));
    let manifest = root.join(format!("{prefix}-workers"));
    let previous: NonZeroUsize = match fs::read_to_string(&manifest).await {
        Ok(value) => value
            .trim()
            .parse()
            .error_context("Invalid stateful metrics worker-count manifest")?,
        Err(error) if error.kind() == ErrorKind::NotFound => NonZeroUsize::new(1).unwrap(),
        Err(error) => return Err(error.into()),
    };
    // The pre-sharding sender used worker zero with no manifest. Keep that namespace compatible.
    if previous != workers {
        let mut entries = fs::read_dir(root).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name
                .strip_prefix(&format!("{prefix}-"))
                .is_some_and(|id| id.parse::<usize>().is_ok())
                && entry.file_type().await?.is_dir()
                && fs::read_dir(entry.path()).await?.next_entry().await?.is_some()
            {
                return Err(generic_error!(
                    "Cannot change data_plane.stateful_metrics_workers from {previous} to {workers} while persisted retries remain in '{}'. Restart with {previous} workers and drain retries before changing the count.",
                    entry.path().display()
                ));
            }
        }
    }
    let temporary = manifest.with_extension("tmp");
    fs::write(&temporary, workers.to_string()).await?;
    fs::rename(temporary, manifest).await?;
    Ok(())
}
