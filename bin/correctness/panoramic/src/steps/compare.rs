//! `compare`: two participants that offer `dump` hold the same metrics, traces, logs, events or service checks, as the
//! comparison analyzers judge it.
//!
//! The comparison is what a case checks ADP against the Agent with: each sends to its own intake, and the step compares
//! what the two intakes hold. Both dumps are kept, with the files collected from each intake, when the case fails.

use std::fmt::Write as _;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::time::Instant;

use super::analysis::{AnalysisMode, AnalysisRunner, LogsAnalysisOptions, TracesAnalysisOptions};
use super::intake::CollectedData;
use super::stopped;
use crate::core::handle::HandleError;
use crate::core::record::StepStatus;
use crate::core::spec::Keep;
use crate::core::step::{Need, Step, StepContext, StepFactory, StepFields, StepReport};

/// The operation the step calls on both participants.
const DUMP: &str = "dump";
/// The key of a `dump` answer that holds the DogStatsD packets forwarded to it.
const FORWARDED_PACKETS: &str = "dogstatsd_forwarded_packets";

/// The names the step keeps each dump and the full list of differences under.
const DUMP_FILE: &str = "dump.json";
const DIFFERENCES_FILE: &str = "differences.txt";

/// Most lines of the differences a failure message quotes.
const MAX_QUOTED: usize = 20;

pub(super) struct CompareFactory;

impl StepFactory for CompareFactory {
    fn name(&self) -> &'static str {
        "compare"
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        let Fields {
            telemetry,
            comparison,
            baseline,
            require_dogstatsd_forwarded_packets,
            otlp_direct_analysis_mode,
            additional_span_ignore_fields,
            logs,
        } = fields.decode()?;
        if comparison == baseline {
            return Err(format!("'comparison' and 'baseline' are both '{}'", comparison));
        }
        let only_for = |set: bool, option: &str, of: Telemetry| {
            if set && telemetry != of {
                Err(format!(
                    "'{}' is an option for {}, not {}",
                    option,
                    of.name(),
                    telemetry.name()
                ))
            } else {
                Ok(())
            }
        };
        only_for(
            otlp_direct_analysis_mode.is_some(),
            "otlp_direct_analysis_mode",
            Telemetry::Traces,
        )?;
        only_for(
            additional_span_ignore_fields.is_some(),
            "additional_span_ignore_fields",
            Telemetry::Traces,
        )?;
        only_for(logs.is_some(), "logs", Telemetry::Logs)?;
        Ok(Box::new(Compare {
            telemetry,
            comparison,
            baseline,
            require_dogstatsd_forwarded_packets,
            otlp_direct_analysis_mode: otlp_direct_analysis_mode.unwrap_or_default(),
            additional_span_ignore_fields: additional_span_ignore_fields.unwrap_or_default(),
            logs: logs.unwrap_or_default(),
        }))
    }
}

/// The step's fields. The options are the legacy comparison case's keys of the same names.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fields {
    telemetry: Telemetry,
    comparison: String,
    baseline: String,
    /// Fails unless the baseline and the comparison each forwarded at least one DogStatsD packet.
    #[serde(default)]
    require_dogstatsd_forwarded_packets: bool,
    /// Traces only: the baseline is OpenTelemetry-based, so trace stats are not compared.
    otlp_direct_analysis_mode: Option<bool>,
    /// Traces only: more span fields to leave out of the comparison.
    additional_span_ignore_fields: Option<Vec<String>>,
    /// Logs only: how logs are paired and counted, and the differences expected.
    logs: Option<LogsAnalysisOptions>,
}

/// The telemetry compared.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Telemetry {
    Events,
    Logs,
    Metrics,
    ServiceChecks,
    Traces,
}

impl Telemetry {
    /// The keys of a `dump` answer that hold this telemetry.
    fn keys(self) -> &'static [&'static str] {
        match self {
            Self::Events => &["events"],
            Self::Logs => &["log_requests"],
            Self::Metrics => &["metrics"],
            Self::ServiceChecks => &["service_checks"],
            Self::Traces => &["spans", "trace_stats"],
        }
    }

    fn mode(self) -> AnalysisMode {
        match self {
            Self::Events => AnalysisMode::Events,
            Self::Logs => AnalysisMode::Logs,
            Self::Metrics => AnalysisMode::Metrics,
            Self::ServiceChecks => AnalysisMode::ServiceChecks,
            Self::Traces => AnalysisMode::Traces,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Events => "events",
            Self::Logs => "logs",
            Self::Metrics => "metrics",
            Self::ServiceChecks => "service_checks",
            Self::Traces => "traces",
        }
    }
}

/// Passes when what `comparison` holds of `telemetry` matches what `baseline` holds.
#[derive(Debug)]
struct Compare {
    telemetry: Telemetry,
    comparison: String,
    baseline: String,
    require_dogstatsd_forwarded_packets: bool,
    otlp_direct_analysis_mode: bool,
    additional_span_ignore_fields: Vec<String>,
    logs: LogsAnalysisOptions,
}

