//! `compare`: two participants that offer `dump` hold the same metrics, traces, logs, events or service checks, as the
//! comparison analyzers judge it.
//!
//! The comparison is what a case checks ADP against the Agent with: each sends to its own intake, and the step compares
//! what the two intakes hold. Both dumps are kept, with the files collected from each intake, when the case fails.
//!
//! Two intakes that hold nothing, or the same unrelated records, are equal. A case sets the `metrics` or `traces`
//! section to say what each intake must hold, and the step checks each side against it before it compares the sides.

use std::fmt::Write as _;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::time::Instant;

use super::analysis::{
    AnalysisMode, AnalysisRunner, LogsAnalysisOptions, MetricsExpectations, TracesAnalysisOptions, TracesOptions,
};
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
            traces,
            logs,
            metrics,
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
        only_for(traces.is_some(), "traces", Telemetry::Traces)?;
        only_for(metrics.is_some(), "metrics", Telemetry::Metrics)?;
        only_for(logs.is_some(), "logs", Telemetry::Logs)?;
        let otlp_direct_analysis_mode = otlp_direct_analysis_mode.unwrap_or_default();
        let traces = traces.unwrap_or_default();
        traces.validate(otlp_direct_analysis_mode)?;
        if let Some(metrics) = &metrics {
            metrics.validate()?;
        }
        Ok(Box::new(Compare {
            telemetry,
            comparison,
            baseline,
            require_dogstatsd_forwarded_packets,
            otlp_direct_analysis_mode,
            additional_span_ignore_fields: additional_span_ignore_fields.unwrap_or_default(),
            traces,
            logs: logs.unwrap_or_default(),
            metrics,
        }))
    }
}

