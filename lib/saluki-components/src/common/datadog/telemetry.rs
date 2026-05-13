use std::{collections::HashMap, sync::Arc, sync::Mutex};

use http::StatusCode;
use metrics::{Counter, Gauge, Histogram};
use saluki_common::collections::FastHashMap;
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;

use super::transaction::Metadata;

const NETWORK_HTTP_REQUESTS_ERRORS_TOTAL: &str = "network_http_requests_errors_total";
const ERROR_TYPE_SENT_REQUEST: &str = "sent_request_error";
const ERROR_SCOPE_TRANSACTION: &str = "transaction";

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
    data_points_sent_by_domain: Arc<Mutex<HashMap<String, Counter>>>,
    data_points_dropped_by_domain: Arc<Mutex<HashMap<String, Counter>>>,
    events_dropped_http: Counter,
    events_dropped_encoder: Counter,
    events_dropped_queue: Counter,
    items_dropped_total: Counter,
    http_failed_send: Counter,
    sent_request_errors: Counter,
    http_errors_by_code: Arc<Mutex<HashMap<StatusCode, Counter>>>,
}

impl ComponentTelemetry {
    /// Creates a new `ComponentTelemetry` instance with default tags derived from the given component context.
    pub fn from_builder(builder: &MetricsBuilder) -> Self {
        Self {
            builder: builder.clone(),
            events_sent: builder.register_counter("component_events_sent_total"),
            events_sent_batch_size: builder.register_trace_histogram("component_events_sent_batch_size"),
            bytes_sent: builder.register_counter("component_bytes_sent_total"),
            data_points_sent_by_domain: Arc::new(Mutex::new(HashMap::new())),
            data_points_dropped_by_domain: Arc::new(Mutex::new(HashMap::new())),
            events_dropped_http: builder.register_counter_with_tags(
                "component_events_dropped_total",
                ["intentional:false", "drop_reason:http_failure"],
            ),
            events_dropped_encoder: builder.register_counter_with_tags(
                "component_events_dropped_total",
                ["intentional:false", "drop_reason:encoder_failure"],
            ),
            events_dropped_queue: builder.register_counter_with_tags(
                "component_events_dropped_total",
                ["intentional:true", "drop_reason:queue_limit"],
            ),
            items_dropped_total: builder.register_counter("component_items_dropped_total"),
            http_failed_send: builder.register_counter_with_tags("component_errors_total", ["error_type:http_send"]),
            sent_request_errors: builder.register_counter_with_tags(
                NETWORK_HTTP_REQUESTS_ERRORS_TOTAL,
                [
                    ("error_type", ERROR_TYPE_SENT_REQUEST),
                    ("error_scope", ERROR_SCOPE_TRANSACTION),
                ],
            ),
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
    pub fn track_successful_transaction(&self, metadata: &Metadata, domain: &str) {
        self.events_sent.increment(metadata.event_count as u64);
        self.events_sent_batch_size.record(metadata.event_count as f64);
        self.track_data_points_sent(domain, metadata.data_point_count as u64);
    }

    /// Tracks sent metric data points.
    pub fn track_data_points_sent(&self, domain: &str, data_point_count: u64) {
        if data_point_count == 0 {
            return;
        }

        let mut counters = self.data_points_sent_by_domain.lock().unwrap();
        let counter = counters.entry(domain.to_string()).or_insert_with(|| {
            self.builder
                .register_counter_with_tags("component_data_points_sent_total", [("domain", domain.to_string())])
        });
        counter.increment(data_point_count);
    }

    /// Tracks dropped metric data points.
    pub fn track_data_points_dropped(&self, domain: &str, data_point_count: u64) {
        if data_point_count == 0 {
            return;
        }

        let mut counters = self.data_points_dropped_by_domain.lock().unwrap();
        let counter = counters.entry(domain.to_string()).or_insert_with(|| {
            self.builder
                .register_counter_with_tags("component_data_points_dropped_total", [("domain", domain.to_string())])
        });
        counter.increment(data_point_count);
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
                    self.builder.register_counter_with_tags(
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

    /// Tracks a permanently failed transaction.
    pub fn track_permanently_failed_transaction(&self, metadata: &Metadata, status: Option<StatusCode>, domain: &str) {
        self.track_failed_transaction(metadata, status);
        self.track_data_points_dropped(domain, metadata.data_point_count as u64);
    }

    /// Tracks a failure before a transaction is submitted to the HTTP client.
    pub fn track_sent_request_error(&self) {
        self.sent_request_errors.increment(1);
    }

    /// Tracks dropped items from queue eviction.
    pub fn track_dropped_items(&self, item_count: u64) {
        self.items_dropped_total.increment(item_count);
    }

    /// Tracks dropped events from queue eviction.
    pub fn track_dropped_events(&self, event_count: u64) {
        self.events_dropped_queue.increment(event_count);
    }

    /// Tracks dropped data points from queue eviction.
    pub fn track_dropped_data_points(&self, domain: &str, data_point_count: u64) {
        self.track_data_points_dropped(domain, data_point_count);
    }
}

/// Endpoint-specific transaction queue telemetry.
///
/// This type covers high-level transaction queue telemetry, such as number of queued transactions, transactions pending
/// retry, etc.
#[derive(Clone)]
pub struct TransactionQueueTelemetry {
    endpoint_id: MetaString,
    shared: SharedTransactionQueueTelemetry,
    high_prio_queue_insertions: Counter,
    high_prio_queue_removals: Counter,
    low_prio_queue_insertions: Counter,
    low_prio_queue_removals: Counter,
    low_prio_queue_entries_dropped: Counter,
}

/// Shared transaction queue telemetry.
#[derive(Clone)]
pub struct SharedTransactionQueueTelemetry {
    inner: Arc<Mutex<SharedTransactionQueueTelemetryInner>>,
}

struct SharedTransactionQueueTelemetryInner {
    per_endpoint: FastHashMap<MetaString, RetryQueueStats>,
    retry_queue_size: Gauge,
    retry_queue_bytes_per_sec: Gauge,
}

#[derive(Default)]
struct RetryQueueStats {
    size: usize,
    bytes_per_sec: f64,
}

impl SharedTransactionQueueTelemetry {
    /// Creates a new `SharedTransactionQueueTelemetry` instance.
    pub fn from_builder(builder: &MetricsBuilder) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SharedTransactionQueueTelemetryInner {
                per_endpoint: FastHashMap::default(),
                retry_queue_size: builder.register_gauge("network_http_retry_queue_size"),
                retry_queue_bytes_per_sec: builder.register_gauge("network_http_retry_queue_bytes_per_sec"),
            })),
        }
    }

