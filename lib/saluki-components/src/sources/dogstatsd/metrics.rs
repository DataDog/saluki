use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use ::metrics::{Counter, Gauge, Histogram, Key, Label};
use saluki_common::collections::FastHashMap;
use saluki_core::{
    components::ComponentContext,
    observability::{metrics as internal_metrics, ComponentMetricsExt as _},
};
use saluki_io::net::ListenAddress;
use saluki_metrics::MetricsBuilder;

const ERROR_TYPE_ORIGIN_DETECTION: &str = "origin_detection";
const MAX_ORIGIN_COUNTERS: usize = 200;
const MESSAGE_TYPE_EVENTS: &str = "events";
const MESSAGE_TYPE_METRICS: &str = "metrics";
const MESSAGE_TYPE_SERVICE_CHECKS: &str = "service_checks";
const METRIC_ERRORS_TOTAL: &str = "component_errors_total";
const METRIC_EVENTS_RECEIVED_TOTAL: &str = "component_events_received_total";

#[derive(Clone)]
struct OriginMetricRecorder {
    inner: Arc<Mutex<OriginMetricRecorderInner>>,
}

struct OriginMetricRecorderInner {
    builder: MetricsBuilder,
    component_id: String,
    component_type: &'static str,
    listener_type: &'static str,
    counters: FastHashMap<String, OriginCounters>,
    counter_order: VecDeque<String>,
}

struct OriginCounters {
    metrics_received: Counter,
    metric_parse_failed: Counter,
    metrics_received_key: Key,
    metric_parse_failed_key: Key,
}

impl OriginMetricRecorder {
    fn new(builder: MetricsBuilder, component_context: &ComponentContext, listener_type: &'static str) -> Self {
        Self {
            inner: Arc::new(Mutex::new(OriginMetricRecorderInner {
                builder,
                component_id: component_context.component_id().to_string(),
                component_type: component_context.component_type().as_str(),
                listener_type,
                counters: FastHashMap::default(),
                counter_order: VecDeque::with_capacity(MAX_ORIGIN_COUNTERS),
            })),
        }
    }

    fn record_metrics_received(&self, count: u64, origin: &str) {
        let mut inner = self.inner.lock().unwrap();
        let counters = inner.get_or_create_origin_counters(origin);
        counters.metrics_received.increment(count);
    }

    fn record_metric_parse_failed(&self, origin: &str) {
        let mut inner = self.inner.lock().unwrap();
        let counters = inner.get_or_create_origin_counters(origin);
        counters.metric_parse_failed.increment(1);
    }

    #[cfg(test)]
    fn cached_origin_count(&self) -> usize {
        self.inner.lock().unwrap().counters.len()
    }

    #[cfg(test)]
    fn has_cached_origin(&self, origin: &str) -> bool {
        self.inner.lock().unwrap().counters.contains_key(origin)
    }
}

impl OriginMetricRecorderInner {
    fn get_or_create_origin_counters(&mut self, origin: &str) -> &OriginCounters {
        if !self.counters.contains_key(origin) {
            self.insert_origin_counters(origin);
        }

        self.counters.get(origin).expect("origin counters should exist")
    }

    fn insert_origin_counters(&mut self, origin: &str) {
        if self.counters.len() >= MAX_ORIGIN_COUNTERS {
            self.evict_oldest_origin();
        }

        let origin = origin.to_string();
        let metrics_received_key = self.metrics_received_key(&origin);
        let metric_parse_failed_key = self.metric_parse_failed_key(&origin);
        let counters = OriginCounters {
            metrics_received: self.builder.register_counter_with_tags(
                METRIC_EVENTS_RECEIVED_TOTAL,
                [
                    ("message_type", MESSAGE_TYPE_METRICS.to_string()),
                    ("listener_type", self.listener_type.to_string()),
                    ("origin", origin.clone()),
                ],
            ),
            metric_parse_failed: self.builder.register_counter_with_tags(
                METRIC_ERRORS_TOTAL,
                [
                    ("listener_type", self.listener_type.to_string()),
                    ("error_type", "decode".to_string()),
                    ("message_type", MESSAGE_TYPE_METRICS.to_string()),
                    ("origin", origin.clone()),
                ],
            ),
            metrics_received_key,
            metric_parse_failed_key,
        };

        self.counter_order.push_back(origin.clone());
        self.counters.insert(origin, counters);
    }

    fn evict_oldest_origin(&mut self) {
        if let Some(origin) = self.counter_order.pop_front() {
            if let Some(counters) = self.counters.remove(&origin) {
                internal_metrics::delete_counter(counters.metrics_received_key);
                internal_metrics::delete_counter(counters.metric_parse_failed_key);
            }
        }
    }

