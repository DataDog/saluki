use saluki_error::{generic_error, ErrorContext as _, GenericError};
use stele::{Metric, MetricValue};
use tracing::{error, info, warn};

mod metric;
use self::metric::{NormalizedMetric, NormalizedMetrics};

/// Raw test run results.
///
/// This holds the raw metrics sent from DogStatsD and Agent Data Plane, from the perspective of the `metrics-intake``
/// collection service.
pub struct RawTestResults {
    dsd_metrics: Vec<Metric>,
    adp_metrics: Vec<Metric>,
}

impl RawTestResults {
    /// Creates a new `RawTestResults` instance with the given metrics.
    pub fn new(dsd_metrics: Vec<Metric>, adp_metrics: Vec<Metric>) -> Self {
        Self {
            dsd_metrics,
            adp_metrics,
        }
    }

    /// Analyzes the raw metrics from DogStatsD and Agent Data Plane, comparing them to one another.
    ///
    /// # Errors
    ///
    /// If analysis fails, an error will be returned with specific details.
    pub fn run_analysis(self) -> Result<(), GenericError> {
        info!(
            "Received {} total metric payloads from DogStatsD, and {} total metric payloads from Agent Data Plane.",
            self.dsd_metrics.len(),
            self.adp_metrics.len()
        );

        // Normalize the metrics from DSD and ADP, which makes them suitable for analysis.
        let mut dsd_metrics = NormalizedMetrics::try_from_stele_metrics(self.dsd_metrics)
            .error_context("Failed to normalize DogStatsD metrics.")?;

        let mut adp_metrics = NormalizedMetrics::try_from_stele_metrics(self.adp_metrics)
            .error_context("Failed to normalize Agent Data Plane metrics.")?;

        info!(
            "Normalized {} unique metrics from raw DogStatsD payloads, and {} unique metrics from raw Agent Data Plane payloads.",
            dsd_metrics.len(),
            adp_metrics.len()
        );

        // Filter out internal telemetry metrics.
        filter_internal_telemetry_metrics(&mut dsd_metrics, &mut adp_metrics);

        // Make sure both DSD and ADP emitted the same unique set of metrics. We don't yet care about the _values_ of
        // those metrics, just that both sides are emitting the same contexts. We check both context and type,
        // so metrics with the same name but different types (e.g., Count vs Rate) are treated as different.
        let (dsd_only_pairs, adp_only_pairs) = NormalizedMetrics::context_differences(&dsd_metrics, &adp_metrics);

        if !dsd_only_pairs.is_empty() || !adp_only_pairs.is_empty() {
            error!("Mismatch in unique metric (context, type) pairs between DogStatsD and Agent Data Plane!");

            error!("Metrics in DogStatsD but not in Agent Data Plane:");
            for (context, metric_type) in dsd_only_pairs {
                error!("  - {} (type: {})", context, metric_type);
            }

            error!("Metrics in Agent Data Plane but not in DogStatsD:");
            for (context, metric_type) in adp_only_pairs {
                error!("  - {} (type: {})", context, metric_type);
            }

            return Err(generic_error!(
                "Mismatch in metric (context, type) pairs between DogStatsD and Agent Data Plane."
            ));
        }

        info!(
            "DogStatsD and Agent Data Plane both emitted the same set of {} unique metric (context, type) pairs. Continuing...",
            dsd_metrics.len()
        );

        compare_metric_values(&dsd_metrics, &adp_metrics)
    }
}

fn compare_metric_values(dsd_metrics: &NormalizedMetrics, adp_metrics: &NormalizedMetrics) -> Result<(), GenericError> {
    let mut mismatched_count = 0;

    // We can safely assume that the metrics are sorted and deduplicated at this point, so we can simply iterate over
    // them in lockstep.
    for (dsd_metric, adp_metric) in dsd_metrics.metrics().iter().zip(adp_metrics.metrics().iter()) {
        let dsd_value = dsd_metric.normalized_value();
        let adp_value = adp_metric.normalized_value();

        if dsd_value != adp_value {
            mismatched_count += 1;

            error!("Found mismatched metric '{}':", dsd_metric.context());
            warn!("  DSD: {}", get_formatted_metric_values(dsd_metric, dsd_value));
            warn!("  ADP: {}", get_formatted_metric_values(adp_metric, adp_value));
        }
    }

    if mismatched_count == 0 {
        Ok(())
    } else {
        Err(generic_error!(
            "{} metrics from DogStatsD and Agent Data Plane did not match.",
            mismatched_count
        ))
    }
}

fn filter_internal_telemetry_metrics(dsd_metrics: &mut NormalizedMetrics, adp_metrics: &mut NormalizedMetrics) {
    let dsd_filtered_metrics = dsd_metrics.remove_matching(is_internal_telemetry);
    let adp_filtered_metrics = adp_metrics.remove_matching(is_internal_telemetry);

    info!("Filtered {} internal telemetry metric(s) from DogStatsD, and {} internal telemetry metric(s) from Agent Data Plane.", dsd_filtered_metrics.len(), adp_filtered_metrics.len());
}

fn is_internal_telemetry(metric: &NormalizedMetric) -> bool {
    let name = metric.context().name();
    name.starts_with("datadog.")
        || name.starts_with("n_o_i_n_d_e_x")
        || name.starts_with("system.")
        || name.starts_with("docker.")
        || name.starts_with("container.")
        || name == "ntp.offset"
}

fn get_formatted_metric_values(metric: &NormalizedMetric, value: &MetricValue) -> String {
    let collapsed_value = get_formatted_metric_value(value);

    let mut raw_values = Vec::new();
    for (ts, raw_value) in metric.raw_values() {
        raw_values.push(format!("({} => {})", ts, get_formatted_metric_value(raw_value)));
    }

    format!("{} (raw: {})", collapsed_value, raw_values.join(", "))
}

fn get_formatted_metric_value(value: &MetricValue) -> String {
    match value {
        MetricValue::Count { value } => format!("count({})", value),
        MetricValue::Rate { interval, value } => format!("rate({} over {}s)", value, interval),
        MetricValue::Gauge { value } => format!("gauge({})", value),
        MetricValue::Sketch { sketch } => format!(
            "sketch(min={} max={} avg={} sum={} cnt={} bins_n={})",
            sketch.min().unwrap_or(0.0),
            sketch.max().unwrap_or(0.0),
            sketch.avg().unwrap_or(0.0),
            sketch.sum().unwrap_or(0.0),
            sketch.count(),
            sketch.bin_count(),
        ),
    }
}
