use saluki_error::{generic_error, ErrorContext as _, GenericError};
use stele::MetricValue;
use tracing::{error, info, warn};

mod types;
use self::types::{NormalizedMetric, NormalizedMetrics};
use crate::correctness::analysis::collected::CollectedData;

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
    /// If analysis fails, an error will be returned with specific details and the full list of mismatches.
    pub fn run_analysis(self) -> Result<(), (GenericError, Vec<String>)> {
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
        // We check both context and type, so metrics with the same name but different types (for example, Count vs Rate) are
        // treated as different.
        compare_metric_contexts(&baseline_metrics, &comparison_metrics)?;

        info!(
            "Baseline and comparison both emitted the same set of {} unique metrics. Continuing...",
            baseline_metrics.len()
        );

        compare_metric_values(&baseline_metrics, &comparison_metrics)
    }
}

const SAMPLE_MISMATCH_LIMIT: usize = 5;

/// Compares the unique set of (context, type) pairs emitted by the baseline and comparison targets.
///
/// # Errors
///
/// If either target emitted a pair the other one didn't, an error is returned along with details listing every
/// mismatched pair, grouped into a baseline-only section and a comparison-only section.
fn compare_metric_contexts(
    baseline_metrics: &NormalizedMetrics, comparison_metrics: &NormalizedMetrics,
) -> Result<(), (GenericError, Vec<String>)> {
    let (baseline_only_pairs, comparison_only_pairs) =
        NormalizedMetrics::context_differences(baseline_metrics, comparison_metrics);

    if baseline_only_pairs.is_empty() && comparison_only_pairs.is_empty() {
        return Ok(());
    }

    // The details are the only machine-readable record of _which_ metrics differed, so they carry both directions in
    // full, even when the inline copy in `result.json` ends up capped.
    let mut details = Vec::with_capacity(baseline_only_pairs.len() + comparison_only_pairs.len() + 3);
    details.push(format!(
        "Mismatch in metrics pairs: {} only in baseline, {} only in comparison.",
        baseline_only_pairs.len(),
        comparison_only_pairs.len()
    ));

    details.push(format!(
        "Metrics in baseline but not in comparison ({}):",
        baseline_only_pairs.len()
    ));
    for (context, metric_type) in baseline_only_pairs {
        details.push(format!("  - {} (type: {})", context, metric_type));
    }

    details.push(format!(
        "Metrics in comparison but not in baseline ({}):",
        comparison_only_pairs.len()
    ));
    for (context, metric_type) in comparison_only_pairs {
        details.push(format!("  - {} (type: {})", context, metric_type));
    }

    error!("Mismatch in unique metrics between baseline and comparison!");
    for detail in &details {
        error!("{}", detail);
    }

    Err((
        generic_error!("Mismatch in metrics pairs between baseline and comparison."),
        details,
    ))
}

fn compare_metric_values(
    baseline_metrics: &NormalizedMetrics, comparison_metrics: &NormalizedMetrics,
) -> Result<(), (GenericError, Vec<String>)> {
    let mut mismatched_count = 0;
    let mut samples: Vec<String> = Vec::new();
    let mut all_details: Vec<String> = Vec::new();

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

            let detail = format!(
                "  {}\n    baseline:    {}\n    comparison:  {}",
                baseline_metric.context(),
                get_formatted_metric_values(baseline_metric, baseline_value),
                get_formatted_metric_values(comparison_metric, comparison_value),
            );
            all_details.push(detail.clone());
            if samples.len() < SAMPLE_MISMATCH_LIMIT {
                samples.push(detail);
            }
        }
    }

    if mismatched_count == 0 {
        Ok(())
    } else {
        let mut msg = format!(
            "{} metrics from baseline and comparison did not match.",
            mismatched_count
        );
        msg.push_str(&format!("\n  (showing {} of {})", samples.len(), mismatched_count));
        for sample in samples {
            msg.push('\n');
            msg.push_str(&sample);
        }
        Err((generic_error!("{}", msg), all_details))
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

#[cfg(test)]
mod tests {
    use serde_json::json;
    use stele::Metric;

    use super::{compare_metric_contexts, NormalizedMetrics};

    fn metrics(entries: &[(&str, &[&str], &str)]) -> NormalizedMetrics {
        let metrics = entries
            .iter()
            .map(|(name, tags, mtype)| {
                serde_json::from_value::<Metric>(json!({
                    "context": {
                        "name": name,
                        "tags": tags,
                    },
                    "values": [[10, {"mtype": mtype, "value": 1.0}]],
                }))
                .expect("metric should deserialize")
            })
            .collect::<Vec<_>>();

        NormalizedMetrics::try_from_stele_metrics(&metrics).expect("metrics should normalize")
    }

    #[test]
    fn context_mismatch_details_list_every_pair_in_both_directions() {
        let baseline = metrics(&[
            ("type.flip", &["env:prod"], "Count"),
            ("shared.metric", &["env:prod"], "Count"),
            ("only.in.baseline", &["env:prod"], "Count"),
        ]);
        let comparison = metrics(&[
            ("only.in.comparison", &["env:prod"], "Count"),
            ("shared.metric", &["env:prod"], "Count"),
            ("type.flip", &["env:prod"], "Gauge"),
        ]);

        let (error, details) =
            compare_metric_contexts(&baseline, &comparison).expect_err("context mismatch should be an error");

        assert_eq!(
            error.to_string(),
            "Mismatch in metrics pairs between baseline and comparison."
        );
        assert_eq!(
            details,
            vec![
                "Mismatch in metrics pairs: 2 only in baseline, 2 only in comparison.",
                "Metrics in baseline but not in comparison (2):",
                "  - only.in.baseline[env:prod] (type: count)",
                "  - type.flip[env:prod] (type: count)",
                "Metrics in comparison but not in baseline (2):",
                "  - only.in.comparison[env:prod] (type: count)",
                "  - type.flip[env:prod] (type: gauge)",
            ]
        );
    }

    #[test]
    fn matching_contexts_yield_no_mismatch() {
        let baseline = metrics(&[("shared.metric", &["env:prod"], "Count")]);
        let comparison = metrics(&[("shared.metric", &["env:prod"], "Count")]);

        assert!(compare_metric_contexts(&baseline, &comparison).is_ok());
    }
}
