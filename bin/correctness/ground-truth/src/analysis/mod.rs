use saluki_error::GenericError;
use serde::Deserialize;

mod collected;
pub use self::collected::CollectedData;

mod metrics;
mod traces;

/// Types of analysis to perform on collected data
#[derive(Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnalysisMode {
    /// Compares metrics between the baseline and comparison targets.
    Metrics,

    /// Compares traces between the baseline and comparison targets.
    Traces,
}

/// Options for traces analysis. Used when `AnalysisMode` is `Traces`.
pub struct TracesAnalysisOptions {
    /// If false, skip comparing trace statistics (APM stats aggregation keys).
    pub compare_trace_stats: bool,
    /// If false, do not require baseline spans to contain Single Step Instrumentation metadata.
    pub require_baseline_ssi: bool,
}

/// Analysis runner.
pub struct AnalysisRunner {
    mode: AnalysisMode,
    baseline_data: CollectedData,
    comparison_data: CollectedData,
    traces_options: Option<TracesAnalysisOptions>,
}

impl AnalysisRunner {
    /// Creates a new `AnalysisRunner` with the given analysis mode, baseline data, and comparison data.
    ///
    /// When mode is `Traces`, `traces_options` should be `Some(...)`; otherwise it is ignored.
    pub fn new(
        mode: AnalysisMode,
        baseline_data: CollectedData,
        comparison_data: CollectedData,
        traces_options: Option<TracesAnalysisOptions>,
    ) -> Self {
        Self {
            mode,
            baseline_data,
            comparison_data,
            traces_options,
        }
    }

    /// Runs the configured analysis.
    ///
    /// # Errors
    ///
    /// If the analysis fails, or if the analysis identifies a difference between the baseline and comparison data, an error is returned.
    pub fn run_analysis(self) -> Result<(), GenericError> {
        match self.mode {
            AnalysisMode::Metrics => {
                let analyzer = metrics::MetricsAnalyzer::new(&self.baseline_data, &self.comparison_data)?;
                analyzer.run_analysis()
            }
            AnalysisMode::Traces => {
                let opts = self.traces_options.unwrap_or(TracesAnalysisOptions {
                    compare_trace_stats: true,
                    require_baseline_ssi: true,
                });
                let analyzer =
                    traces::TracesAnalyzer::new(&self.baseline_data, &self.comparison_data, opts)?;
                analyzer.run_analysis()
            }
        }
    }
}
