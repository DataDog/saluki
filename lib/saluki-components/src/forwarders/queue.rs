//! Shared priority scheduling and persistent retry storage for delivery components.

use std::{path::Path, sync::Arc};

use agent_data_plane_config::shared::SharedConfiguration;
use saluki_error::GenericError;
use saluki_io::net::util::retry::{DiskUsageRetrieverImpl, PersistedQueueArgs, RetryQueue, Retryable};
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;

pub use crate::common::datadog::io::{PendingTransaction, PendingTransactions};
use crate::common::datadog::{
    retry::RetryConfiguration,
    telemetry::{SharedTransactionQueueTelemetry, TransactionQueueTelemetry},
};

/// Queue settings shared with the HTTP forwarder.
pub struct DeliveryQueueConfiguration {
    high_priority_capacity: usize,
    retry: RetryConfiguration,
}

impl DeliveryQueueConfiguration {
    /// Reads priority capacity, memory limits, and optional disk storage from forwarder settings.
    pub fn from_configuration(shared: &SharedConfiguration) -> Self {
        Self {
            high_priority_capacity: shared.endpoints.forwarder.high_prio_buffer_size,
            retry: RetryConfiguration::from_configuration(&shared.endpoints.forwarder, shared.run_path.as_deref()),
        }
    }

    /// Returns the storage root when disk persistence is enabled.
    ///
    /// Delivery components may validate persistent worker layouts before opening any queues.
    pub fn storage_path(&self) -> Option<&Path> {
        (self.retry.storage_max_size_bytes() > 0).then(|| self.retry.storage_path())
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
        let mut queue = RetryQueue::new(queue_name, retry.queue_max_size_bytes())
            .with_flush_to_disk_mem_ratio(retry.flush_to_disk_mem_ratio());
        if retry.storage_max_size_bytes() > 0 {
            queue = queue
                .with_disk_persistence(PersistedQueueArgs {
                    root_path: retry.storage_path().to_path_buf(),
                    max_on_disk_bytes: retry.storage_max_size_bytes(),
                    storage_max_disk_ratio: retry.storage_max_disk_ratio(),
                    disk_usage_retriever: Arc::new(DiskUsageRetrieverImpl::new(retry.storage_path().to_path_buf())),
                    max_age_days: retry.outdated_file_in_days(),
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
            retry.capacity_time_interval_secs(),
        ))
    }
}
