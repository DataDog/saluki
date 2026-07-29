use saluki_error::GenericError;
use serde::Deserialize;

mod collected;
pub use self::collected::CollectedData;

mod dogstatsd_forwarding;
mod events;
mod expvars;
pub use self::expvars::ExpvarSnapshots;
mod metrics;
mod service_checks;
mod traces;

/// Types of analysis to perform on collected data
#[derive(Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisMode {
    /// Compares events between the baseline and comparison targets.
    Events,

    /// Compares metrics between the baseline and comparison targets.
    Metrics,

    /// Compares service checks between the baseline and comparison targets.
    ServiceChecks,

    /// Compares traces between the baseline and comparison targets.
    Traces,

    /// Compares numeric Core Agent expvar deltas between the baseline and comparison targets.
    Expvars,
}

/// Options for traces analysis. Used when `AnalysisMode` is `Traces`.
pub struct TracesAnalysisOptions {
    /// If true, use OTLP-direct analysis (baseline is OTel-based): skip trace stats comparison and don't require baseline SSI metadata.
    pub otlp_direct_analysis_mode: bool,

    /// Additional span field paths to ignore when diffing baseline vs comparison. Merged with the built-in list.
    pub additional_span_ignore_fields: Vec<String>,
}

/// Analysis runner.
pub struct AnalysisRunner {
    mode: AnalysisMode,
    baseline_data: Option<CollectedData>,
    comparison_data: Option<CollectedData>,
    expvar_snapshots: Option<(ExpvarSnapshots, ExpvarSnapshots)>,
    traces_options: Option<TracesAnalysisOptions>,
    require_dogstatsd_forwarded_packets: bool,
}

impl AnalysisRunner {
    /// Creates a new `AnalysisRunner` with the given analysis mode, baseline data, and comparison data.
    ///
    /// When mode is `Traces`, `traces_options` should be `Some(...)`; otherwise it's ignored.
    pub fn new(
        mode: AnalysisMode, baseline_data: CollectedData, comparison_data: CollectedData,
        traces_options: Option<TracesAnalysisOptions>,
    ) -> Result<Self, GenericError> {
        if matches!(mode, AnalysisMode::Expvars) {
            return Err(saluki_error::generic_error!(
                "Use AnalysisRunner::new_expvars for expvar analysis."
            ));
        }

        Ok(Self {
            mode,
            baseline_data: Some(baseline_data),
            comparison_data: Some(comparison_data),
            expvar_snapshots: None,
            traces_options,
            require_dogstatsd_forwarded_packets: false,
        })
    }

    /// Creates an analysis runner for Core Agent expvar snapshots.
    pub fn new_expvars(baseline: ExpvarSnapshots, comparison: ExpvarSnapshots) -> Self {
        Self {
            mode: AnalysisMode::Expvars,
            baseline_data: None,
            comparison_data: None,
            expvar_snapshots: Some((baseline, comparison)),
            traces_options: None,
            require_dogstatsd_forwarded_packets: false,
        }
    }

    /// Sets whether forwarded DogStatsD packets are required for this analysis run.
    pub const fn with_dogstatsd_forwarding_requirement(mut self, require_packets: bool) -> Self {
        self.require_dogstatsd_forwarded_packets = require_packets;
        self
    }

    /// Runs the configured analysis.
    ///
    /// # Errors
    ///
    /// If the analysis fails, or if the analysis identifies a difference between the baseline and comparison data,
    /// an error is returned alongside the full list of mismatch details (for log output).
    pub fn run_analysis(self) -> Result<(), (GenericError, Vec<String>)> {
        if let AnalysisMode::Expvars = self.mode {
            let (baseline, comparison) = self
                .expvar_snapshots
                .expect("expvar analysis runner must contain snapshots");
            let report = expvars::compare_snapshots(&baseline, &comparison);
            return if report.matches() {
                Ok(())
            } else {
                Err((saluki_error::generic_error!(report.summary()), vec![report.details()]))
            };
        }

        let baseline_data = self
            .baseline_data
            .as_ref()
            .expect("telemetry analysis runner must contain baseline data");
        let comparison_data = self
            .comparison_data
            .as_ref()
            .expect("telemetry analysis runner must contain comparison data");

        match self.mode {
            AnalysisMode::Events => {
                let analyzer = events::EventsAnalyzer::new(baseline_data, comparison_data);
                analyzer.run_analysis()
            }
            AnalysisMode::Metrics => {
                let analyzer =
                    metrics::MetricsAnalyzer::new(baseline_data, comparison_data).map_err(|e| (e, vec![]))?;
                analyzer.run_analysis()
            }
            AnalysisMode::ServiceChecks => {
                let analyzer = service_checks::ServiceChecksAnalyzer::new(baseline_data, comparison_data);
                analyzer.run_analysis()
            }
            AnalysisMode::Traces => {
                let opts = self.traces_options.unwrap_or(TracesAnalysisOptions {
                    otlp_direct_analysis_mode: false,
                    additional_span_ignore_fields: Vec::new(),
                });
                let analyzer =
                    traces::TracesAnalyzer::new(baseline_data, comparison_data, opts).map_err(|e| (e, vec![]))?;
                analyzer.run_analysis()
            }
            AnalysisMode::Expvars => unreachable!("expvar analysis returns before telemetry analysis"),
        }?;

        dogstatsd_forwarding::run_analysis(
            baseline_data.dogstatsd_forwarded_packets(),
            comparison_data.dogstatsd_forwarded_packets(),
            self.require_dogstatsd_forwarded_packets,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::AnalysisMode;

    #[test]
    fn expvars_analysis_mode_deserializes() {
        let mode = serde_yaml::from_str::<AnalysisMode>("expvars").expect("expvars analysis mode should deserialize");

        assert!(matches!(mode, AnalysisMode::Expvars));
    }
}
