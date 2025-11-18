use saluki_error::{generic_error, ErrorContext as _, GenericError};
use stele::MetricValue;
use tracing::{error, info, warn};

mod types;
use self::types::{NormalizedMetric, NormalizedMetrics};
use crate::analysis::collected::CollectedData;

/// Analyzes metrics for correctness.
pub struct MetricsAnalyzer {
    baseline_metrics: NormalizedMetrics,
    comparison_metrics: NormalizedMetrics,
}

impl MetricsAnalyzer {
    /// Creates a new `MetricsAnalyzer` instance with the given baseline/comparison data.
    pub fn new(baseline_data: &CollectedData, comparison_data: &CollectedData) -> Result<Self, GenericError> {
        let baseline_metrics = NormalizedMetrics::try_from_stele_metrics(baseline_data.metrics())
            .error_context("Failed to normalize baseline metrics.")?;

        let comparison_metrics = NormalizedMetrics::try_from_stele_metrics(comparison_data.metrics())
            .error_context("Failed to normalize comparison metrics.")?;

        Ok(Self {
            baseline_metrics,
            comparison_metrics,
        })
    }

    /// Analyzes the raw metrics from both the baseline and comparison targets, comparing them to one another.
    ///
    /// # Errors
    ///
    /// If analysis fails, an error will be returned with specific details.
    pub fn run_analysis(self) -> Result<(), GenericError> {
        let mut baseline_metrics = self.baseline_metrics;
        let mut comparison_metrics = self.comparison_metrics;

        info!(
            "Analyzing {} unfiltered metrics from baseline target, and {} unfiltered metrics from comparison target.",
            baseline_metrics.len(),
            comparison_metrics.len()
        );

        // Filter out internal telemetry metrics.
        filter_internal_telemetry_metrics(&mut baseline_metrics, &mut comparison_metrics);

        // Make sure both the baseline and comparison targets emitted the same unique set of metrics.
        //
        // We don't yet care about the _values_ of those metrics, just that both sides are emitting the same contexts.
        // We check both context and type, so metrics with the same name but different types (e.g., Count vs Rate) are
        // treated as different.
        let (baseline_only_pairs, comparison_only_pairs) =
            NormalizedMetrics::context_differences(&baseline_metrics, &comparison_metrics);

        if !baseline_only_pairs.is_empty() || !comparison_only_pairs.is_empty() {
            error!("Mismatch in unique metrics between baseline and comparison!");

            error!("Metrics in baseline but not in comparison:");
            for (context, metric_type) in baseline_only_pairs {
                error!("  - {} (type: {})", context, metric_type);
            }

            error!("Metrics in comparison but not in baseline:");
            for (context, metric_type) in comparison_only_pairs {
                error!("  - {} (type: {})", context, metric_type);
            }

            return Err(generic_error!(
                "Mismatch in metrics pairs between baseline and comparison."
            ));
        }

        info!(
            "Baseline and comparison both emitted the same set of {} unique metrics. Continuing...",
            baseline_metrics.len()
        );

        compare_metric_values(&baseline_metrics, &comparison_metrics)
    }
}

fn compare_metric_values(
    baseline_metrics: &NormalizedMetrics, comparison_metrics: &NormalizedMetrics,
) -> Result<(), GenericError> {
    let mut mismatched_count = 0;

    // We can safely assume that the metrics are sorted and deduplicated at this point, so we can simply iterate over
    // them in lockstep.
    for (baseline_metric, comparison_metric) in baseline_metrics
        .metrics()
        .iter()
        .zip(comparison_metrics.metrics().iter())
    {
        let baseline_value = baseline_metric.normalized_value();
        let comparison_value = comparison_metric.normalized_value();

        if baseline_value != comparison_value {
            mismatched_count += 1;

            error!("Found mismatched metric '{}':", baseline_metric.context());
            warn!(
                "  Baseline: {}",
                get_formatted_metric_values(baseline_metric, baseline_value)
            );
            warn!(
                "  Comparison: {}",
                get_formatted_metric_values(comparison_metric, comparison_value)
            );
        }
    }

    if mismatched_count == 0 {
        Ok(())
    } else {
        Err(generic_error!(
            "{} metrics from baseline and comparison did not match.",
            mismatched_count
        ))
    }
}

fn filter_internal_telemetry_metrics(
    baseline_metrics: &mut NormalizedMetrics, comparison_metrics: &mut NormalizedMetrics,
) {
    let baseline_filtered_metrics = baseline_metrics.remove_matching(is_internal_telemetry);
    let comparison_filtered_metrics = comparison_metrics.remove_matching(is_internal_telemetry);

    info!(
        "Filtered {} internal telemetry metric(s) from baseline, and {} internal telemetry metric(s) from comparison.",
        baseline_filtered_metrics.len(),
        comparison_filtered_metrics.len()
    );
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
