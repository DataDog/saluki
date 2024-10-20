use std::collections::HashSet;

use saluki_error::{generic_error, GenericError};
use stele::Metric;
use tracing::{error, info};

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

    /// Returns the raw metrics received from DogStatsD.
    pub fn dsd_metrics(&self) -> &[Metric] {
        &self.dsd_metrics
    }

    /// Returns the raw metrics received from Agent Data Plane.
    pub fn adp_metrics(&self) -> &[Metric] {
        &self.adp_metrics
    }

    /// Analyzes the raw metrics from DogStatsD and Agent Data Plane, comparing them to one another.
    ///
    /// # Errors
    ///
    /// If analysis fails, an error will be returned with specific details.
    pub fn run_analysis(self) -> Result<(), GenericError> {
        // Consume and normalize the metrics, which ensures they're in a consistent state before any analysis.
        let mut dsd_metrics = self.dsd_metrics;
        dsd_metrics.iter_mut().for_each(Metric::normalize);

        let mut adp_metrics = self.adp_metrics;
        adp_metrics.iter_mut().for_each(Metric::normalize);

        info!("Received {} total metrics from DogStatsD, and {} total metrics from Agent Data Plane.", dsd_metrics.len(), adp_metrics.len());

        // Filter out internal telemetry metrics.
        filter_internal_telemetry_metrics(&mut dsd_metrics, &mut adp_metrics);

        // Filter out histogram metrics, since we don't currently emit them the same way in ADP as we do in DSD, so
        // there's no reason to compare them yet.
        filter_histogram_metrics(&mut dsd_metrics, &mut adp_metrics);

        // With known outliers filtered out, we can actually compare the metrics between DSD and ADP.
        //
        // Our first test will simply be to analyze the number of unique metrics (contexts) overall, since we should be
        // emitting the same metrics from both DSD and ADP.
        let dsd_unique_metrics = dsd_metrics.iter().map(|m| m.context()).collect::<HashSet<_>>();
        let adp_unique_metrics = adp_metrics.iter().map(|m| m.context()).collect::<HashSet<_>>();

        if dsd_unique_metrics != adp_unique_metrics {
            error!(
                "Mismatch in unique metrics between DogStatsD and Agent Data Plane: {} vs. {}",
                dsd_unique_metrics.len(),
                adp_unique_metrics.len()
            );

            let missing_from_adp = dsd_unique_metrics.difference(&adp_unique_metrics).collect::<Vec<_>>();
            let missing_from_dsd = adp_unique_metrics.difference(&dsd_unique_metrics).collect::<Vec<_>>();

            error!("Metrics in DogStatsD but not in Agent Data Plane:");
            for metric in missing_from_adp {
                error!("  - {}", metric);
            }

            error!("Metrics in Agent Data Plane but not in DogStatsD:");
            for metric in missing_from_dsd {
                error!("  - {}", metric);
            }

            return Err(generic_error!(
                "Mismatch in metric payloads between DogStatsD and Agent Data Plane."
            ));
        }

        info!("DogStatsD and Agent Data Plane both emitted the same set of unique metric contexts. Continuing...");

        // Merge and normalize the metrics, which sorts and deduplicates them. This involves merging multiple instances
        // of `Metric` with the same context, and within a `Metric`, sorting and merging datapoints with the same
        // timestamp, using metric type-specific merging logic.
        //
        // We'll compare the metric values after this step.
        let mut dsd_anchored_metrics = dsd_metrics.clone();
        let mut adp_anchored_metrics = adp_metrics.clone();

        merge_and_normalize_metrics(&mut dsd_anchored_metrics);
        merge_and_normalize_metrics(&mut adp_anchored_metrics);

        info!("Merged and normalized metrics from DogStatsD ({}) and Agent Data Plane ({}). Comparing values...", dsd_anchored_metrics.len(), adp_anchored_metrics.len());

        compare_metric_values(&dsd_anchored_metrics, &adp_anchored_metrics);

        Ok(())
    }
}

