use saluki_error::GenericError;
use serde::Deserialize;

mod collected;
pub use self::collected::CollectedData;

mod metrics;

/// Types of analysis to perform on collected data
#[derive(Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnalysisMode {
    /// Compares metrics between the baseline and comparison targets.
    Metrics,
}

/// Analysis runner.
pub struct AnalysisRunner {
    mode: AnalysisMode,
    baseline_data: CollectedData,
    comparison_data: CollectedData,
}

impl AnalysisRunner {
    /// Creates a new `AnalysisRunner` with the given analysis mode, baseline data, and comparison data.
    pub fn new(mode: AnalysisMode, baseline_data: CollectedData, comparison_data: CollectedData) -> Self {
        Self {
            mode,
            baseline_data,
            comparison_data,
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
        }
    }
}
