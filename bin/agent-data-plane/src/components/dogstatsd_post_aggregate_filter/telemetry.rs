use metrics::Counter;
use saluki_metrics::MetricsBuilder;

pub(super) const FILTERED_METRICS_METRIC: &str = "dogstatsd_post_aggregate_filtered_metrics_total";

#[derive(Clone)]
pub(super) struct Telemetry {
    filtered_metrics: Counter,
}

impl Telemetry {
    pub(super) fn new(builder: &MetricsBuilder) -> Self {
        Self {
            filtered_metrics: builder.register_debug_counter(FILTERED_METRICS_METRIC),
        }
    }

    #[cfg(test)]
    pub(super) fn noop() -> Self {
        Self {
            filtered_metrics: Counter::noop(),
        }
    }

    pub(super) fn increment_filtered_metrics(&self) {
        self.filtered_metrics.increment(1);
    }
}
