use std::{collections::HashMap, sync::Arc, sync::Mutex};

use http::StatusCode;
use metrics::{Counter, Histogram};
use saluki_metrics::MetricsBuilder;

use super::transaction::Metadata;

/// Component-specific telemetry.
///
/// This type covers high-level component telemetry, such as events/bytes sent, tailored to the Datadog destinations and
/// the commonalities between them.
#[derive(Clone)]
pub struct ComponentTelemetry {
    builder: MetricsBuilder,
    events_sent: Counter,
    events_sent_batch_size: Histogram,
    bytes_sent: Counter,
    events_dropped_http: Counter,
    events_dropped_encoder: Counter,
    events_dropped_queue: Counter,
    items_dropped_total: Counter,
    http_failed_send: Counter,
    http_errors_by_code: Arc<Mutex<HashMap<StatusCode, Counter>>>,
}

impl ComponentTelemetry {
    /// Creates a new `ComponentTelemetry` instance with default tags derived from the given component context.
    pub fn from_builder(builder: &MetricsBuilder) -> Self {
        Self {
            builder: builder.clone(),
            events_sent: builder.register_debug_counter("component_events_sent_total"),
            events_sent_batch_size: builder.register_debug_histogram("component_events_sent_batch_size"),
            bytes_sent: builder.register_debug_counter("component_bytes_sent_total"),
            events_dropped_http: builder.register_debug_counter_with_tags(
                "component_events_dropped_total",
                ["intentional:false", "drop_reason:http_failure"],
            ),
            events_dropped_encoder: builder.register_debug_counter_with_tags(
                "component_events_dropped_total",
                ["intentional:false", "drop_reason:encoder_failure"],
            ),
            events_dropped_queue: builder.register_debug_counter_with_tags(
                "component_events_dropped_total",
                ["intentional:true", "drop_reason:queue_limit"],
            ),
            items_dropped_total: builder.register_debug_counter("component_items_dropped_total"),
            http_failed_send: builder
                .register_debug_counter_with_tags("component_errors_total", ["error_type:http_send"]),
            http_errors_by_code: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Returns a reference to the "bytes sent" counter.
    pub fn bytes_sent(&self) -> &Counter {
        &self.bytes_sent
    }

    /// Returns a reference to the "events dropped (encoder)" counter.
    pub fn events_dropped_encoder(&self) -> &Counter {
        &self.events_dropped_encoder
    }

    /// Tracks a successful transaction.
    pub fn track_successful_transaction(&self, metadata: &Metadata) {
        self.events_sent.increment(metadata.event_count as u64);
        self.events_sent_batch_size.record(metadata.event_count as f64);
    }

    /// Tracks a failed transaction.
    ///
    /// When `status` is `Some`, the metric is additionally emitted with an `error_code` tag for the specific status.
    pub fn track_failed_transaction(&self, metadata: &Metadata, status: Option<StatusCode>) {
        match status {
            None => self.http_failed_send.increment(1),
            Some(status) => {
                let mut map = self.http_errors_by_code.lock().unwrap();
                let counter = map.entry(status).or_insert_with(|| {
                    self.builder.register_debug_counter_with_tags(
                        "component_errors_total",
                        [
                            ("error_type", "http_send".to_string()),
                            ("error_code", status.as_str().to_string()),
                        ],
                    )
                });
                counter.increment(1);
            }
        }
        self.events_dropped_http.increment(metadata.event_count as u64);
    }

    /// Tracks dropped items from queue eviction.
    pub fn track_dropped_items(&self, item_count: u64) {
        self.items_dropped_total.increment(item_count);
    }

    /// Tracks dropped events from queue eviction.
    pub fn track_dropped_events(&self, event_count: u64) {
        self.events_dropped_queue.increment(event_count);
    }
}

/// Endpoint-specific transaction queue telemetry.
///
/// This type covers high-level transaction queue telemetry, such as number of queued transactions, transactions pending
/// retry, etc.
#[derive(Clone)]
pub struct TransactionQueueTelemetry {
    high_prio_queue_insertions: Counter,
    high_prio_queue_removals: Counter,
    low_prio_queue_insertions: Counter,
    low_prio_queue_removals: Counter,
    low_prio_queue_entries_dropped: Counter,
}

impl TransactionQueueTelemetry {
    /// Creates a new `TransactionQueueTelemetry` instance with default tags derived from the given component context and the
    /// endpoint URL.
    pub fn from_builder(builder: &MetricsBuilder, endpoint_url: &str) -> Self {
        let builder = builder.clone().add_default_tag(("endpoint", endpoint_url.to_string()));

        Self {
            high_prio_queue_insertions: builder.register_debug_counter("endpoint_high_prio_queue_insertions_total"),
            high_prio_queue_removals: builder.register_debug_counter("endpoint_high_prio_queue_removals_total"),
            low_prio_queue_insertions: builder.register_debug_counter("endpoint_low_prio_queue_insertions_total"),
            low_prio_queue_removals: builder.register_debug_counter("endpoint_low_prio_queue_removals_total"),
            low_prio_queue_entries_dropped: builder
                .register_debug_counter("endpoint_low_prio_queue_entries_dropped_total"),
        }
    }

    pub fn high_prio_queue_insertions(&self) -> &Counter {
        &self.high_prio_queue_insertions
    }

    pub fn high_prio_queue_removals(&self) -> &Counter {
        &self.high_prio_queue_removals
    }

    pub fn low_prio_queue_insertions(&self) -> &Counter {
        &self.low_prio_queue_insertions
    }

    pub fn low_prio_queue_removals(&self) -> &Counter {
        &self.low_prio_queue_removals
    }

    pub fn low_prio_queue_entries_dropped(&self) -> &Counter {
        &self.low_prio_queue_entries_dropped
    }
}
