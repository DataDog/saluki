use std::sync::{Arc, Mutex};

use http::StatusCode;
use metrics::{Counter, Gauge, Histogram};
use saluki_common::collections::FastHashMap;
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;

use super::{retry_capacity::RetryQueueCapacityAggregator, transaction::Metadata};

const NETWORK_HTTP_REQUESTS_ERRORS_TOTAL: &str = "network_http_requests_errors_total";
const ERROR_TYPE_SENT_REQUEST: &str = "sent_request_error";
const ERROR_SCOPE_TRANSACTION: &str = "transaction";

#[derive(Clone)]
pub(super) struct TransactionInputTelemetry {
    count: Counter,
    bytes: Counter,
}

impl TransactionInputTelemetry {
    /// Tracks a transaction entering the forwarder queue.
    pub(super) fn track(&self, bytes: u64) {
        self.count.increment(1);
        self.bytes.increment(bytes);
    }
}

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
    data_points_sent_by_domain: Arc<Mutex<FastHashMap<String, Gauge>>>,
    data_points_dropped_by_domain: Arc<Mutex<FastHashMap<String, Gauge>>>,
    events_dropped_http: Counter,
    events_dropped_encoder: Counter,
    events_dropped_queue: Counter,
    items_dropped_total: Counter,
    http_failed_send: Counter,
    sent_request_errors: Counter,
    http_errors_by_code: Arc<Mutex<FastHashMap<StatusCode, Counter>>>,
}

impl ComponentTelemetry {
    /// Creates a new `ComponentTelemetry` instance with default tags derived from the given component context.
    pub fn from_builder(builder: &MetricsBuilder) -> Self {
        Self {
            builder: builder.clone(),
            events_sent: builder.register_counter("component_events_sent_total"),
            events_sent_batch_size: builder.register_trace_histogram("component_events_sent_batch_size"),
            bytes_sent: builder.register_counter("component_bytes_sent_total"),
            data_points_sent_by_domain: Arc::new(Mutex::new(FastHashMap::default())),
            data_points_dropped_by_domain: Arc::new(Mutex::new(FastHashMap::default())),
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
            http_errors_by_code: Arc::new(Mutex::new(FastHashMap::default())),
        }
    }

    /// Returns a reference to the "bytes sent" counter.
    pub fn bytes_sent(&self) -> &Counter {
        &self.bytes_sent
    }

    /// Creates telemetry handles for transactions entering the forwarder queue.
    pub(super) fn register_transaction_input_telemetry(
        &self, domain: &str, endpoint: &str,
    ) -> TransactionInputTelemetry {
        let tags = [("domain", domain.to_string()), ("endpoint", endpoint.to_string())];
        TransactionInputTelemetry {
            count: self
                .builder
                .register_counter_with_tags("network_http_requests_input_total", tags.clone()),
            bytes: self
                .builder
                .register_counter_with_tags("network_http_requests_input_bytes_total", tags),
        }
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

        let mut gauges = self.data_points_sent_by_domain.lock().unwrap();
        let gauge = gauges.entry(domain.to_string()).or_insert_with(|| {
            self.builder
                .register_gauge_with_tags("component_data_points_sent_total", [("domain", domain.to_string())])
        });
        gauge.increment(data_point_count as f64);
    }

