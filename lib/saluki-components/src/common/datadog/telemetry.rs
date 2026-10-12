use std::sync::{Arc, Mutex, PoisonError};

use http::StatusCode;
use metrics::{Counter, Gauge, Histogram};
use saluki_common::collections::FastHashMap;
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;

use super::{retry_capacity::RetryQueueCapacityAggregator, transaction::Metadata};

const NETWORK_HTTP_REQUESTS_ERRORS_TOTAL: &str = "network_http_requests_errors_total";
const NETWORK_HTTP_REQUESTS_REQUEUED_TOTAL: &str = "network_http_requests_requeued_total";
const NETWORK_HTTP_REQUESTS_RETRIES_TOTAL: &str = "network_http_requests_retries_total";
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

pub(crate) struct TransactionRetryTelemetry {
    builder: MetricsBuilder,
    domain: MetaString,
    counters_by_endpoint: FastHashMap<MetaString, TransactionRetryCounters>,
}

#[derive(Clone)]
pub(super) struct TransactionRetryCounters {
    retries: Counter,
    requeued: Counter,
}

impl TransactionRetryCounters {
    /// Tracks a completed retry dispatch.
    pub(super) fn increment_retries(&self) {
        self.retries.increment(1);
    }

    /// Tracks a retry that was requeued before dispatch.
    pub(super) fn increment_requeued(&self) {
        self.requeued.increment(1);
    }
}

impl TransactionRetryTelemetry {
    /// Creates retry telemetry for a fixed intake domain.
    pub(super) fn from_builder(builder: &MetricsBuilder, domain: &str) -> Self {
        Self {
            builder: builder.clone(),
            domain: MetaString::from(domain),
            counters_by_endpoint: FastHashMap::default(),
        }
    }

    /// Returns cached retry counter handles for the logical endpoint, registering them on first use.
    pub(super) fn counters_for(&mut self, endpoint: &str) -> TransactionRetryCounters {
        if let Some(counters) = self.counters_by_endpoint.get(endpoint) {
            return counters.clone();
        }

        let endpoint = MetaString::from(endpoint);
        let tags = [("domain", self.domain.to_string()), ("endpoint", endpoint.to_string())];
        let counters = TransactionRetryCounters {
            retries: self
                .builder
                .register_counter_with_tags(NETWORK_HTTP_REQUESTS_RETRIES_TOTAL, tags.clone()),
            requeued: self
                .builder
                .register_counter_with_tags(NETWORK_HTTP_REQUESTS_REQUEUED_TOTAL, tags),
        };
        self.counters_by_endpoint.insert(endpoint, counters.clone());
        counters
    }
}

/// The series a forwarder keeps per intake domain, and how many of its recipients send to the domain.
#[derive(Default)]
struct DomainSeries {
    /// The registrations of recipients that send to the domain.
    recipients: usize,

    /// `component_data_points_sent_total` for the domain, registered at its first count.
    data_points_sent: Option<Gauge>,

    /// `component_data_points_dropped_total` for the domain, registered at its first count.
    data_points_dropped: Option<Gauge>,
}

type DomainSeriesMap = Arc<Mutex<FastHashMap<String, DomainSeries>>>;

/// A recipient's hold on the per-domain series of its forwarder.
///
/// The series of a domain are released with its last registration, so a domain no recipient sends to anymore stops
/// being reported, and its running totals restart from zero once the metrics registry has evicted them, as after a
/// restart. Dropping the registration releases it.
pub(crate) struct DomainRegistration {
    domains: DomainSeriesMap,
    domain: String,
}

