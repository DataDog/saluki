use metrics::Counter;
use saluki_metrics::MetricsBuilder;

/// Component-specific telemetry.
///
/// This type covers high-level component telemetry, such as events/bytes sent, tailored to the Datadog destinations and
/// the commonalities between them.
#[derive(Clone)]
pub struct ComponentTelemetry {
    events_sent: Counter,
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