    /// Tracks dropped metric data points.
    pub fn track_data_points_dropped(&self, domain: &str, data_point_count: u64) {
        if data_point_count == 0 {
            return;
        }

        let mut gauges = self.data_points_dropped_by_domain.lock().unwrap();
        let gauge = gauges.entry(domain.to_string()).or_insert_with(|| {
            self.builder
                .register_gauge_with_tags("component_data_points_dropped_total", [("domain", domain.to_string())])
        });
        gauge.increment(data_point_count as f64);
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
    builder: MetricsBuilder,
    per_endpoint: FastHashMap<MetaString, RetryQueueStats>,
    capacity: RetryQueueCapacityAggregator,
    retry_queue_size: Gauge,
    retry_queue_bytes_per_sec: Gauge,
    retry_queue_bytes_per_sec_by_domain: FastHashMap<String, Gauge>,
    retry_queue_capacity_secs_by_domain: FastHashMap<String, Gauge>,
    retry_queue_capacity_bytes_by_domain: FastHashMap<String, Gauge>,
}

#[derive(Default)]
struct RetryQueueStats {
    size: usize,
    bytes_per_sec: f64,
}

impl SharedTransactionQueueTelemetryInner {
    fn from_builder(builder: &MetricsBuilder) -> Self {
        Self {
            builder: builder.clone(),
            per_endpoint: FastHashMap::default(),
            capacity: RetryQueueCapacityAggregator::new(),
            retry_queue_size: builder.register_gauge("network_http_retry_queue_size"),
            retry_queue_bytes_per_sec: builder.register_gauge("network_http_retry_queue_bytes_per_sec"),
            retry_queue_bytes_per_sec_by_domain: FastHashMap::default(),
            retry_queue_capacity_secs_by_domain: FastHashMap::default(),
            retry_queue_capacity_bytes_by_domain: FastHashMap::default(),
        }
    }

    fn record_size(&mut self, endpoint_id: &MetaString, len: usize) {
        self.per_endpoint.entry(endpoint_id.clone()).or_default().size = len;

        let total_size = self.per_endpoint.values().map(|stats| stats.size).sum::<usize>();
        self.retry_queue_size.set(total_size as f64);
    }

    fn record_bytes_per_sec(&mut self, endpoint_id: &MetaString, bytes_per_sec: f64) {
        self.per_endpoint.entry(endpoint_id.clone()).or_default().bytes_per_sec = bytes_per_sec;

        let total_bytes_per_sec = self.per_endpoint.values().map(|stats| stats.bytes_per_sec).sum::<f64>();
        self.retry_queue_bytes_per_sec.set(total_bytes_per_sec);
    }

    fn record_capacity_stats(
        &mut self, endpoint_id: &MetaString, domain: &MetaString, bytes_per_sec: f64, in_memory_capacity_bytes: u64,
        disk_available_capacity_bytes: u64,
    ) {
        self.capacity.update_endpoint(
            endpoint_id.clone(),
            domain.clone(),
            bytes_per_sec,
            in_memory_capacity_bytes,
            disk_available_capacity_bytes,
        );
        self.retry_queue_bytes_per_sec.set(self.capacity.total_bytes_per_sec());

        let domain_stats = self
            .capacity
            .domain_stats()
            .map(|(domain, stats)| (domain.to_string(), stats))
            .collect::<Vec<_>>();

        for (domain, stats) in domain_stats {
            let bytes_per_sec = stats.bytes_per_sec;
            let capacity_secs = stats.capacity_secs;
            let capacity_bytes = stats.capacity_bytes;

            let bytes_per_sec_gauge = self
                .retry_queue_bytes_per_sec_by_domain
                .entry(domain.clone())
                .or_insert_with(|| {
                    self.builder.register_gauge_with_tags(
                        "network_http_retry_queue_bytes_per_sec",
                        [("domain", domain.clone())],
                    )
                });
            bytes_per_sec_gauge.set(bytes_per_sec);

            let capacity_secs_gauge = self
                .retry_queue_capacity_secs_by_domain
                .entry(domain.clone())
                .or_insert_with(|| {
                    self.builder.register_gauge_with_tags(
                        "network_http_retry_queue_capacity_secs",
                        [("domain", domain.clone())],
                    )
                });
            capacity_secs_gauge.set(capacity_secs);

            let capacity_bytes_gauge = self
                .retry_queue_capacity_bytes_by_domain
                .entry(domain.clone())
                .or_insert_with(|| {
                    self.builder
                        .register_gauge_with_tags("network_http_retry_queue_capacity_bytes", [("domain", domain)])
                });
            capacity_bytes_gauge.set(capacity_bytes);
        }
    }
}

impl SharedTransactionQueueTelemetry {
    /// Creates a new `SharedTransactionQueueTelemetry` instance.
    pub fn from_builder(builder: &MetricsBuilder) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SharedTransactionQueueTelemetryInner::from_builder(builder))),
        }
    }

    fn record_retry_queue_size(&self, endpoint_id: &MetaString, len: usize) {
        self.inner.lock().unwrap().record_size(endpoint_id, len);
    }

    fn record_retry_queue_bytes_per_sec(&self, endpoint_id: &MetaString, bytes_per_sec: f64) {
        self.inner
            .lock()
            .unwrap()
            .record_bytes_per_sec(endpoint_id, bytes_per_sec);
    }

    fn record_retry_queue_capacity_stats(
        &self, endpoint_id: &MetaString, domain: &MetaString, bytes_per_sec: f64, in_memory_capacity_bytes: u64,
        disk_available_capacity_bytes: u64,
    ) {
        self.inner.lock().unwrap().record_capacity_stats(
            endpoint_id,
            domain,
            bytes_per_sec,
            in_memory_capacity_bytes,
            disk_available_capacity_bytes,
        );
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

    pub fn record_retry_queue_capacity_stats(
        &self, domain: &MetaString, bytes_per_sec: f64, in_memory_capacity_bytes: u64,
        disk_available_capacity_bytes: u64,
    ) {
        self.shared.record_retry_queue_capacity_stats(
            &self.endpoint_id,
            domain,
            bytes_per_sec,
            in_memory_capacity_bytes,
            disk_available_capacity_bytes,
        );
    }
}

