use saluki_error::GenericError;
use serde::Deserialize;

mod collected;
pub use self::collected::CollectedData;

mod dogstatsd_forwarding;
pub use self::dogstatsd_forwarding::DogStatsDForwardingComparisonMode;
mod events;
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
    baseline_data: CollectedData,
    comparison_data: CollectedData,
    traces_options: Option<TracesAnalysisOptions>,
    require_dogstatsd_forwarded_packets: bool,
    require_dogstatsd_forwarded_packet_batches: bool,
    dogstatsd_forwarding_comparison_mode: DogStatsDForwardingComparisonMode,
}

impl AnalysisRunner {
    /// Creates a new `AnalysisRunner` with the given analysis mode, baseline data, and comparison data.
    ///
    /// When mode is `Traces`, `traces_options` should be `Some(...)`; otherwise it's ignored.
    pub fn new(
        mode: AnalysisMode, baseline_data: CollectedData, comparison_data: CollectedData,
        traces_options: Option<TracesAnalysisOptions>,
    ) -> Self {
        Self {
            mode,
            baseline_data,
            comparison_data,
            traces_options,
            require_dogstatsd_forwarded_packets: false,
            require_dogstatsd_forwarded_packet_batches: false,
            dogstatsd_forwarding_comparison_mode: DogStatsDForwardingComparisonMode::Packets,
        }
    }

    /// Sets whether forwarded DogStatsD packets are required for this analysis run.
    pub const fn with_dogstatsd_forwarding_requirement(mut self, require_packets: bool) -> Self {
        self.require_dogstatsd_forwarded_packets = require_packets;
        self
    }

    /// Sets whether forwarded DogStatsD packet batching is required for this analysis run.
    pub const fn with_dogstatsd_forwarding_batch_requirement(mut self, require_batches: bool) -> Self {
        self.require_dogstatsd_forwarded_packet_batches = require_batches;
        self
    }

    /// Sets how forwarded DogStatsD captures are compared for this analysis run.
    pub const fn with_dogstatsd_forwarding_comparison_mode(
        mut self, mode: DogStatsDForwardingComparisonMode,
    ) -> Self {
        self.dogstatsd_forwarding_comparison_mode = mode;
        self
    }

    /// Runs the configured analysis.
    ///
    /// # Errors
    ///
    /// If the analysis fails, or if the analysis identifies a difference between the baseline and comparison data,
    /// an error is returned alongside the full list of mismatch details (for log output).
    pub fn run_analysis(self) -> Result<(), (GenericError, Vec<String>)> {
        match self.mode {
            AnalysisMode::Events => {
                let analyzer = events::EventsAnalyzer::new(&self.baseline_data, &self.comparison_data);
                analyzer.run_analysis()
            }
            AnalysisMode::Metrics => {
                let analyzer = metrics::MetricsAnalyzer::new(&self.baseline_data, &self.comparison_data)
                    .map_err(|e| (e, vec![]))?;
                analyzer.run_analysis()
            }
            AnalysisMode::ServiceChecks => {
                let analyzer = service_checks::ServiceChecksAnalyzer::new(&self.baseline_data, &self.comparison_data);
                analyzer.run_analysis()
            }
            AnalysisMode::Traces => {
                let opts = self.traces_options.unwrap_or(TracesAnalysisOptions {
                    otlp_direct_analysis_mode: false,
                    additional_span_ignore_fields: Vec::new(),
                });
                let analyzer = traces::TracesAnalyzer::new(&self.baseline_data, &self.comparison_data, opts)
                    .map_err(|e| (e, vec![]))?;
                analyzer.run_analysis()
            }
        }?;

        dogstatsd_forwarding::run_analysis(
            self.baseline_data.dogstatsd_forwarded_packets(),
            self.comparison_data.dogstatsd_forwarded_packets(),
            self.require_dogstatsd_forwarded_packets,
            self.require_dogstatsd_forwarded_packet_batches,
            self.dogstatsd_forwarding_comparison_mode,
        )
    }
}
