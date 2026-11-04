//! Shared priority scheduling and persistent retry storage for delivery components.

use std::{path::Path, sync::Arc};

use saluki_error::GenericError;
use saluki_io::net::util::retry::{DiskUsageRetrieverImpl, PersistedQueueArgs, RetryQueue, Retryable};
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;

pub use super::io::{PendingTransaction, PendingTransactions};
use super::{
    retry::RetryConfiguration,
    telemetry::{SharedTransactionQueueTelemetry, TransactionQueueTelemetry},
};

/// Queue settings shared with the HTTP forwarder.
pub struct DeliveryQueueConfiguration {
    high_priority_capacity: usize,
    retry: RetryConfiguration,
}

impl DeliveryQueueConfiguration {
    /// Creates delivery storage from priority capacity and retry settings.
    pub fn new(high_priority_capacity: usize, mut retry: RetryConfiguration) -> Self {
        retry.storage_path = retry.resolved_storage_path();
        Self {
            high_priority_capacity,
            retry,
        }
    }

    /// Returns the configured logical retry-memory budget in bytes.
    pub const fn retry_memory_bytes(&self) -> u64 {
        self.retry.queue_max_size_bytes
    }

    /// Returns the storage root when disk persistence is enabled.
    pub fn storage_path(&self) -> Option<&Path> {
        (self.retry.storage_max_size_bytes > 0).then_some(self.retry.storage_path.as_path())
    }

    /// Builds a queue with a distinct storage namespace and endpoint telemetry.
    ///
    /// Callers must use a stable queue name unique to the payload format, destination, and worker.
    ///
    /// # Errors
    /// Returns an error if explicitly configured disk persistence cannot be initialized.
    pub async fn build<T: Retryable>(
        &self, queue_name: String, endpoint: &str, builder: &MetricsBuilder,
    ) -> Result<PendingTransactions<T>, GenericError> {
        let retry = &self.retry;
        let storage_path = retry.resolved_storage_path();
        let mut queue = RetryQueue::new(queue_name, retry.queue_max_size_bytes)
            .with_flush_to_disk_mem_ratio(retry.flush_to_disk_mem_ratio);
        if retry.storage_max_size_bytes > 0 {
            queue = queue
                .with_disk_persistence(PersistedQueueArgs {
                    root_path: storage_path.clone(),
                    max_on_disk_bytes: retry.storage_max_size_bytes,
                    storage_max_disk_ratio: retry.storage_max_disk_ratio,
                    disk_usage_retriever: Arc::new(DiskUsageRetrieverImpl::new(storage_path.clone())),
                    max_age_days: retry.outdated_file_in_days,
                })
                .await?;
        }
        let shared = SharedTransactionQueueTelemetry::from_builder(builder);
        let telemetry = TransactionQueueTelemetry::from_builder(builder, endpoint, shared);
        Ok(PendingTransactions::new(
            self.high_priority_capacity,
            queue,
            telemetry,
            MetaString::from(endpoint),
            retry.effective_capacity_time_interval_secs(),
        ))
    }
}