/// The step's fields.
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
    /// Traces only: the differences expected, each with the gap and the issue that explain it.
    traces: Option<TracesOptions>,
    /// Logs only: how logs are paired and counted, and the differences expected.
    logs: Option<LogsAnalysisOptions>,
    /// Metrics only: what each side must hold, checked before the sides are compared.
    metrics: Option<MetricsExpectations>,
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
    traces: TracesOptions,
    logs: LogsAnalysisOptions,
    metrics: Option<MetricsExpectations>,
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
                known_differences: self.traces.known_differences.clone(),
            };
            let runner = AnalysisRunner::new(self.telemetry.mode(), baseline, comparison, Some(traces))
                .with_logs_options(self.logs.clone())
                .with_metrics_expectations(self.metrics.clone())
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

    use super::super::intake::fixtures::{count, gauge, metric_json};
    use super::super::scripted::{observation, run_with_peer, Scripted, DUMP_FILE};
    use super::*;
    use crate::core::step::StepFields;

    const FIELDS: &str = "{telemetry: metrics, comparison: app, baseline: peer}";

    /// An intake whose `dump` answers with `dump`.
    fn dumping(dump: Value) -> Scripted {
        Scripted {
            files: [(DUMP_FILE, Ok(dump.to_string().into_bytes()))].into(),
            ..Scripted::default()
        }
    }

    fn holding(value: f64) -> Scripted {
        dumping(json!({"metrics": [{
            "context": {"name": "app.count", "tags": []},
            "values": [[1, {"mtype": "Count", "value": value}]],
        }]}))
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

    #[test]
    fn a_traces_known_difference_names_an_issue_and_covers_only_stats() {
        let build = |options: &str, entry: &str| {
            let fields = format!(
                "{{telemetry: traces, comparison: app, baseline: peer, {} traces: {{known_differences: [{}]}}}}",
                options, entry
            );
            CompareFactory.build(&StepFields(serde_saphyr::from_str(&fields).unwrap()))
        };

        assert!(build("", "{gap: gap/a, issue: 1, paths: [stats.hits], reason: r}").is_ok());

        let error = build("", "{gap: gap/a, paths: [stats.hits], reason: r}").unwrap_err();
        assert_eq!(error, "known difference 'gap/a' names no 'issue'");

        let error = build("", "{gap: gap/a, issue: 1, paths: [meta.foo], reason: r}").unwrap_err();
        assert!(error.contains("only 'stats.*' paths"), "{}", error);

        // The mode skips the stats comparison, so every entry would be stale.
        let error = build(
            "otlp_direct_analysis_mode: true,",
            "{gap: gap/a, issue: 1, paths: [stats.hits], reason: r}",
        )
        .unwrap_err();
        assert!(error.contains("takes no 'known_differences'"), "{}", error);

        let fields = "{telemetry: metrics, comparison: app, baseline: peer, traces: {}}";
        let error = CompareFactory
            .build(&StepFields(serde_saphyr::from_str(fields).unwrap()))
            .unwrap_err();
        assert_eq!(error, "'traces' is an option for traces, not metrics");
    }

    /// What every case below expects of each side: two series, and the two fixture metrics by name.
    const EXPECTING_FIXTURE: &str = "{telemetry: metrics, comparison: app, baseline: peer, metrics: {min_series: 2, \
        series: [{name: fixture.requests}, {name: fixture.latency, metric_type: gauge, value: 3}]}}";

    /// An intake that holds `metrics`, and the internal telemetry that every Agent sends.
    fn with_internal(metrics: &[Value]) -> Scripted {
        let mut held = vec![metric_json("datadog.agent.running", &[], &[gauge(1.0)])];
        held.extend_from_slice(metrics);
        dumping(json!({"metrics": held}))
    }

    fn fixture() -> Vec<Value> {
        vec![
            metric_json("fixture.requests", &["env:test"], &[count(5.0)]),
            metric_json("fixture.latency", &["env:test"], &[gauge(3.0)]),
        ]
    }

    fn unrelated(names: &[&str]) -> Vec<Value> {
        names.iter().map(|name| metric_json(name, &[], &[count(1.0)])).collect()
    }

    async fn compare(fields: &str, comparison: &Scripted, baseline: &Scripted) -> StepReport {
        run_with_peer(&CompareFactory, fields, comparison, baseline, &observation()).await
    }

    #[tokio::test]
    async fn a_fixture_that_each_side_holds_passes_whatever_internal_telemetry_comes_with_it() {
        let comparison = with_internal(&fixture());
        let mut agent_metrics = fixture();
        agent_metrics.push(metric_json("datadog.agent.other", &[], &[gauge(9.0)]));
        let baseline = with_internal(&agent_metrics);

        let report = compare(EXPECTING_FIXTURE, &comparison, &baseline).await;
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);
    }

    #[tokio::test]
    async fn a_fixture_withheld_from_one_side_or_both_fails_and_names_each_side() {
        let held = with_internal(&fixture());
        let withheld = with_internal(&[]);
        // A side that sends nothing at all, not even internal telemetry.
        let silent = dumping(json!({"metrics": []}));

        let cases: [(&str, &Scripted, &Scripted, &[&str], Option<&str>); 4] = [
            (
                "the comparison",
                &withheld,
                &held,
                &["The comparison holds 0 series"],
                Some("The baseline holds"),
            ),
            (
                "the baseline",
                &held,
                &withheld,
                &["The baseline holds 0 series"],
                Some("The comparison holds"),
            ),
            (
                "both sides",
                &withheld,
                &withheld,
                &["The comparison holds 0 series", "The baseline holds 0 series"],
                None,
            ),
            (
                "a silent side",
                &silent,
                &held,
                &["The comparison holds 0 series"],
                Some("The baseline holds"),
            ),
        ];
        for (label, comparison, baseline, named, unnamed) in cases {
            let report = compare(EXPECTING_FIXTURE, comparison, baseline).await;
            assert_eq!(report.status, StepStatus::Failed, "{}: {}", label, report.message);
            for expected in named {
                assert!(report.message.contains(expected), "{}: {}", label, report.message);
            }
            assert!(
                report.message.contains("`min_series` is 2"),
                "{}: {}",
                label,
                report.message
            );
            assert!(report.message.contains("`series[0]`"), "{}: {}", label, report.message);
            if let Some(unnamed) = unnamed {
                assert!(!report.message.contains(unnamed), "{}: {}", label, report.message);
            }
        }

        // Without expectations, two sides that hold only internal telemetry are equal.
        let report = compare(FIELDS, &withheld, &withheld).await;
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);
    }

    #[tokio::test]
    async fn unrelated_series_that_meet_the_count_fail_the_named_check() {
        let comparison = with_internal(&unrelated(&["other.a", "other.b", "other.c"]));
        let baseline = with_internal(&fixture());

        let report = compare(EXPECTING_FIXTURE, &comparison, &baseline).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(!report.message.contains("`min_series`"), "{}", report.message);
        assert!(
            report.message.contains(
                "The comparison holds no series that meets `series[0]` (name=fixture.requests, tags_match=subset): \
                 no series has that name."
            ),
            "{}",
            report.message
        );
        assert!(
            report.message.contains("`series[1]` (name=fixture.latency"),
            "{}",
            report.message
        );
        assert!(!report.message.contains("The baseline holds"), "{}", report.message);
    }

    #[tokio::test]
    async fn a_series_with_the_name_and_the_wrong_value_fails_and_says_why() {
        let wrong = vec![
            metric_json("fixture.requests", &["env:test"], &[count(5.0)]),
            metric_json("fixture.latency", &["env:test"], &[gauge(4.0)]),
        ];
        let report = compare(EXPECTING_FIXTURE, &with_internal(&wrong), &with_internal(&wrong)).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(
            report
                .message
                .contains("1 series have that name: fixture.latency[env:test] (gauge) fails value: expected 3"),
            "{}",
            report.message
        );
        assert!(
            report.message.contains("The baseline holds no series"),
            "{}",
            report.message
        );
    }

    #[test]
    fn expectations_that_do_not_fit_the_step_are_rejected() {
        let cases = [
            (
                "{telemetry: logs, comparison: app, baseline: peer, metrics: {min_series: 1}}",
                "'metrics' is an option for metrics, not logs",
            ),
            (
                "{telemetry: metrics, comparison: app, baseline: peer, metrics: {}}",
                "'metrics' sets no expectation",
            ),
            (
                "{telemetry: metrics, comparison: app, baseline: peer, metrics: {min_series: 0}}",
                "invalid value: integer `0`",
            ),
            (
                "{telemetry: metrics, comparison: app, baseline: peer, metrics: {min_traces: 1}}",
                "unknown field `min_traces`",
            ),
            (
                "{telemetry: metrics, comparison: app, baseline: peer, metrics: {series: [{tags: [a]}]}}",
                "missing field `name`",
            ),
            (
                "{telemetry: metrics, comparison: app, baseline: peer, \
                 metrics: {series: [{name: m, metric_type: sketch, value: 1}]}}",
                "can't combine `value` with `metric_type: sketch`",
            ),
        ];
        for (fields, expected) in cases {
            let error = CompareFactory
                .build(&StepFields(serde_saphyr::from_str(fields).unwrap()))
                .unwrap_err();
            assert!(error.contains(expected), "{}: {}", fields, error);
        }
    }
}