fn compare_metric_values(dsd_metrics: &[Metric], adp_metrics: &[Metric]) {
    let mut mismatched_count = 0;

    // We can safely assume that the metrics are sorted and deduplicated at this point, so we can simply iterate over
    // them in lockstep.
    for (dsd_metric, adp_metric) in dsd_metrics.iter().zip(adp_metrics.iter()) {
        if dsd_metric != adp_metric {
            mismatched_count += 1;
        }
    }

    if mismatched_count == 0 {
        info!("All metrics from DogStatsD and Agent Data Plane matched successfully.");
    } else {
        error!("{} metrics from DogStatsD and Agent Data Plane did not match.", mismatched_count);
    }
}

/// Filters out metrics that match a given predicate.
///
/// If `filter` returns `true` for a given metric, it will be removed from the provided `metrics` vector.
///
/// Returns all of the metrics that were filtered out.
fn filter_metrics<F>(metrics: &mut Vec<Metric>, filter: F) -> Vec<Metric>
where
    F: Fn(&Metric) -> bool,
{
    let mut filtered = metrics.clone();
    filtered.retain(|metric| !filter(metric));
    metrics.retain(|metric| filter(metric));

    std::mem::replace(metrics, filtered)
}

fn filter_internal_telemetry_metrics(dsd_metrics: &mut Vec<Metric>, adp_metrics: &mut Vec<Metric>) {
    let dsd_filtered_metrics = filter_metrics(dsd_metrics, is_internal_telemetry);
    let adp_filtered_metrics = filter_metrics(adp_metrics, is_internal_telemetry);

    info!("Filtered {} internal telemetry metric(s) from DogStatsD, and {} internal telemetry metric(s) from Agent Data Plane.", dsd_filtered_metrics.len(), adp_filtered_metrics.len());
}

fn filter_histogram_metrics(dsd_metrics: &mut Vec<Metric>, adp_metrics: &mut Vec<Metric>) {
    // First, we do the raw filtering of histogram metrics.
    let dsd_filtered_metrics = filter_metrics(dsd_metrics, is_histogram_metric);
    let adp_filtered_metrics = filter_metrics(adp_metrics, is_histogram_metric);

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
        .filter_map(|metric| {
            metric.context().name().rsplit_once('.').map(|(base_name, _)| base_name)
        })
        .collect::<HashSet<_>>();

    let adp_filtered_metrics = filter_metrics(adp_metrics, |metric| {
        histogram_metric_base_names.contains(metric.context().name())
    });

    info!("Filtered {} histogram metric(s) from DogStatsD, and {} histogram metric(s) from Agent Data Plane.", dsd_filtered_metrics.len(), adp_filtered_metrics.len());
}

fn merge_and_normalize_metrics(metrics: &mut Vec<Metric>) {
    // We sort the metrics by context (name and tags) so that we can then deduplicate them.
    //
    // During this, the act of merging the values also sorts and deduplicates the values, such that the values will be
    // sorted by timestamp, with all identical datapoints (identical by timestamp) merged.
    metrics.sort_by(|a, b| a.context().cmp(b.context()));
    metrics.dedup_by(|a, b| {
        // We merge `a` into `b` if they're equal, because `a` is the one that gets removed if they match.
        if a.context() == b.context() {
            b.merge_values(a);
            true
        } else {
            false
        }
    });

    // We also "anchor" the metric values, which involves adjusting the timestamps so that the first timestamp is 0,
    // thereby "anchoring" the metric to a fixed point in time. We do this to better compare flushed datapoints between
    // DSD and ADP.
    metrics.iter_mut().for_each(Metric::anchor_values); 
}

fn is_internal_telemetry(metric: &Metric) -> bool {
    metric.context().name().starts_with("datadog.") || metric.context().name().starts_with("n_o_i_n_d_e_x")
}

fn is_histogram_metric(metric: &Metric) -> bool {
    metric.context().name().ends_with(".min")
        || metric.context().name().ends_with(".max")
        || metric.context().name().ends_with(".avg")
        || metric.context().name().ends_with(".median")
        || metric.context().name().ends_with(".count")
        || metric.context().name().ends_with(".sum")
        || metric.context().name().ends_with(".95percentile")
}