    fn metrics_received_key(&self, origin: &str) -> Key {
        Key::from_parts(
            METRIC_EVENTS_RECEIVED_TOTAL,
            vec![
                Label::new("component_id", self.component_id.clone()),
                Label::from_static_parts("component_type", self.component_type),
                Label::from_static_parts("message_type", MESSAGE_TYPE_METRICS),
                Label::from_static_parts("listener_type", self.listener_type),
                Label::new("origin", origin.to_string()),
            ],
        )
    }

    fn metric_parse_failed_key(&self, origin: &str) -> Key {
        Key::from_parts(
            METRIC_ERRORS_TOTAL,
            vec![
                Label::new("component_id", self.component_id.clone()),
                Label::from_static_parts("component_type", self.component_type),
                Label::from_static_parts("listener_type", self.listener_type),
                Label::from_static_parts("error_type", "decode"),
                Label::from_static_parts("message_type", MESSAGE_TYPE_METRICS),
                Label::new("origin", origin.to_string()),
            ],
        )
    }
}

#[derive(Clone)]
pub(super) struct Metrics {
    metrics_received: Counter,
    events_received: Counter,
    service_checks_received: Counter,
    bytes_received: Counter,
    bytes_received_size: Histogram,
    framing_errors: Counter,
    metric_decoder_errors: Counter,
    event_decoder_errors: Counter,
    service_check_decoder_errors: Counter,
    origin_detection_errors: Counter,
    failed_context_resolve_total: Counter,
    connections_active: Gauge,
    packet_receive_success: Counter,
    packet_receive_failure: Counter,
    packets_forwarded: Counter,
    bytes_forwarded: Counter,
    packet_forwarding_errors: Counter,
    origin_metric_recorder: Option<OriginMetricRecorder>,
}

impl Metrics {
    pub(super) fn origin_telemetry_enabled(&self) -> bool {
        self.origin_metric_recorder.is_some()
    }

    pub(super) fn record_metrics_received(&self, count: u64, origin: Option<&str>) {
        match (origin, self.origin_metric_recorder.as_ref()) {
            (Some(origin), Some(recorder)) if !origin.is_empty() => recorder.record_metrics_received(count, origin),
            _ => self.metrics_received.increment(count),
        }
    }

    pub(super) fn record_metric_parse_failed(&self, origin: Option<&str>) {
        match (origin, self.origin_metric_recorder.as_ref()) {
            (Some(origin), Some(recorder)) if !origin.is_empty() => recorder.record_metric_parse_failed(origin),
            _ => self.metric_decoder_errors.increment(1),
        }
    }

    pub(super) fn events_received(&self) -> &Counter {
        &self.events_received
    }

    pub(super) fn service_checks_received(&self) -> &Counter {
        &self.service_checks_received
    }

    pub(super) fn bytes_received(&self) -> &Counter {
        &self.bytes_received
    }

    pub(super) fn bytes_received_size(&self) -> &Histogram {
        &self.bytes_received_size
    }

    pub(super) fn framing_errors(&self) -> &Counter {
        &self.framing_errors
    }

    pub(super) fn event_decode_failed(&self) -> &Counter {
        &self.event_decoder_errors
    }

    pub(super) fn service_check_decode_failed(&self) -> &Counter {
        &self.service_check_decoder_errors
    }

    pub(super) fn origin_detection_errors(&self) -> &Counter {
        &self.origin_detection_errors
    }

    pub(super) fn failed_context_resolve_total(&self) -> &Counter {
        &self.failed_context_resolve_total
    }

    pub(super) fn connections_active(&self) -> &Gauge {
        &self.connections_active
    }

    pub(super) fn packet_receive_success(&self) -> &Counter {
        &self.packet_receive_success
    }

    pub(super) fn packet_receive_failure(&self) -> &Counter {
        &self.packet_receive_failure
    }

    pub(super) fn packets_forwarded(&self) -> &Counter {
        &self.packets_forwarded
    }

    pub(super) fn bytes_forwarded(&self) -> &Counter {
        &self.bytes_forwarded
    }

    pub(super) fn packet_forwarding_errors(&self) -> &Counter {
        &self.packet_forwarding_errors
    }
}

