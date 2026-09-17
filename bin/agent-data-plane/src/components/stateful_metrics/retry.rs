//! Self-contained logical retry entries, independent of any stream's dictionary.

use std::mem::{size_of, size_of_val};

use foldspace_core::{LogicalMetricBatch, LogicalMetricSeries, MetricResource};
use metrics::counter;
use saluki_common::hash::hash_single_stable;
use saluki_components::forwarders::queue::{DeliveryQueueConfiguration, PendingTransactions};
use saluki_error::GenericError;
use saluki_io::net::util::retry::{EventContainer, PushResult, Retryable};
use saluki_metrics::MetricsBuilder;
use serde::{Deserialize, Serialize};
use tracing::warn;

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

pub(super) fn track_drops(result: PushResult) {
    if result.had_drops() {
        warn!(
            batches = result.items_dropped,
            points = result.data_points_dropped,
            "Stateful metrics retry storage dropped queued data."
        );
        counter!("stateful_metrics_batches_abandoned_total").increment(result.items_dropped);
        counter!("stateful_metrics_points_dropped_total").increment(result.data_points_dropped);
    }
}

// A queue rejection consumes the entry; account for it without terminating the sender.
pub(super) fn track_enqueue(result: Result<PushResult, GenericError>, points: u64) {
    match result {
        Ok(result) => track_drops(result),
        Err(error) => {
            warn!(%error, points, "Stateful metrics batch could not enter retry storage.");
            counter!("stateful_metrics_batches_abandoned_total").increment(1);
            counter!("stateful_metrics_points_dropped_total").increment(points);
        }
    }
}
