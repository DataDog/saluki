use metrics::{Counter, Gauge};
use saluki_metrics::MetricsBuilder;

/// Telemetry of a buffering resolver.
///
/// The counters count the events of one resolver. The pending-bytes and deny list gauges report state that the
/// resolvers of a pipeline share.
pub(super) struct BufferTelemetry {
    memory_usage_bytes: Gauge,
    pending_payloads: Gauge,
    buffered_payloads: Counter,
    denied_by_denylist: Counter,
    denied_by_memory_limit: Counter,
    denylist_size: Gauge,
    denylist_expired: Counter,
}

impl BufferTelemetry {
    pub fn new(builder: &MetricsBuilder) -> Self {
        Self {
            memory_usage_bytes: builder.register_gauge("tag_buffer_memory_usage_bytes"),
            pending_payloads: builder.register_gauge("tag_buffer_pending_payloads"),
            buffered_payloads: builder.register_counter("tag_buffer_buffered_payloads_total"),
            denied_by_denylist: builder
                .register_counter_with_tags("tag_buffer_denied_payloads_total", [("reason", "denylist")]),
            denied_by_memory_limit: builder
                .register_counter_with_tags("tag_buffer_denied_payloads_total", [("reason", "memory_limit")]),
            denylist_size: builder.register_gauge("tag_buffer_denylist_size"),
            denylist_expired: builder.register_counter("tag_buffer_denylist_expired_total"),
        }
    }

    pub fn track_buffered(&self) {
        self.buffered_payloads.increment(1);
    }

    pub fn track_denied_by_denylist(&self) {
        self.denied_by_denylist.increment(1);
    }

    pub fn track_denied_by_memory_limit(&self) {
        self.denied_by_memory_limit.increment(1);
    }

    pub fn track_denylist_expired(&self, expired: usize) {
        self.denylist_expired.increment(expired as u64);
    }

    pub fn set_pending(&self, payloads: usize, bytes: usize) {
        self.pending_payloads.set(payloads as f64);
        self.memory_usage_bytes.set(bytes as f64);
    }

    pub fn set_denylist_size(&self, size: usize) {
        self.denylist_size.set(size as f64);
    }
}
