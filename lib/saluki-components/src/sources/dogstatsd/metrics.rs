use ::metrics::{Counter, Gauge, Histogram};
use saluki_core::{components::ComponentContext, observability::ComponentMetricsExt as _};
use saluki_io::net::ListenAddress;
use saluki_metrics::MetricsBuilder;

const ERROR_TYPE_ORIGIN_DETECTION: &str = "origin_detection";

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
}

impl Metrics {
    pub(super) fn metrics_received(&self) -> &Counter {
        &self.metrics_received
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

    pub(super) fn metric_decode_failed(&self) -> &Counter {
        &self.metric_decoder_errors
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

pub(super) fn build_metrics(listen_addr: &ListenAddress, component_context: &ComponentContext) -> Metrics {
    let builder = MetricsBuilder::from_component_context(component_context);

    let listener_type = match listen_addr {
        ListenAddress::Tcp(_) => "tcp",
        ListenAddress::Udp(_) => "udp",
        ListenAddress::Unix(_) => "unix",
        ListenAddress::Unixgram(_) => "unixgram",
    };

    Metrics {
        metrics_received: builder.register_counter_with_tags(
            "component_events_received_total",
            [("message_type", "metrics"), ("listener_type", listener_type)],
        ),
        events_received: builder.register_counter_with_tags(
            "component_events_received_total",
            [("message_type", "events"), ("listener_type", listener_type)],
        ),
        service_checks_received: builder.register_counter_with_tags(
            "component_events_received_total",
            [("message_type", "service_checks"), ("listener_type", listener_type)],
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
            "component_errors_total",
            [
                ("listener_type", listener_type),
                ("error_type", "decode"),
                ("message_type", "metrics"),
            ],
        ),
        event_decoder_errors: builder.register_counter_with_tags(
            "component_errors_total",
            [
                ("listener_type", listener_type),
                ("error_type", "decode"),
                ("message_type", "events"),
            ],
        ),
        service_check_decoder_errors: builder.register_counter_with_tags(
            "component_errors_total",
            [
                ("listener_type", listener_type),
                ("error_type", "decode"),
                ("message_type", "service_checks"),
            ],
        ),
        origin_detection_errors: builder
            .register_counter_with_tags("component_errors_total", [("error_type", ERROR_TYPE_ORIGIN_DETECTION)]),
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
    }
}
