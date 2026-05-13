use saluki_error::GenericError;
use serde::Deserialize;

mod agent_telemetry;
mod collected;
pub use self::collected::CollectedData;

mod events;
mod metrics;
mod service_checks;
mod traces;

/// Types of analysis to perform on collected data
#[derive(Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisMode {
    /// Compares agent telemetry payloads between the baseline and comparison targets.
    ///
    /// Checks that both targets report the same set of metric names via the `agenttelemetry`
    /// component. Requires the agent to be configured with `agent_telemetry.logs_dd_url` pointing
    /// to the intake's HTTPS listener (port 2050) and `skip_ssl_validation: true`.
    AgentTelemetry,

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
            AnalysisMode::AgentTelemetry => {
                let analyzer = agent_telemetry::AgentTelemetryAnalyzer::new(&self.baseline_data, &self.comparison_data);
                analyzer.run_analysis()
            }
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
        }
    }
}