impl Drop for DomainRegistration {
    fn drop(&mut self) {
        let mut domains = self.domains.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(series) = domains.get_mut(&self.domain) {
            series.recipients = series.recipients.saturating_sub(1);
            if series.recipients == 0 {
                domains.remove(&self.domain);
            }
        }
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
    domains: DomainSeriesMap,
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
            domains: Arc::new(Mutex::new(FastHashMap::default())),
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

    /// Registers a recipient that sends to `domain`, which holds the domain's series until the returned registration is
    /// dropped.
    ///
    /// A domain counted without a registration keeps its series, as every domain of a forwarder whose endpoints never
    /// change does.
    pub(crate) fn register_domain(&self, domain: &str) -> DomainRegistration {
        let mut domains = self.domains.lock().unwrap_or_else(PoisonError::into_inner);
        domains.entry(domain.to_string()).or_default().recipients += 1;

        DomainRegistration {
            domains: Arc::clone(&self.domains),
            domain: domain.to_string(),
        }
    }

    /// Returns how many domains the forwarder keeps series for.
    #[cfg(test)]
    pub(super) fn domain_count(&self) -> usize {
        self.domains.lock().unwrap().len()
    }

    /// Returns a reference to the "events dropped (encoder)" counter.
    pub fn events_dropped_encoder(&self) -> &Counter {
        &self.events_dropped_encoder
    }

    /// Tracks a successful transaction.
    pub fn track_successful_transaction(&self, metadata: &Metadata, body_size: u64, domain: &str) {
        self.events_sent.increment(metadata.event_count as u64);
        self.events_sent_batch_size.record(metadata.event_count as f64);
        self.bytes_sent.increment(body_size);
        self.track_data_points_sent(domain, metadata.data_point_count as u64);
    }

    /// Tracks sent metric data points.
    pub fn track_data_points_sent(&self, domain: &str, data_point_count: u64) {
        if data_point_count == 0 {
            return;
        }

        let mut domains = self.domains.lock().unwrap();
        let series = domains.entry(domain.to_string()).or_default();
        let gauge = series.data_points_sent.get_or_insert_with(|| {
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

        let mut domains = self.domains.lock().unwrap();
        let series = domains.entry(domain.to_string()).or_default();
        let gauge = series.data_points_dropped.get_or_insert_with(|| {
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
/// retry, etc. The queue's share of the forwarder's shared retry queue series is keyed by its recipient, and is
/// released when the last clone is dropped: the queue then stops counting in the totals, and a domain no other
/// recipient sends to has its series released.
#[derive(Clone)]
pub struct TransactionQueueTelemetry {
    registration: Arc<QueueRegistration>,
    high_prio_queue_insertions: Counter,
    high_prio_queue_removals: Counter,
    low_prio_queue_insertions: Counter,
    low_prio_queue_removals: Counter,
    low_prio_queue_entries_dropped: Counter,
}

/// One recipient's share of the shared retry queue series, released when it is dropped.
struct QueueRegistration {
    recipient: MetaString,
    shared: SharedTransactionQueueTelemetry,
}

impl Drop for QueueRegistration {
    fn drop(&mut self) {
        self.shared
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .release(&self.recipient);
    }
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
    retry_queue_size_by_domain: FastHashMap<String, Gauge>,
    retry_queue_bytes_per_sec: Gauge,
    retry_queue_bytes_per_sec_by_domain: FastHashMap<String, Gauge>,
    retry_queue_capacity_secs_by_domain: FastHashMap<String, Gauge>,
    retry_queue_capacity_bytes_by_domain: FastHashMap<String, Gauge>,
}

#[derive(Default)]
struct RetryQueueStats {
    domain: MetaString,
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
            retry_queue_size_by_domain: FastHashMap::default(),
            retry_queue_bytes_per_sec: builder.register_gauge("network_http_retry_queue_bytes_per_sec"),
            retry_queue_bytes_per_sec_by_domain: FastHashMap::default(),
            retry_queue_capacity_secs_by_domain: FastHashMap::default(),
            retry_queue_capacity_bytes_by_domain: FastHashMap::default(),
        }
    }

    fn record_size(&mut self, endpoint_id: &MetaString, domain: &MetaString, len: usize) {
        let stats = self.per_endpoint.entry(endpoint_id.clone()).or_default();
        stats.domain = domain.clone();
        stats.size = len;

        let total_size = self.per_endpoint.values().map(|stats| stats.size).sum::<usize>();
        self.retry_queue_size.set(total_size as f64);

        let domain_size = self
            .per_endpoint
            .values()
            .filter(|stats| stats.domain == *domain)
            .map(|stats| stats.size)
            .sum::<usize>();
        let domain_size_gauge = self
            .retry_queue_size_by_domain
            .entry(domain.to_string())
            .or_insert_with(|| {
                self.builder
                    .register_gauge_with_tags("network_http_retry_queue_size", [("domain", domain.to_string())])
            });
        domain_size_gauge.set(domain_size as f64);
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
        self.publish_capacity_gauges();
    }

    /// Forgets a recipient that stopped: its queue stops counting in the totals, and a domain that no other recipient
    /// sends to has its series released.
    fn release(&mut self, endpoint_id: &MetaString) {
        let released = self.per_endpoint.remove(endpoint_id);
        self.capacity.remove_endpoint(endpoint_id);

        let total_size = self.per_endpoint.values().map(|stats| stats.size).sum::<usize>();
        self.retry_queue_size.set(total_size as f64);
        let total_bytes_per_sec = self.per_endpoint.values().map(|stats| stats.bytes_per_sec).sum::<f64>();
        self.retry_queue_bytes_per_sec.set(total_bytes_per_sec);

        if let Some(released) = released {
            let mut remaining = self
                .per_endpoint
                .values()
                .filter(|stats| stats.domain == released.domain)
                .peekable();
            if remaining.peek().is_some() {
                let domain_size = remaining.map(|stats| stats.size).sum::<usize>();
                if let Some(gauge) = self.retry_queue_size_by_domain.get(released.domain.as_ref()) {
                    gauge.set(domain_size as f64);
                }
            } else {
                self.retry_queue_size_by_domain.remove(released.domain.as_ref());
            }
        }
        self.publish_capacity_gauges();
    }

    /// Sets the capacity gauges of every domain with a current estimate, and releases those of every domain no endpoint
    /// sends to anymore. A domain whose rate fell to zero keeps its last values.
    fn publish_capacity_gauges(&mut self) {
        let capacity = &self.capacity;
        self.retry_queue_bytes_per_sec_by_domain
            .retain(|domain, _| capacity.covers_domain(domain));
        self.retry_queue_capacity_secs_by_domain
            .retain(|domain, _| capacity.covers_domain(domain));
        self.retry_queue_capacity_bytes_by_domain
            .retain(|domain, _| capacity.covers_domain(domain));

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

    /// Returns how many queues count in the shared series.
    #[cfg(test)]
    pub(super) fn queue_count(&self) -> usize {
        self.inner.lock().unwrap().per_endpoint.len()
    }

    fn record_retry_queue_size(&self, endpoint_id: &MetaString, domain: &MetaString, len: usize) {
        self.inner.lock().unwrap().record_size(endpoint_id, domain, len);
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
    ///
    /// `recipient` identifies the queue among the forwarder's queues, so that two recipients on one URL, with different
    /// keys, each count their own queue. It names no series.
    pub fn from_builder(
        builder: &MetricsBuilder, endpoint_url: &str, recipient: &str, shared: SharedTransactionQueueTelemetry,
    ) -> Self {
        let builder = builder.clone().add_default_tag(("endpoint", endpoint_url.to_string()));

        Self {
            registration: Arc::new(QueueRegistration {
                recipient: MetaString::from(recipient),
                shared,
            }),
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

    pub fn record_retry_queue_size(&self, domain: &MetaString, len: usize) {
        let QueueRegistration { recipient, shared } = &*self.registration;
        shared.record_retry_queue_size(recipient, domain, len);
    }

    pub fn record_retry_queue_bytes_per_sec(&self, bytes_per_sec: f64) {
        let QueueRegistration { recipient, shared } = &*self.registration;
        shared.record_retry_queue_bytes_per_sec(recipient, bytes_per_sec);
    }

    pub fn record_retry_queue_capacity_stats(
        &self, domain: &MetaString, bytes_per_sec: f64, in_memory_capacity_bytes: u64,
        disk_available_capacity_bytes: u64,
    ) {
        let QueueRegistration { recipient, shared } = &*self.registration;
        shared.record_retry_queue_capacity_stats(
            recipient,
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
    fn transaction_retry_telemetry_caches_counters_by_logical_endpoint() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let builder = MetricsBuilder::default().add_default_tag(("component_id", "forwarder"));
        let mut telemetry = TransactionRetryTelemetry::from_builder(&builder, "https://example.com");

        let first_series = telemetry.counters_for("series_v2");
        let second_series = telemetry.counters_for("series_v2");
        let sketches = telemetry.counters_for("sketches_v2");

        first_series.increment_retries();
        second_series.increment_retries();
        second_series.increment_requeued();
        sketches.increment_retries();

        assert_eq!(telemetry.counters_by_endpoint.len(), 2);
        let series_tags = &[
            ("component_id", "forwarder"),
            ("domain", "https://example.com"),
            ("endpoint", "series_v2"),
        ];
        assert_eq!(
            recorder.counter(("network_http_requests_retries_total", series_tags)),
            Some(2)
        );
        assert_eq!(
            recorder.counter(("network_http_requests_requeued_total", series_tags)),
            Some(1)
        );

        let sketches_tags = &[
            ("component_id", "forwarder"),
            ("domain", "https://example.com"),
            ("endpoint", "sketches_v2"),
        ];
        assert_eq!(
            recorder.counter(("network_http_requests_retries_total", sketches_tags)),
            Some(1)
        );
        assert_eq!(
            recorder.counter(("network_http_requests_requeued_total", sketches_tags)),
            Some(0)
        );
    }

    #[test]
    fn shared_transaction_queue_telemetry_aggregates_endpoint_snapshots() {
        let builder = MetricsBuilder::default();
        let shared = SharedTransactionQueueTelemetry::from_builder(&builder);
        let first = TransactionQueueTelemetry::from_builder(&builder, "https://one.example", "one", shared.clone());
        let second = TransactionQueueTelemetry::from_builder(&builder, "https://two.example", "two", shared.clone());

        let domain = MetaString::from_static("https://example.com");

        first.record_retry_queue_size(&domain, 3);
        first.record_retry_queue_bytes_per_sec(10.0);
        second.record_retry_queue_size(&domain, 5);
        second.record_retry_queue_bytes_per_sec(2.5);

        assert_eq!(aggregate_snapshot(&shared), (8, 12.5));

        first.record_retry_queue_size(&domain, 1);
        first.record_retry_queue_bytes_per_sec(4.0);

        assert_eq!(aggregate_snapshot(&shared), (6, 6.5));
    }

    #[test]
    fn shared_transaction_queue_telemetry_records_retry_queue_size_by_domain() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let builder = MetricsBuilder::default();
        let shared = SharedTransactionQueueTelemetry::from_builder(&builder);
        let first = TransactionQueueTelemetry::from_builder(&builder, "https://one.example/a", "one-a", shared.clone());
        let second =
            TransactionQueueTelemetry::from_builder(&builder, "https://one.example/b", "one-b", shared.clone());
        let third = TransactionQueueTelemetry::from_builder(&builder, "https://two.example", "two", shared.clone());
        let one = MetaString::from_static("https://one.example");
        let two = MetaString::from_static("https://two.example");

        first.record_retry_queue_size(&one, 3);
        second.record_retry_queue_size(&one, 4);
        third.record_retry_queue_size(&two, 5);

        assert_eq!(recorder.gauge("network_http_retry_queue_size"), Some(12.0));
        assert_eq!(
            recorder.gauge(("network_http_retry_queue_size", &[("domain", "https://one.example")])),
            Some(7.0)
        );
        assert_eq!(
            recorder.gauge(("network_http_retry_queue_size", &[("domain", "https://two.example")])),
            Some(5.0)
        );

        first.record_retry_queue_size(&one, 0);

        assert_eq!(recorder.gauge("network_http_retry_queue_size"), Some(9.0));
        assert_eq!(
            recorder.gauge(("network_http_retry_queue_size", &[("domain", "https://one.example")])),
            Some(4.0)
        );
    }

    #[test]
    fn shared_transaction_queue_telemetry_records_domain_capacity_snapshot() {
        let builder = MetricsBuilder::default();
        let shared = SharedTransactionQueueTelemetry::from_builder(&builder);
        let telemetry =
            TransactionQueueTelemetry::from_builder(&builder, "https://example.com", "example", shared.clone());

        telemetry.record_retry_queue_capacity_stats(&MetaString::from_static("domain"), 10.0, 20, 50);

        assert_eq!(domain_capacity_snapshot(&shared, "domain"), Some((10.0, 7.0, 70.0)));
    }

    #[test]
    fn two_recipients_on_one_url_each_count_their_own_queue() {
        // Two additional endpoint keys on one URL are two recipients: keyed by URL, the second's size would replace the
        // first's in the totals.
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let builder = MetricsBuilder::default();
        let shared = SharedTransactionQueueTelemetry::from_builder(&builder);
        let domain = MetaString::from_static("https://one.example");
        let first = TransactionQueueTelemetry::from_builder(&builder, "https://one.example", "key-0", shared.clone());
        let second = TransactionQueueTelemetry::from_builder(&builder, "https://one.example", "key-1", shared.clone());

        first.record_retry_queue_size(&domain, 3);
        second.record_retry_queue_size(&domain, 4);

        assert_eq!(recorder.gauge("network_http_retry_queue_size"), Some(7.0));
        assert_eq!(
            recorder.gauge(("network_http_retry_queue_size", &[("domain", "https://one.example")])),
            Some(7.0)
        );
    }

    #[test]
    fn a_stopped_queue_stops_counting_and_releases_a_domain_no_other_queue_sends_to() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let builder = MetricsBuilder::default();
        let shared = SharedTransactionQueueTelemetry::from_builder(&builder);
        let one = MetaString::from_static("https://one.example");
        let two = MetaString::from_static("https://two.example");
        let first = TransactionQueueTelemetry::from_builder(&builder, "https://one.example", "first", shared.clone());
        let second = TransactionQueueTelemetry::from_builder(&builder, "https://one.example", "second", shared.clone());
        let third = TransactionQueueTelemetry::from_builder(&builder, "https://two.example", "third", shared.clone());
        for (telemetry, domain, size, bytes_per_sec) in [
            (&first, &one, 3, 10.0),
            (&second, &one, 4, 20.0),
            (&third, &two, 5, 30.0),
        ] {
            telemetry.record_retry_queue_size(domain, size);
            telemetry.record_retry_queue_bytes_per_sec(bytes_per_sec);
            telemetry.record_retry_queue_capacity_stats(domain, bytes_per_sec, 100, 0);
        }
        assert_eq!(recorder.gauge("network_http_retry_queue_size"), Some(12.0));

        // A clone keeps the queue counted; the last one releases it.
        let first_clone = first.clone();
        drop(first);
        assert_eq!(recorder.gauge("network_http_retry_queue_size"), Some(12.0));
        drop(first_clone);
        assert_eq!(recorder.gauge("network_http_retry_queue_size"), Some(9.0));
        assert_eq!(recorder.gauge("network_http_retry_queue_bytes_per_sec"), Some(50.0));
        assert_eq!(
            recorder.gauge(("network_http_retry_queue_size", &[("domain", "https://one.example")])),
            Some(4.0)
        );
        assert_eq!(
            domain_capacity_snapshot(&shared, "https://one.example").map(|(rate, _, _)| rate),
            Some(20.0)
        );

        // The last queue on a domain takes the domain's series with it, and leaves the other domains' alone.
        drop(third);
        assert_eq!(recorder.gauge("network_http_retry_queue_size"), Some(4.0));
        let inner = shared.inner.lock().unwrap();
        assert_eq!(inner.per_endpoint.len(), 1);
        for domains in [
            inner.retry_queue_size_by_domain.keys().collect::<Vec<_>>(),
            inner.retry_queue_bytes_per_sec_by_domain.keys().collect(),
            inner.retry_queue_capacity_secs_by_domain.keys().collect(),
            inner.retry_queue_capacity_bytes_by_domain.keys().collect(),
        ] {
            assert_eq!(domains, ["https://one.example"]);
        }
        assert!(inner
            .capacity
            .domain_stats()
            .all(|(domain, _)| domain != "https://two.example"));
    }

    #[test]
    fn a_domain_releases_its_series_with_its_last_recipient_and_errors_by_status_code_stay() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let telemetry = component_telemetry();
        let first = telemetry.register_domain("datadoghq.com");
        let second = telemetry.register_domain("datadoghq.com");
        telemetry.track_data_points_sent("datadoghq.com", 3);
        telemetry.track_data_points_dropped("datadoghq.com", 2);
        telemetry.track_failed_transaction(
            &Metadata::from_event_and_data_point_count(1, 0),
            Some(StatusCode::FORBIDDEN),
        );

        drop(first);
        assert!(telemetry.domains.lock().unwrap().contains_key("datadoghq.com"));
        drop(second);
        assert!(telemetry.domains.lock().unwrap().is_empty());
        assert_eq!(telemetry.http_errors_by_code.lock().unwrap().len(), 1);

        // A domain counted again after its release starts over, with fresh handles.
        telemetry.track_data_points_sent("datadoghq.com", 1);
        let domains = telemetry.domains.lock().unwrap();
        let series = &domains["datadoghq.com"];
        assert_eq!(series.recipients, 0);
        assert!(series.data_points_sent.is_some() && series.data_points_dropped.is_none());
    }

    fn component_telemetry() -> ComponentTelemetry {
        ComponentTelemetry::from_builder(&MetricsBuilder::default())
    }

    #[test]
    fn track_successful_transaction_records_events_batch_size_and_data_points() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let telemetry = component_telemetry();

        let metadata = Metadata::from_event_and_data_point_count(4, 9);
        telemetry.track_successful_transaction(&metadata, 42, "datadoghq.com");

        assert_eq!(recorder.counter("component_events_sent_total"), Some(4));
        assert_eq!(recorder.histogram("component_events_sent_batch_size"), Some(vec![4.0]));
        assert_eq!(recorder.counter("component_bytes_sent_total"), Some(42));
        assert_eq!(
            recorder.gauge(("component_data_points_sent_total", &[("domain", "datadoghq.com")])),
            Some(9.0)
        );
    }

    #[test]
    fn track_data_points_sent_and_dropped_ignore_zero_counts() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let telemetry = component_telemetry();

        // A zero count short-circuits before registering the gauge, so no metric is emitted.
        telemetry.track_data_points_sent("datadoghq.com", 0);
        telemetry.track_data_points_dropped("datadoghq.com", 0);
        assert_eq!(
            recorder.gauge(("component_data_points_sent_total", &[("domain", "datadoghq.com")])),
            None
        );
        assert_eq!(
            recorder.gauge(("component_data_points_dropped_total", &[("domain", "datadoghq.com")])),
            None
        );

        telemetry.track_data_points_sent("datadoghq.com", 3);
        telemetry.track_data_points_dropped("datadoghq.eu", 5);
        assert_eq!(
            recorder.gauge(("component_data_points_sent_total", &[("domain", "datadoghq.com")])),
            Some(3.0)
        );
        assert_eq!(
            recorder.gauge(("component_data_points_dropped_total", &[("domain", "datadoghq.eu")])),
            Some(5.0)
        );
    }

    #[test]
    fn track_failed_transaction_without_status_records_send_failure_and_dropped_events() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let telemetry = component_telemetry();

        let metadata = Metadata::from_event_and_data_point_count(6, 0);
        telemetry.track_failed_transaction(&metadata, None);

        assert_eq!(
            recorder.counter(("component_errors_total", &[("error_type", "http_send")])),
            Some(1)
        );
        assert_eq!(
            recorder.counter((
                "component_events_dropped_total",
                &[("intentional", "false"), ("drop_reason", "http_failure")],
            )),
            Some(6)
        );
    }

    #[test]
    fn track_failed_transaction_with_status_tags_the_error_code() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let telemetry = component_telemetry();

        let metadata = Metadata::from_event_and_data_point_count(2, 0);
        telemetry.track_failed_transaction(&metadata, Some(StatusCode::FORBIDDEN));

        assert_eq!(
            recorder.counter((
                "component_errors_total",
                &[("error_type", "http_send"), ("error_code", "403")],
            )),
            Some(1)
        );
        assert_eq!(
            recorder.counter((
                "component_events_dropped_total",
                &[("intentional", "false"), ("drop_reason", "http_failure")],
            )),
            Some(2)
        );
    }

    #[test]
    fn track_permanently_failed_transaction_also_drops_data_points() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let telemetry = component_telemetry();

        let metadata = Metadata::from_event_and_data_point_count(1, 7);
        telemetry.track_permanently_failed_transaction(&metadata, None, "datadoghq.com");

        assert_eq!(
            recorder.counter(("component_errors_total", &[("error_type", "http_send")])),
            Some(1)
        );
        assert_eq!(
            recorder.counter((
                "component_events_dropped_total",
                &[("intentional", "false"), ("drop_reason", "http_failure")],
            )),
            Some(1)
        );
        assert_eq!(
            recorder.gauge(("component_data_points_dropped_total", &[("domain", "datadoghq.com")])),
            Some(7.0)
        );
    }

    #[test]
    fn track_sent_request_error_records_transaction_scoped_error() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let telemetry = component_telemetry();

        telemetry.track_sent_request_error();
        telemetry.track_sent_request_error();

        assert_eq!(
            recorder.counter((
                "network_http_requests_errors_total",
                &[("error_type", "sent_request_error"), ("error_scope", "transaction")],
            )),
            Some(2)
        );
    }

    #[test]
    fn track_dropped_items_and_events_record_queue_eviction() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let telemetry = component_telemetry();

        telemetry.track_dropped_items(5);
        telemetry.track_dropped_events(3);

        assert_eq!(recorder.counter("component_items_dropped_total"), Some(5));
        assert_eq!(
            recorder.counter((
                "component_events_dropped_total",
                &[("intentional", "true"), ("drop_reason", "queue_limit")],
            )),
            Some(3)
        );
    }
}
