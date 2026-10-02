use std::time::Duration;

use metrics::{Counter, Gauge};
use saluki_metrics::MetricsBuilder;

/// Incremental encoder-specific telemetry.
#[derive(Clone)]
pub struct ComponentTelemetry {
    events_dropped_encoder: Counter,
    events_flushed: Counter,
    last_flush_events: Gauge,
    last_flush_duration: Gauge,
}

impl ComponentTelemetry {
    /// Creates a new `ComponentTelemetry` instance with default tags derived from the given component context.
    pub fn from_builder(builder: &MetricsBuilder) -> Self {
        Self {
            events_dropped_encoder: builder.register_counter_with_tags(
                "component_events_dropped_total",
                ["intentional:false", "drop_reason:encoder_failure"],
            ),
            events_flushed: builder.register_counter("encoder_flushed_events_total"),
            last_flush_events: builder.register_gauge("encoder_last_flush_events"),
            last_flush_duration: builder.register_gauge("encoder_last_flush_duration_nanoseconds"),
        }
    }

    /// Returns a reference to the "events dropped (encoder)" counter.
    pub fn events_dropped_encoder(&self) -> &Counter {
        &self.events_dropped_encoder
    }

    /// Records a completed flush of the given number of events, which took the given duration.
    pub fn record_flush(&self, events: u64, duration: Duration) {
        self.events_flushed.increment(events);
        self.last_flush_events.set(events as f64);
        self.last_flush_duration.set(duration.as_nanos() as f64);
    }
}

#[cfg(test)]
mod tests {
    use saluki_metrics::test::TestRecorder;

    use super::*;

    #[test]
    fn record_flush_tracks_totals_and_last_flush() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());

        telemetry.record_flush(3, Duration::from_micros(5));
        telemetry.record_flush(2, Duration::from_micros(7));

        assert_eq!(recorder.counter("encoder_flushed_events_total"), Some(5));
        assert_eq!(recorder.gauge("encoder_last_flush_events"), Some(2.0));
        assert_eq!(recorder.gauge("encoder_last_flush_duration_nanoseconds"), Some(7_000.0));
    }
}
