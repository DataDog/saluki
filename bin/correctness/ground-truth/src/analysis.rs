use std::collections::HashSet;

use saluki_error::GenericError;
use stele::Metric;
use tracing::info;

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
        let mut dsd_metrics = self.dsd_metrics;
        let mut adp_metrics = self.adp_metrics;

        let dsd_unfiltered_metrics_len = dsd_metrics.len();
        let adp_unfiltered_metrics_len = adp_metrics.len();

        // Filter out internal telemetry metrics.
        dsd_metrics.retain(|metric| !is_internal_telemetry(metric));
        adp_metrics.retain(|metric| !is_internal_telemetry(metric));

        let dsd_filtered_metrics_len = dsd_unfiltered_metrics_len - dsd_metrics.len();
        let adp_filtered_metrics_len = adp_unfiltered_metrics_len - adp_metrics.len();

        info!("Received {} metrics ({} filtered out) from DogStatsD, and {} metrics ({} filtered out) from Agent Data Plane.", dsd_metrics.len(), dsd_filtered_metrics_len, adp_metrics.len(), adp_filtered_metrics_len);

        // Now we'll capture the unique metric series we received from both DSD and ADP, to make sure we're looking
        // at the exact same metrics before we start comparing their datapoints.
        //
        // We'll specifically sort the tags for each metric, since having them be in different orders is cancelled out
        // at the intake layer, and we want to emulate that.
        let mut dsd_unique_metrics = dsd_metrics
            .iter()
            .map(|metric| {
                let name = metric.name();
                let mut tags = metric.tags().to_vec();
                tags.sort();

                format!("{}{{{}}}", name, tags.join(","))
            })
            .collect::<Vec<_>>();

        let mut adp_unique_metrics = adp_metrics
            .iter()
            .map(|metric| {
                let name = metric.name();
                let mut tags = metric.tags().to_vec();
                tags.sort();

                format!("{}{{{}}}", name, tags.join(","))
            })
            .collect::<Vec<_>>();

        dsd_unique_metrics.sort();
        dsd_unique_metrics.dedup();

        adp_unique_metrics.sort();
        adp_unique_metrics.dedup();

        info!(
            "Received {} unique metrics from DogStatsD and {} unique metrics from Agent Data Plane.",
            dsd_unique_metrics.len(),
            adp_unique_metrics.len()
        );

        let dsd_metrics_set = dsd_unique_metrics.into_iter().collect::<HashSet<_>>();
        let adp_metrics_set = adp_unique_metrics.into_iter().collect::<HashSet<_>>();

        let metrics_in_dsd_not_in_adp = dsd_metrics_set.difference(&adp_metrics_set).collect::<Vec<_>>();
        let metrics_in_adp_not_in_dsd = adp_metrics_set.difference(&dsd_metrics_set).collect::<Vec<_>>();

        info!("Metrics in DogStatsD but not in Agent Data Plane:");
        for metric in metrics_in_dsd_not_in_adp {
            info!("  - {}", metric);
        }

        info!("Metrics in Agent Data Plane but not in DogStatsD:");
        for metric in metrics_in_adp_not_in_dsd {
            info!("  - {}", metric);
        }

        // TODO: actually merge our metrics together, based on their context (name and tags), and start evaluating the
        // value of the metrics themselves.

        Ok(())
    }
}

fn is_internal_telemetry(metric: &Metric) -> bool {
    metric.name().starts_with("datadog.") || metric.name().starts_with("n_o_i_n_d_e_x")
}
