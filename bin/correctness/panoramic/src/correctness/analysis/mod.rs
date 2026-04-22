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
    /// If true, use OTLP-direct analysis (baseline is OTel-based): skip trace stats comparison and do not require baseline SSI metadata.
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
}

impl AnalysisRunner {
    /// Creates a new `AnalysisRunner` with the given analysis mode, baseline data, and comparison data.
    ///
    /// When mode is `Traces`, `traces_options` should be `Some(...)`; otherwise it is ignored.
    pub fn new(
        mode: AnalysisMode, baseline_data: CollectedData, comparison_data: CollectedData,
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
    /// If the analysis fails, or if the analysis identifies a difference between the baseline and comparison data,
    /// an error is returned alongside the full list of mismatch details (for log output).
    pub fn run_analysis(self) -> Result<(), (GenericError, Vec<String>)> {
        match self.mode {
            AnalysisMode::Metrics => {
                let analyzer = metrics::MetricsAnalyzer::new(&self.baseline_data, &self.comparison_data)
                    .map_err(|e| (e, vec![]))?;
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
        }
    }
}
