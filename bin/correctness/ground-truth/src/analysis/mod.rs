use std::collections::HashSet;

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

        // Filter out histogram metrics, since we don't currently emit them the same way in ADP as we do in DSD, so
        // there's no reason to compare them yet.
        filter_histogram_metrics(&mut dsd_metrics, &mut adp_metrics);

        // Make sure both DSD and ADP emitted the same unique set of metrics. We don't yet care about the _values_ of
        // those metrics, just that both sides are emitting the same contexts.
        let (dsd_only_contexts, adp_only_contexts) = NormalizedMetrics::context_differences(&dsd_metrics, &adp_metrics);

        if !dsd_only_contexts.is_empty() || !adp_only_contexts.is_empty() {
            error!("Mismatch in unique metrics between DogStatsD and Agent Data Plane!");

            error!("Metrics in DogStatsD but not in Agent Data Plane:");
            for context in dsd_only_contexts {
                error!("  - {}", context);
            }

            error!("Metrics in Agent Data Plane but not in DogStatsD:");
            for context in adp_only_contexts {
                error!("  - {}", context);
            }

            return Err(generic_error!(
                "Mismatch in metric payloads between DogStatsD and Agent Data Plane."
            ));
        }

        info!(
            "DogStatsD and Agent Data Plane both emitted the same set of {} unique metric contexts. Continuing...",
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

fn filter_histogram_metrics(dsd_metrics: &mut NormalizedMetrics, adp_metrics: &mut NormalizedMetrics) {
    // First, we do the raw filtering of histogram metrics.
    let dsd_filtered_metrics = dsd_metrics.remove_matching(is_histogram_metric);
    let adp_filtered_metrics = adp_metrics.remove_matching(is_histogram_metric);

    if !adp_filtered_metrics.is_empty() {
        panic!("Found histogram metrics in Agent Data Plane. This is unexpected, as we don't currently emit histogram metrics in ADP.");
    }

    // Next, we examine the metrics we filtered from the DSD side to find the unique metric context once the
    // histogram-specific suffix is removed. Since the same metrics are going into DSD and ADP, we want to find the
    // corresponding metric on the ADP side and remove it.
    //
    // Essentially, we may find `foo_metric.avg`, `foo_metric.min`, and so on, when filtering from DSD. We would then
    // expect to see `foo_metric` in the ADP metrics... and that's what we want to remove.
    let histogram_metric_base_names = dsd_filtered_metrics
        .iter()
        .filter_map(|metric| metric.context().name().rsplit_once('.').map(|(base_name, _)| base_name))
        .collect::<HashSet<_>>();

    let adp_filtered_metrics =
        adp_metrics.remove_matching(|metric| histogram_metric_base_names.contains(metric.context().name()));

    info!(
        "Filtered {} histogram metric(s) from DogStatsD, and {} histogram metric(s) from Agent Data Plane.",
        dsd_filtered_metrics.len(),
        adp_filtered_metrics.len()
    );
}

fn is_internal_telemetry(metric: &NormalizedMetric) -> bool {
    metric.context().name().starts_with("datadog.") || metric.context().name().starts_with("n_o_i_n_d_e_x")
}

fn is_histogram_metric(metric: &NormalizedMetric) -> bool {
    metric.context().name().ends_with(".min")
        || metric.context().name().ends_with(".max")
        || metric.context().name().ends_with(".avg")
        || metric.context().name().ends_with(".median")
        || metric.context().name().ends_with(".count")
        || metric.context().name().ends_with(".sum")
        || metric.context().name().ends_with(".95percentile")
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
