use saluki_error::{generic_error, GenericError};
use serde::Deserialize;

pub use self::expectations::TracesExpectations;
pub use self::logs::LogsAnalysisOptions;
pub use self::metrics::MetricsOptions;
use self::ratchet::KnownDifference;
pub use self::traces::{TracesOptions, TRACE_ID_FIELD};
use crate::steps::intake::CollectedData;

mod dogstatsd_forwarding;
mod events;
mod expectations;
mod logs;
mod metrics;
mod ratchet;
mod service_checks;
mod traces;

/// Types of analysis to perform on collected data
#[derive(Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisMode {
    /// Compares events between the baseline and comparison targets.
    Events,

    /// Compares logs between the baseline and comparison targets.
    Logs,

    /// Compares metrics between the baseline and comparison targets.
    Metrics,

    /// Compares service checks between the baseline and comparison targets.
    ServiceChecks,

    /// Compares traces between the baseline and comparison targets.
    Traces,
}

/// Options for traces analysis. Used when `AnalysisMode` is `Traces`.
pub struct TracesAnalysisOptions {
    /// If true, use OTLP-direct analysis (baseline is OTel-based): skip trace stats comparison and don't require SSI metadata.
    pub otlp_direct_analysis_mode: bool,

    /// Whether each side must hold a span with Single Step Instrumentation (SSI) metadata. Has no effect in OTLP-direct
    /// analysis, which never requires it.
    pub require_ssi: bool,

    /// Additional span field paths to ignore when diffing baseline vs comparison. Merged with the built-in list.
    pub additional_span_ignore_fields: Vec<String>,

    /// Trace statistics differences that are expected, each tagged with the gap and the issue that explain it. An
    /// entry that matches nothing fails the analysis.
    pub known_differences: Vec<KnownDifference>,

    /// What each side must hold, checked before the sides are compared. An unmet expectation fails the analysis, and the
    /// sides are still compared. A known difference never excuses an unmet expectation.
    pub expectations: TracesExpectations,
}

impl Default for TracesAnalysisOptions {
    fn default() -> Self {
        Self {
            otlp_direct_analysis_mode: false,
            require_ssi: true,
            additional_span_ignore_fields: Vec::new(),
            known_differences: Vec::new(),
            expectations: TracesExpectations::default(),
        }
    }
}

/// Analysis runner.
pub struct AnalysisRunner {
    mode: AnalysisMode,
    baseline_data: CollectedData,
    comparison_data: CollectedData,
    traces_options: Option<TracesAnalysisOptions>,
    logs_options: LogsAnalysisOptions,
    metrics_options: Option<MetricsOptions>,
    require_dogstatsd_forwarded_packets: bool,
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
            logs_options: LogsAnalysisOptions::default(),
            metrics_options: None,
            require_dogstatsd_forwarded_packets: false,
        }
    }

    /// Sets the options used when mode is `Logs`.
    pub fn with_logs_options(mut self, options: LogsAnalysisOptions) -> Self {
        self.logs_options = options;
        self
    }

    /// Sets what each side must hold, and the known differences, when mode is `Metrics`.
    pub fn with_metrics_options(mut self, options: Option<MetricsOptions>) -> Self {
        self.metrics_options = options;
        self
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
        match self.mode {
            AnalysisMode::Events => {
                let analyzer = events::EventsAnalyzer::new(&self.baseline_data, &self.comparison_data);
                analyzer.run_analysis()
            }
            AnalysisMode::Logs => {
                let analyzer = logs::LogsAnalyzer::new(&self.baseline_data, &self.comparison_data, self.logs_options);
                analyzer.run_analysis()
            }
            AnalysisMode::Metrics => {
                let analyzer =
                    metrics::MetricsAnalyzer::new(&self.baseline_data, &self.comparison_data, self.metrics_options)
                        .map_err(|e| (e, vec![]))?;
                analyzer.run_analysis()
            }
            AnalysisMode::ServiceChecks => {
                let analyzer = service_checks::ServiceChecksAnalyzer::new(&self.baseline_data, &self.comparison_data);
                analyzer.run_analysis()
            }
            AnalysisMode::Traces => {
                let opts = self.traces_options.unwrap_or_default();
                let analyzer = traces::TracesAnalyzer::new(&self.baseline_data, &self.comparison_data, opts)
                    .map_err(|e| (e, vec![]))?;
                analyzer.run_analysis()
            }
        }?;

        dogstatsd_forwarding::run_analysis(
            self.baseline_data.dogstatsd_forwarded_packets(),
            self.comparison_data.dogstatsd_forwarded_packets(),
            self.require_dogstatsd_forwarded_packets,
        )
    }
}

/// Joins the failures of an analysis into one, with each message on its own line and the details of each in order.
///
/// An analysis that finds more than one failure, such as an unmet expectation and a difference between the sides,
/// reports them all.
fn join_failures(
    failures: impl IntoIterator<Item = Option<(GenericError, Vec<String>)>>,
) -> Result<(), (GenericError, Vec<String>)> {
    let mut messages = Vec::new();
    let mut details = Vec::new();
    for (error, more) in failures.into_iter().flatten() {
        messages.push(error.to_string());
        details.extend(more);
    }
    if messages.is_empty() {
        Ok(())
    } else {
        Err((generic_error!("{}", messages.join("\n")), details))
    }
}