#[cfg(test)]
mod tests {
    use saluki_metrics::test::TestRecorder;

    use super::*;

    fn aggregate_snapshot(shared: &SharedTransactionQueueTelemetry) -> (usize, f64) {
        let inner = shared.inner.lock().unwrap();
        let total_size = inner.per_endpoint.values().map(|stats| stats.size).sum::<usize>();
        let total_bytes_per_sec = inner
            .per_endpoint
            .values()
            .map(|stats| stats.bytes_per_sec)
            .sum::<f64>();

        (total_size, total_bytes_per_sec)
    }

    fn domain_capacity_snapshot(shared: &SharedTransactionQueueTelemetry, domain: &str) -> Option<(f64, f64, f64)> {
        let inner = shared.inner.lock().unwrap();
        let snapshot = inner
            .capacity
            .domain_stats()
            .find_map(|(stats_domain, stats)| (stats_domain == domain).then_some(stats))?;

        Some((snapshot.bytes_per_sec, snapshot.capacity_secs, snapshot.capacity_bytes))
    }

    #[test]
    fn registered_transaction_input_telemetry_handles_record_same_counters() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());

        let first = telemetry.register_transaction_input_telemetry("https://example.com", "series_v2");
        let second = telemetry.register_transaction_input_telemetry("https://example.com", "series_v2");

        first.track(12);
        second.track(30);

        let tags = &[("domain", "https://example.com"), ("endpoint", "series_v2")];
        assert_eq!(recorder.counter(("network_http_requests_input_total", tags)), Some(2));
        assert_eq!(
            recorder.counter(("network_http_requests_input_bytes_total", tags)),
            Some(42)
        );
    }

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

        assert_eq!(aggregate_snapshot(&shared), (8, 12.5));

        first.record_retry_queue_size(1);
        first.record_retry_queue_bytes_per_sec(4.0);

        assert_eq!(aggregate_snapshot(&shared), (6, 6.5));
    }

    #[test]
    fn shared_transaction_queue_telemetry_records_domain_capacity_snapshot() {
        let builder = MetricsBuilder::default();
        let shared = SharedTransactionQueueTelemetry::from_builder(&builder);
        let telemetry = TransactionQueueTelemetry::from_builder(&builder, "https://example.com", shared.clone());

        telemetry.record_retry_queue_capacity_stats(&MetaString::from_static("domain"), 10.0, 20, 50);

        assert_eq!(domain_capacity_snapshot(&shared, "domain"), Some((10.0, 7.0, 70.0)));
    }
}