    fn record_retry_queue_size(&self, endpoint_id: &MetaString, len: usize) {
        let mut inner = self.inner.lock().unwrap();
        inner.per_endpoint.entry(endpoint_id.clone()).or_default().size = len;

        let total_size = inner.per_endpoint.values().map(|stats| stats.size).sum::<usize>();
        inner.retry_queue_size.set(total_size as f64);
    }

    fn record_retry_queue_bytes_per_sec(&self, endpoint_id: &MetaString, bytes_per_sec: f64) {
        let mut inner = self.inner.lock().unwrap();
        inner.per_endpoint.entry(endpoint_id.clone()).or_default().bytes_per_sec = bytes_per_sec;

        let total_bytes_per_sec = inner
            .per_endpoint
            .values()
            .map(|stats| stats.bytes_per_sec)
            .sum::<f64>();
        inner.retry_queue_bytes_per_sec.set(total_bytes_per_sec);
    }

    #[cfg(test)]
    pub(crate) fn aggregate_snapshot(&self) -> (usize, f64) {
        let inner = self.inner.lock().unwrap();
        let total_size = inner.per_endpoint.values().map(|stats| stats.size).sum::<usize>();
        let total_bytes_per_sec = inner
            .per_endpoint
            .values()
            .map(|stats| stats.bytes_per_sec)
            .sum::<f64>();
        (total_size, total_bytes_per_sec)
    }
}

impl TransactionQueueTelemetry {
    /// Creates a new `TransactionQueueTelemetry` instance with default tags derived from the given component context and the
    /// endpoint URL.
    pub fn from_builder(builder: &MetricsBuilder, endpoint_url: &str, shared: SharedTransactionQueueTelemetry) -> Self {
        let builder = builder.clone().add_default_tag(("endpoint", endpoint_url.to_string()));

        Self {
            endpoint_id: MetaString::from(endpoint_url),
            shared,
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

    pub fn record_retry_queue_size(&self, len: usize) {
        self.shared.record_retry_queue_size(&self.endpoint_id, len);
    }

    pub fn record_retry_queue_bytes_per_sec(&self, bytes_per_sec: f64) {
        self.shared
            .record_retry_queue_bytes_per_sec(&self.endpoint_id, bytes_per_sec);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_transaction_queue_telemetry_aggregates_endpoint_snapshots() {
        let builder = MetricsBuilder::default();
        let shared = SharedTransactionQueueTelemetry::from_builder(&builder);
        let first = TransactionQueueTelemetry::from_builder(&builder, "https://one.example", shared.clone());
        let second = TransactionQueueTelemetry::from_builder(&builder, "https://two.example", shared.clone());

        first.record_retry_queue_size(3);
        first.record_retry_queue_bytes_per_sec(10.0);
        second.record_retry_queue_size(5);
        second.record_retry_queue_bytes_per_sec(2.5);

        assert_eq!(shared.aggregate_snapshot(), (8, 12.5));

        first.record_retry_queue_size(1);
        first.record_retry_queue_bytes_per_sec(4.0);

        assert_eq!(shared.aggregate_snapshot(), (6, 6.5));
    }
}