#[async_trait]
impl Step for Compare {
    fn name(&self) -> &'static str {
        "compare"
    }

    fn needs(&self) -> Vec<Need> {
        vec![
            Need::Operation(self.comparison.clone(), DUMP.to_string()),
            Need::Operation(self.baseline.clone(), DUMP.to_string()),
        ]
    }

    /// No budget of its own: the step has what remains of the case's timeout.
    fn budget(&self) -> Duration {
        Duration::MAX
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let compared = async {
            let (comparison, baseline) =
                tokio::try_join!(self.dump(cx, &self.comparison), self.dump(cx, &self.baseline))?;
            let traces = TracesAnalysisOptions {
                otlp_direct_analysis_mode: self.otlp_direct_analysis_mode,
                additional_span_ignore_fields: self.additional_span_ignore_fields.clone(),
            };
            let runner = AnalysisRunner::new(self.telemetry.mode(), baseline, comparison, Some(traces))
                .with_logs_options(self.logs.clone())
                .with_dogstatsd_forwarding_requirement(self.require_dogstatsd_forwarded_packets);
            let report = match runner.run_analysis() {
                Ok(()) => StepReport {
                    status: StepStatus::Passed,
                    message: format!(
                        "No difference between the {} of '{}' (comparison) and '{}' (baseline).",
                        self.telemetry.name(),
                        self.comparison,
                        self.baseline
                    ),
                },
                Err((error, details)) => {
                    let mut message = format!("{:?}", error);
                    if !details.is_empty() {
                        let text = details.join("\n");
                        cx.keep(&self.comparison, DIFFERENCES_FILE, text.as_bytes(), Keep::Always)
                            .await;
                        let quoted = if details.len() > MAX_QUOTED {
                            format!("The first {} of {} lines of differences", MAX_QUOTED, details.len())
                        } else {
                            format!("The {} lines of differences", details.len())
                        };
                        let _ = write!(
                            message,
                            "\n{}, all kept as {} of '{}':",
                            quoted, DIFFERENCES_FILE, self.comparison
                        );
                        for line in details.iter().take(MAX_QUOTED) {
                            let _ = write!(message, "\n{}", line);
                        }
                    }
                    StepReport {
                        status: StepStatus::Failed,
                        message,
                    }
                }
            };
            Ok::<_, HandleError>(report)
        };
        compared.await.unwrap_or_else(stopped)
    }
}

impl Compare {
    /// What `on` holds, as its `dump` answers it. The answer is kept in case the case fails.
    async fn dump(&self, cx: &StepContext<'_>, on: &str) -> Result<CollectedData, HandleError> {
        let limit = cx.deadline.saturating_duration_since(Instant::now());
        let mut keys = self.telemetry.keys().to_vec();
        if self.require_dogstatsd_forwarded_packets {
            keys.push(FORWARDED_PACKETS);
        }
        let dump: Value = cx.handle(on)?.op(DUMP, json!(keys), limit).await?;
        cx.keep(on, DUMP_FILE, dump.to_string().as_bytes(), Keep::OnFailure)
            .await;
        serde_json::from_value(dump)
            .map_err(|e| HandleError::Harness(format!("the dump of '{}' does not decode: {}", on, e)))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::scripted::{observation, run_with_peer, Scripted, DUMP_FILE};
    use super::*;
    use crate::core::step::StepFields;

    const FIELDS: &str = "{telemetry: metrics, comparison: app, baseline: peer}";

    fn holding(value: f64) -> Scripted {
        let dump = json!({"metrics": [{
            "context": {"name": "app.count", "tags": []},
            "values": [[1, {"mtype": "Count", "value": value}]],
        }]})
        .to_string();
        Scripted {
            files: [(DUMP_FILE, Ok(dump.into_bytes()))].into(),
            ..Scripted::default()
        }
    }

    #[tokio::test]
    async fn a_difference_fails_and_a_dump_that_fails_errors() {
        let report = run_with_peer(&CompareFactory, FIELDS, &holding(2.0), &holding(3.0), &observation()).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(report.message.contains("did not match"), "{}", report.message);

        let unreachable = Scripted {
            files: [(
                DUMP_FILE,
                Err(HandleError::Harness("Docker refused the exec".to_string())),
            )]
            .into(),
            ..Scripted::default()
        };
        let report = run_with_peer(&CompareFactory, FIELDS, &holding(2.0), &unreachable, &observation()).await;
        assert_eq!(report.status, StepStatus::Errored, "{}", report.message);
        assert!(report.message.contains("Docker refused the exec"), "{}", report.message);
    }

    #[test]
    fn an_option_for_other_telemetry_is_rejected() {
        let fields = "{telemetry: metrics, comparison: app, baseline: peer, logs: {min_count: 1}}";
        let error = CompareFactory
            .build(&StepFields(serde_saphyr::from_str(fields).unwrap()))
            .unwrap_err();
        assert_eq!(error, "'logs' is an option for logs, not metrics");
    }
}
