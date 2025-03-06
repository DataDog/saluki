use metrics::{Counter, Histogram};
use saluki_metrics::MetricsBuilder;

/// Component-specific telemetry.
///
/// This type covers high-level component telemetry, such as events/bytes sent, tailored to the Datadog destinations and
/// the commonalities between them.
#[derive(Clone)]
pub struct ComponentTelemetry {
    events_sent: Counter,
    events_sent_batch_size: Histogram,
    bytes_sent: Counter,
    events_dropped_http: Counter,
    events_dropped_encoder: Counter,
    http_failed_send: Counter,
}

impl ComponentTelemetry {
    /// Creates a new `ComponentTelemetry` instance with default tags derived from the given component context.
    pub fn from_builder(builder: &MetricsBuilder) -> Self {
        Self {
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
            http_failed_send: builder
                .register_debug_counter_with_tags("component_errors_total", ["error_type:http_send"]),
        }
    }

    pub fn events_sent(&self) -> &Counter {
        &self.events_sent
    }

    pub fn events_sent_batch_size(&self) -> &Histogram {
        &self.events_sent_batch_size
    }

    pub fn bytes_sent(&self) -> &Counter {
        &self.bytes_sent
    }

    pub fn events_dropped_http(&self) -> &Counter {
        &self.events_dropped_http
    }

    pub fn events_dropped_encoder(&self) -> &Counter {
        &self.events_dropped_encoder
    }

    pub fn http_failed_send(&self) -> &Counter {
        &self.http_failed_send
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
}