pub(super) fn build_metrics(
    listen_addr: &ListenAddress, component_context: &ComponentContext, origin_telemetry_enabled: bool,
) -> Metrics {
    let builder = MetricsBuilder::from_component_context(component_context);

    let listener_type = match listen_addr {
        ListenAddress::Tcp(_) => "tcp",
        ListenAddress::Udp(_) => "udp",
        ListenAddress::Unix(_) => "unix",
        ListenAddress::Unixgram(_) => "unixgram",
        ListenAddress::NamedPipe { .. } => "named_pipe",
    };

    Metrics {
        metrics_received: builder.register_counter_with_tags(
            METRIC_EVENTS_RECEIVED_TOTAL,
            event_tags(MESSAGE_TYPE_METRICS, listener_type, origin_telemetry_enabled),
        ),
        events_received: builder.register_counter_with_tags(
            METRIC_EVENTS_RECEIVED_TOTAL,
            event_tags(MESSAGE_TYPE_EVENTS, listener_type, origin_telemetry_enabled),
        ),
        service_checks_received: builder.register_counter_with_tags(
            METRIC_EVENTS_RECEIVED_TOTAL,
            event_tags(MESSAGE_TYPE_SERVICE_CHECKS, listener_type, origin_telemetry_enabled),
        ),
        bytes_received: builder
            .register_counter_with_tags("component_bytes_received_total", [("listener_type", listener_type)]),
        bytes_received_size: builder
            .register_trace_histogram_with_tags("component_bytes_received_size", [("listener_type", listener_type)]),
        framing_errors: builder.register_counter_with_tags(
            "component_errors_total",
            [("listener_type", listener_type), ("error_type", "framing")],
        ),
        metric_decoder_errors: builder.register_counter_with_tags(
            METRIC_ERRORS_TOTAL,
            error_tags(MESSAGE_TYPE_METRICS, listener_type, origin_telemetry_enabled),
        ),
        event_decoder_errors: builder.register_counter_with_tags(
            METRIC_ERRORS_TOTAL,
            error_tags(MESSAGE_TYPE_EVENTS, listener_type, origin_telemetry_enabled),
        ),
        service_check_decoder_errors: builder.register_counter_with_tags(
            METRIC_ERRORS_TOTAL,
            error_tags(MESSAGE_TYPE_SERVICE_CHECKS, listener_type, origin_telemetry_enabled),
        ),
        origin_detection_errors: builder
            .register_counter_with_tags(METRIC_ERRORS_TOTAL, [("error_type", ERROR_TYPE_ORIGIN_DETECTION)]),
        connections_active: builder
            .register_gauge_with_tags("component_connections_active", [("listener_type", listener_type)]),
        packet_receive_success: builder.register_counter_with_tags(
            "component_packets_received_total",
            [("listener_type", listener_type), ("state", "ok")],
        ),
        packet_receive_failure: builder.register_counter_with_tags(
            "component_packets_received_total",
            [("listener_type", listener_type), ("state", "error")],
        ),
        packets_forwarded: builder.register_counter_with_tags(
            "component_packets_forwarded_total",
            [("listener_type", listener_type), ("state", "ok")],
        ),
        bytes_forwarded: builder
            .register_counter_with_tags("component_bytes_forwarded_total", [("listener_type", listener_type)]),
        packet_forwarding_errors: builder.register_counter_with_tags(
            "component_packets_forwarded_total",
            [("listener_type", listener_type), ("state", "error")],
        ),
        failed_context_resolve_total: builder.register_counter("component_failed_context_resolve_total"),
        origin_metric_recorder: origin_telemetry_enabled
            .then(|| OriginMetricRecorder::new(builder, component_context, listener_type)),
    }
}

fn event_tags(
    message_type: &'static str, listener_type: &'static str, origin_telemetry_enabled: bool,
) -> Vec<(&'static str, &'static str)> {
    let mut tags = vec![("message_type", message_type), ("listener_type", listener_type)];
    if origin_telemetry_enabled {
        tags.push(("origin", ""));
    }
    tags
}

fn error_tags(
    message_type: &'static str, listener_type: &'static str, origin_telemetry_enabled: bool,
) -> Vec<(&'static str, &'static str)> {
    let mut tags = vec![
        ("listener_type", listener_type),
        ("error_type", "decode"),
        ("message_type", message_type),
    ];
    if origin_telemetry_enabled {
        tags.push(("origin", ""));
    }
    tags
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    use saluki_core::{components::ComponentContext, support::SubsystemIdentifier, topology::ComponentId};
    use saluki_metrics::test::TestRecorder;

    use super::*;

    fn test_context() -> ComponentContext {
        ComponentContext::source(
            &SubsystemIdentifier::from_segments(["test"]),
            ComponentId::try_from("dogstatsd_test").expect("valid component ID"),
        )
    }

    fn udp_listen_addr() -> ListenAddress {
        ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8125)))
    }

    #[test]
    fn origin_metric_recorder_evicts_oldest_origin_when_cache_is_full() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let metrics = build_metrics(&udp_listen_addr(), &test_context(), true);

        for index in 0..=MAX_ORIGIN_COUNTERS {
            metrics.record_metrics_received(1, Some(&format!("container_id://test-container-{index}")));
        }

        let origin_recorder = metrics
            .origin_metric_recorder
            .as_ref()
            .expect("origin metric recorder should be enabled");
        assert_eq!(origin_recorder.cached_origin_count(), MAX_ORIGIN_COUNTERS);
        assert!(!origin_recorder.has_cached_origin("container_id://test-container-0"));
        assert!(origin_recorder.has_cached_origin("container_id://test-container-1"));
        assert!(origin_recorder.has_cached_origin("container_id://test-container-200"));
    }
}
