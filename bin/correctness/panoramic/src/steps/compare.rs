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
    AnalysisMode, AnalysisRunner, LogsAnalysisOptions, MetricsOptions, TracesAnalysisOptions, TracesOptions,
    TRACE_ID_FIELD,
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
        if additional_span_ignore_fields
            .iter()
            .flatten()
            .any(|field| field == TRACE_ID_FIELD)
        {
            return Err(format!(
                "'additional_span_ignore_fields' can't hold '{}': a split trace shows up only there",
                TRACE_ID_FIELD
            ));
        }
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
    /// Traces only: what each side must hold, whether it needs Single Step Instrumentation metadata, and the
    /// differences expected, each with the gap and the issue that explain it.
    traces: Option<TracesOptions>,
    /// Logs only: how logs are paired and counted, and the differences expected.
    logs: Option<LogsAnalysisOptions>,
    /// Metrics only: what each side must hold, checked before the sides are compared, and the series whose type is
    /// expected to differ, each with the gap and the issue that explain it.
    metrics: Option<MetricsOptions>,
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
    metrics: Option<MetricsOptions>,
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
                require_ssi: self.traces.require_ssi.unwrap_or(true),
                additional_span_ignore_fields: self.additional_span_ignore_fields.clone(),
                known_differences: self.traces.known_differences.clone(),
                expectations: self.traces.expectations(),
            };
            let runner = AnalysisRunner::new(self.telemetry.mode(), baseline, comparison, Some(traces))
                .with_logs_options(self.logs.clone())
                .with_metrics_options(self.metrics.clone())
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
    fn a_traces_known_difference_names_an_issue_and_covers_stats_and_named_spans() {
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
        assert!(error.contains("only 'stats.*' and 'span.*' paths"), "{}", error);
        let error = build(
            "",
            "{gap: gap/a, issue: 1, paths: [spans.foo], when: {resource: [r]}, reason: r}",
        )
        .unwrap_err();
        assert!(
            error.contains("only 'stats.*' and 'span.*' paths, and 'spans'"),
            "{}",
            error
        );

        let spans_entry = "{gap: gap/a, issue: 1, paths: [spans], kinds: [missing], when: {resource: [r]}, reason: r}";
        assert!(build("", spans_entry).is_ok());
        assert!(build("otlp_direct_analysis_mode: true,", spans_entry).is_ok());
        assert!(build(
            "",
            "{gap: gap/a, issue: 1, paths: [spans], kinds: [extra], when: {service: [s], name: [n]}, reason: r}"
        )
        .is_ok());
        for unscoped in [
            "{gap: gap/a, issue: 1, paths: [spans], reason: r}",
            "{gap: gap/a, issue: 1, paths: [spans], when: {hits: [1]}, reason: r}",
        ] {
            let error = build("", unscoped).unwrap_err();
            assert!(
                error.contains("must name the spans by service, name, resource"),
                "{}",
                error
            );
        }

        // A known difference that covers whole spans names them by `name` or `resource`, and sets one of `missing`
        // and `extra`.
        for loose in [
            "{gap: gap/a, issue: 1, paths: [spans], when: {resource: [r]}, reason: r}",
            "{gap: gap/a, issue: 1, paths: [spans], kinds: [], when: {resource: [r]}, reason: r}",
            "{gap: gap/a, issue: 1, paths: [spans], kinds: [missing, extra], when: {resource: [r]}, reason: r}",
            "{gap: gap/a, issue: 1, paths: [spans], kinds: [value], when: {resource: [r]}, reason: r}",
            "{gap: gap/a, issue: 1, paths: [spans], kinds: [missing], when: {service: [s]}, reason: r}",
        ] {
            let error = build("", loose).unwrap_err();
            assert!(error.contains("covers whole spans"), "{}", error);
        }
        // A `span.` path is not a whole span, so it needs neither.
        assert!(build(
            "",
            "{gap: gap/a, issue: 1, paths: [span.metrics._top_level], when: {service: [s]}, reason: r}"
        )
        .is_ok());

        let span_entry = "{gap: gap/a, issue: 1, paths: [span.metrics._top_level], when: {resource: [r]}, reason: r}";
        assert!(build("", span_entry).is_ok());
        for unscoped in [
            "{gap: gap/a, issue: 1, paths: [span.metrics._top_level], reason: r}",
            "{gap: gap/a, issue: 1, paths: [span.metrics._top_level], when: {hits: [1]}, reason: r}",
        ] {
            let error = build("", unscoped).unwrap_err();
            assert!(
                error.contains("must name the spans by service, name, resource"),
                "{}",
                error
            );
        }

        // The mode skips the stats comparison, so a stats entry would be stale. It still compares spans.
        let error = build(
            "otlp_direct_analysis_mode: true,",
            "{gap: gap/a, issue: 1, paths: [stats.hits], reason: r}",
        )
        .unwrap_err();
        assert!(error.contains("can't cover a 'stats.*' path"), "{}", error);
        assert!(build("otlp_direct_analysis_mode: true,", span_entry).is_ok());

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

    /// A fixture that a case withholds from one side or both, and what the failure must say about it.
    struct Withheld<'a> {
        label: &'a str,
        comparison: &'a Scripted,
        baseline: &'a Scripted,
        /// Text that the failure holds, one entry for each side that lacks the fixture.
        names: &'a [&'a str],
        /// Text that the failure leaves out, because it names a side that holds the fixture.
        omits: Option<&'a str>,
    }

    impl Withheld<'_> {
        /// Asserts that `fields` fails on the sides, names each expected side, and holds every text of `always`.
        async fn assert_fails(&self, fields: &str, always: &[&str]) {
            let report = compare(fields, self.comparison, self.baseline).await;
            let label = self.label;
            assert_eq!(report.status, StepStatus::Failed, "{}: {}", label, report.message);
            for expected in self.names.iter().chain(always) {
                assert!(report.message.contains(expected), "{}: {}", label, report.message);
            }
            if let Some(omitted) = self.omits {
                assert!(!report.message.contains(omitted), "{}: {}", label, report.message);
            }
        }
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

        let cases = [
            Withheld {
                label: "the comparison",
                comparison: &withheld,
                baseline: &held,
                names: &["The comparison holds 0 series"],
                omits: Some("The baseline holds"),
            },
            Withheld {
                label: "the baseline",
                comparison: &held,
                baseline: &withheld,
                names: &["The baseline holds 0 series"],
                omits: Some("The comparison holds"),
            },
            Withheld {
                label: "both sides",
                comparison: &withheld,
                baseline: &withheld,
                names: &["The comparison holds 0 series", "The baseline holds 0 series"],
                omits: None,
            },
            Withheld {
                label: "a silent side",
                comparison: &silent,
                baseline: &held,
                names: &["The comparison holds 0 series"],
                omits: Some("The baseline holds"),
            },
        ];
        for case in cases {
            case.assert_fails(EXPECTING_FIXTURE, &["`min_series` is 2", "`series[0]`"])
                .await;
        }

        // The expectations fail two sides that hold only internal telemetry, and the comparison finds them equal.
        let report = compare(EXPECTING_FIXTURE, &withheld, &withheld).await;
        assert!(
            report
                .message
                .starts_with("Expectations are unmet: 3 on the baseline and 3 on the comparison.\n"),
            "{}",
            report.message
        );
        assert!(!report.message.contains("Mismatch"), "{}", report.message);

        // Without expectations, two sides that hold only internal telemetry are equal.
        let report = compare(FIELDS, &withheld, &withheld).await;
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);
    }

    #[tokio::test]
    async fn an_unmet_metrics_expectation_still_reports_the_differences_between_the_sides() {
        let baseline = with_internal(&fixture());

        // The comparison lacks a series that the baseline holds.
        let missing = with_internal(&fixture()[..1]);
        let report = compare(EXPECTING_FIXTURE, &missing, &baseline).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(
            report.message.starts_with(
                "Expectations are unmet: 0 on the baseline and 2 on the comparison.\n\
                 Mismatch in metrics pairs between baseline and comparison.\n"
            ),
            "{}",
            report.message
        );
        assert!(report.message.contains("`series[1]`"), "{}", report.message);
        assert!(
            report.message.contains("  - fixture.latency[env:test] (type: gauge)"),
            "{}",
            report.message
        );

        // The comparison holds a series with the wrong value.
        let wrong = with_internal(&[
            metric_json("fixture.requests", &["env:test"], &[count(5.0)]),
            metric_json("fixture.latency", &["env:test"], &[gauge(4.0)]),
        ]);
        let report = compare(EXPECTING_FIXTURE, &wrong, &baseline).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(
            report.message.starts_with(
                "Expectations are unmet: 0 on the baseline and 1 on the comparison.\n\
                 1 metrics from baseline and comparison did not match."
            ),
            "{}",
            report.message
        );
        assert!(
            report.message.contains("fixture.latency[env:test]"),
            "{}",
            report.message
        );
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

    /// What every trace case below expects of each side: two traces with three spans, two named spans, and two groups of
    /// stats. Stats are compared, and each side holds Single Step Instrumentation metadata, as the output of ADP and the
    /// Agent does.
    const EXPECTING_TRACES: &str = "{telemetry: traces, comparison: app, baseline: peer, \
        traces: {min_traces: 2, min_spans: 3, min_stats_groups: 2, \
        spans: [{service: api, name: web.request}, {service: db, resource: SELECT 1}]}}";

    /// The expectations of `EXPECTING_TRACES` for a step that does not require the metadata.
    const EXPECTING_TRACES_WITHOUT_SSI: &str = "{telemetry: traces, comparison: app, baseline: peer, \
        traces: {require_ssi: false, min_traces: 2, min_spans: 3, min_stats_groups: 2, \
        spans: [{service: api, name: web.request}, {service: db, resource: SELECT 1}]}}";

    /// Spans are equal across the sides only when they share a trace ID and span ID.
    fn span_json(service: &str, name: &str, resource: &str, trace_id: u64, span_id: u64, ssi: bool) -> Value {
        let meta = if ssi {
            json!({"_dd.install.id": "install"})
        } else {
            json!({})
        };
        json!({
            "agent_metadata": {
                "hostname": "host", "env": "", "tags": {}, "agent_version": "", "target_tps": 10.0, "error_tps": 10.0,
                "rare_sampler_enabled": false,
            },
            "tracer_metadata": {
                "container_id": "", "language_name": "", "language_version": "", "tracer_version": "",
                "runtime_id": "", "tags": {}, "env": "", "hostname": "", "app_version": "",
            },
            "trace_chunk_metadata": {"priority": 1, "origin": "", "tags": {}, "dropped_trace": false},
            "service": service, "name": name, "resource": resource, "trace_id": trace_id, "span_id": span_id,
            "parent_id": 0, "start": 0, "duration": 1, "error": 0, "meta": meta, "metrics": {}, "type_": "",
            "meta_struct": {}, "span_links": [], "span_events": [],
        })
    }

    /// An empty DDSketch with a relative accuracy of 0.01, as a dump encodes the sketches of stats.
    const EMPTY_SKETCH: &str = "CgkJ/UqBWr9S8D8=";

    /// One group of stats, as the aggregator holds it, keyed by `key`. Its sketches are empty.
    fn stats_group_json(key: u64, service: &str, name: &str, resource: &str) -> Value {
        json!({
            "agent_metadata": {"hostname": "host", "env": "", "agent_version": "", "client_computed": false},
            "tracer_metadata": {
                "hostname": "", "env": "", "version": "", "service": "", "language": "", "tracer_version": "",
                "runtime_id": "", "sequence": 0, "agent_aggregation": "", "container_id": "", "tags": [],
                "git_commit_sha": "", "image_tag": "", "process_tags_hash": 0, "process_tags": "",
            },
            "buckets": {"[0,10]": {
                "service": service, "name": name, "resource": resource, "type_": "", "hits": 1, "errors": 0,
                "duration_ns": 1, "ok_summary": EMPTY_SKETCH, "error_summary": EMPTY_SKETCH, "synthetics": false, "top_level_hits": 1,
                "span_kind": "", "peer_tags": [], "is_trace_root": null, "db_type": "", "grpc_status_code": "",
                "http_status_code": 0, "http_method": "", "http_endpoint": "",
            }},
        })
        .as_object()
        .map(|group| json!({key.to_string(): group}))
        .expect("a group is an object")
    }

    /// An intake that holds `spans`, and `groups` of stats.
    fn tracing(spans: Vec<Value>, groups: &[Value]) -> Scripted {
        let groups = groups
            .iter()
            .flat_map(|group| group.as_object().expect("a group is an object").clone())
            .collect::<serde_json::Map<_, _>>();
        dumping(json!({"spans": spans, "trace_stats": {"groups": groups}}))
    }

    fn fixture_spans(ssi: bool) -> Vec<Value> {
        vec![
            span_json("api", "web.request", "GET /", 1, 10, ssi),
            span_json("db", "db.query", "SELECT 1", 1, 11, ssi),
            span_json("api", "web.request", "GET /x", 2, 20, ssi),
        ]
    }

    fn fixture_groups() -> Vec<Value> {
        vec![
            stats_group_json(1, "api", "web.request", "GET /"),
            stats_group_json(2, "db", "db.query", "SELECT 1"),
        ]
    }

    fn fixture_traces() -> Scripted {
        tracing(fixture_spans(true), &fixture_groups())
    }

    #[tokio::test]
    async fn a_trace_fixture_that_each_side_holds_passes_with_stats_and_with_or_without_the_ssi_check() {
        let report = compare(EXPECTING_TRACES, &fixture_traces(), &fixture_traces()).await;
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);

        let without_ssi = tracing(fixture_spans(false), &fixture_groups());
        let report = compare(EXPECTING_TRACES_WITHOUT_SSI, &without_ssi, &without_ssi).await;
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);
    }

    #[tokio::test]
    async fn a_trace_fixture_withheld_from_one_side_or_both_fails_and_names_each_side() {
        let held = fixture_traces();
        let withheld = tracing(vec![], &[]);
        let cases = [
            Withheld {
                label: "the comparison",
                comparison: &withheld,
                baseline: &held,
                names: &["The comparison holds 0 traces, but `min_traces` is 2."],
                omits: Some("The baseline holds"),
            },
            Withheld {
                label: "the baseline",
                comparison: &held,
                baseline: &withheld,
                names: &["The baseline holds 0 traces, but `min_traces` is 2."],
                omits: Some("The comparison holds"),
            },
            Withheld {
                label: "both sides",
                comparison: &withheld,
                baseline: &withheld,
                names: &[
                    "The comparison holds 0 traces, but `min_traces` is 2.",
                    "The baseline holds 0 traces, but `min_traces` is 2.",
                ],
                omits: None,
            },
        ];
        for case in cases {
            case.assert_fails(
                EXPECTING_TRACES,
                &[
                    "`min_spans` is 3",
                    "`min_stats_groups` is 2",
                    "`spans[0]`",
                    "`spans[1]`",
                ],
            )
            .await;
        }

        // Without expectations, a spans-only comparison finds two sides that hold nothing equal.
        let report = compare(
            "{telemetry: traces, comparison: app, baseline: peer, otlp_direct_analysis_mode: true}",
            &withheld,
            &withheld,
        )
        .await;
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);
    }

    #[tokio::test]
    async fn a_side_that_holds_more_traces_or_spans_than_the_limits_fails_and_names_the_side() {
        // The fixture holds 2 traces and 3 spans, and the limits allow exactly that.
        let fixture_with_spans = |extra: &[Value]| {
            let mut spans = fixture_spans(true);
            spans.extend_from_slice(extra);
            tracing(spans, &fixture_groups())
        };
        let fields = "{telemetry: traces, comparison: app, baseline: peer, \
            traces: {max_traces: 2, max_spans: 3, min_stats_groups: 2}}";

        let report = compare(fields, &fixture_traces(), &fixture_traces()).await;
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);

        // A span of a third trace goes over both limits, and a span of a known trace goes over the span limit only.
        let third_trace = fixture_with_spans(&[span_json("api", "web.request", "GET /y", 3, 30, true)]);
        let fourth_span = fixture_with_spans(&[span_json("api", "web.request", "GET /y", 2, 30, true)]);
        for (label, side, expected, omitted) in [
            (
                "a third trace",
                &third_trace,
                &[
                    "The comparison holds 3 traces, but `max_traces` is 2.",
                    "The comparison holds 4 spans, but `max_spans` is 3.",
                ][..],
                None,
            ),
            (
                "a fourth span",
                &fourth_span,
                &["The comparison holds 4 spans, but `max_spans` is 3."][..],
                Some("`max_traces`"),
            ),
        ] {
            let report = compare(fields, side, &fixture_traces()).await;
            assert_eq!(report.status, StepStatus::Failed, "{}: {}", label, report.message);
            for expected in expected {
                assert!(report.message.contains(expected), "{}: {}", label, report.message);
            }
            if let Some(omitted) = omitted {
                assert!(!report.message.contains(omitted), "{}: {}", label, report.message);
            }
            assert!(
                !report.message.contains("The baseline holds"),
                "{}: {}",
                label,
                report.message
            );
        }

        // Both sides can be over a limit: the equality of the sides does not excuse it.
        let report = compare(fields, &third_trace, &third_trace).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        for side in ["comparison", "baseline"] {
            let expected = format!("The {} holds 3 traces, but `max_traces` is 2.", side);
            assert!(report.message.contains(&expected), "{}: {}", side, report.message);
        }
    }

    #[tokio::test]
    async fn unrelated_spans_that_meet_the_counts_fail_the_named_check() {
        let unrelated = tracing(
            vec![
                span_json("other", "other.a", "a", 1, 10, true),
                span_json("other", "other.b", "b", 1, 11, true),
                span_json("api", "other.c", "c", 2, 20, true),
                span_json("db", "other.d", "SELECT 2", 3, 30, true),
            ],
            &fixture_groups(),
        );

        let report = compare(EXPECTING_TRACES, &unrelated, &fixture_traces()).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        for count in ["`min_traces`", "`min_spans`", "`min_stats_groups`"] {
            assert!(!report.message.contains(count), "{}", report.message);
        }
        assert!(
            report.message.contains(
                "The comparison holds no span that meets `spans[0]` (service=api, name=web.request): \
                 1 span(s) have that service, with the names other.c."
            ),
            "{}",
            report.message
        );
        assert!(
            report.message.contains(
                "The comparison holds no span that meets `spans[1]` (service=db, resource=SELECT 1): \
                 1 span(s) have that service, with the resources SELECT 2."
            ),
            "{}",
            report.message
        );
        assert!(!report.message.contains("The baseline holds"), "{}", report.message);
    }

    #[tokio::test]
    async fn an_unmet_traces_expectation_still_reports_the_differences_between_the_sides() {
        let mut spans = fixture_spans(true);
        spans[1] = span_json("db", "db.query", "SELECT 2", 1, 11, true);
        let comparison = tracing(spans, &fixture_groups());

        let report = compare(EXPECTING_TRACES, &comparison, &fixture_traces()).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(
            report.message.starts_with(
                "Expectations are unmet: 0 on the baseline and 1 on the comparison.\n\
                 1 mismatched span(s)/stats group(s) between baseline and comparison."
            ),
            "{}",
            report.message
        );
        assert!(report.message.contains("`spans[1]`"), "{}", report.message);
        assert!(
            report
                .message
                .contains(r#"resource: baseline="SELECT 1" comparison="SELECT 2""#),
            "{}",
            report.message
        );
    }

    #[tokio::test]
    async fn spans_without_the_expected_stats_groups_fail_the_stats_check_with_the_ssi_check_on_or_off() {
        let no_stats = tracing(fixture_spans(true), &[]);
        let no_stats_without_ssi = tracing(fixture_spans(false), &[]);

        for (label, fields, side) in [
            ("ssi required", EXPECTING_TRACES, &no_stats),
            ("ssi not required", EXPECTING_TRACES_WITHOUT_SSI, &no_stats_without_ssi),
        ] {
            let report = compare(fields, side, side).await;
            assert_eq!(report.status, StepStatus::Failed, "{}: {}", label, report.message);
            for name in ["comparison", "baseline"] {
                let expected = format!("The {} holds 0 stats groups, but `min_stats_groups` is 2.", name);
                assert!(report.message.contains(&expected), "{}: {}", label, report.message);
            }
            for met in ["`min_traces`", "`min_spans`", "`spans["] {
                assert!(!report.message.contains(met), "{}: {}", label, report.message);
            }
        }

        // Stats missing from one side name that side only.
        let report = compare(EXPECTING_TRACES, &no_stats, &fixture_traces()).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(
            report.message.contains("The comparison holds 0 stats groups"),
            "{}",
            report.message
        );
        assert!(!report.message.contains("The baseline holds"), "{}", report.message);

        // Without expectations, the same two sides are equal: only `min_stats_groups` fails them.
        let report = compare(
            "{telemetry: traces, comparison: app, baseline: peer}",
            &no_stats,
            &no_stats,
        )
        .await;
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);
    }

    #[tokio::test]
    async fn ssi_metadata_is_required_unless_the_case_opts_out() {
        let without = tracing(fixture_spans(false), &fixture_groups());
        let with = tracing(fixture_spans(true), &fixture_groups());
        let by_default = "{telemetry: traces, comparison: app, baseline: peer}";
        let opted_out =
            "{telemetry: traces, comparison: app, baseline: peer, traces: {require_ssi: false, min_stats_groups: 2}}";
        let direct = "{telemetry: traces, comparison: app, baseline: peer, otlp_direct_analysis_mode: true}";

        let report = compare(by_default, &without, &without).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        let report = compare(by_default, &with, &with).await;
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);
        let report = compare(opted_out, &without, &without).await;
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);
        let report = compare(direct, &without, &without).await;
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);
    }

    #[tokio::test]
    async fn a_known_difference_never_satisfies_the_stats_expectation() {
        let known = "known_differences: [{gap: gap/no-stats, issue: 1, paths: [stats.groups], kinds: [missing], \
            reason: ADP sends no stats}]";
        let without_expectations = format!(
            "{{telemetry: traces, comparison: app, baseline: peer, traces: {{{}}}}}",
            known
        );
        let expecting = format!(
            "{{telemetry: traces, comparison: app, baseline: peer, \
             traces: {{min_stats_groups: 2, {}}}}}",
            known
        );
        let no_stats = tracing(fixture_spans(true), &[]);

        // The gap covers every group that the comparison lacks, so the sides match.
        let report = compare(&without_expectations, &no_stats, &fixture_traces()).await;
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);

        // The expectation counts the groups that a side holds. The gap does not excuse a side that holds too few.
        let report = compare(&expecting, &no_stats, &fixture_traces()).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(
            report
                .message
                .contains("The comparison holds 0 stats groups, but `min_stats_groups` is 2."),
            "{}",
            report.message
        );
        assert!(!report.message.contains("The baseline holds"), "{}", report.message);

        // When both sides meet the expectation, the step checks the known difference as before. This one is stale.
        let report = compare(&expecting, &fixture_traces(), &fixture_traces()).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(report.message.contains("gap/no-stats"), "{}", report.message);
        assert!(!report.message.contains("`min_stats_groups`"), "{}", report.message);
    }

    /// The fixture spans, with the `metrics` of the span `span_id` set to `metrics`.
    fn spans_with_metrics(span_id: u64, metrics: Value) -> Vec<Value> {
        let mut spans = fixture_spans(false);
        for span in &mut spans {
            if span["span_id"] == span_id {
                span["metrics"] = metrics.clone();
            }
        }
        spans
    }

    #[tokio::test]
    async fn a_known_span_difference_covers_only_its_field_on_the_named_spans() {
        let fields = "{telemetry: traces, comparison: app, baseline: peer, traces: {require_ssi: false, \
            min_stats_groups: 2, known_differences: [{gap: gap/top-level, issue: 1, paths: [span.metrics._top_level], \
            kinds: [missing], when: {resource: [SELECT 1]}, reason: no top-level mark}]}}";
        let baseline = tracing(spans_with_metrics(11, json!({"_top_level": 1.0})), &fixture_groups());

        // The comparison lacks the mark on the named span, as the entry says.
        let report = compare(fields, &fixture_traces(), &baseline).await;
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);

        // Another field of the same span still fails, and the failure names the span.
        let comparison = tracing(spans_with_metrics(11, json!({"_dd.measured": 1.0})), &fixture_groups());
        let report = compare(fields, &comparison, &baseline).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(
            report
                .message
                .contains("span 11 (service=\"db\" name=\"db.query\" resource=\"SELECT 1\")"),
            "{}",
            report.message
        );
        assert!(report.message.contains("metrics._dd.measured"), "{}", report.message);
        assert!(!report.message.contains("metrics._top_level"), "{}", report.message);

        // The same field on a span the entry does not name fails.
        let baseline = tracing(spans_with_metrics(10, json!({"_top_level": 1.0})), &fixture_groups());
        let report = compare(fields, &fixture_traces(), &baseline).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(
            report.message.contains("span 10 (service=\"api\""),
            "{}",
            report.message
        );
        // The entry matched nothing, so it is stale too.
        assert!(report.message.contains("gap/top-level (#1)"), "{}", report.message);

        // Once the difference is gone, the entry is stale, and the failure lists it under its own heading.
        let report = compare(fields, &fixture_traces(), &fixture_traces()).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(
            report.message.contains("(1 known difference path(s) no longer occur)"),
            "{}",
            report.message
        );
        assert!(report.message.contains("no longer occurs"), "{}", report.message);
        assert!(!report.message.contains("stats group(s))"), "{}", report.message);
    }

    #[test]
    fn a_traces_known_difference_cannot_cover_the_trace_id() {
        let build = |path: &str| {
            let fields = format!(
                "{{telemetry: traces, comparison: app, baseline: peer, traces: {{known_differences: [{{gap: g, \
                 issue: 1, paths: ['{}'], when: {{service: [api]}}, reason: r}}]}}}}",
                path
            );
            CompareFactory
                .build(&StepFields(serde_saphyr::from_str(&fields).unwrap()))
                .map(|_| ())
        };

        for path in ["span.trace_id", "span.*", "span.**", "span.**.trace_id"] {
            let error = build(path).unwrap_err();
            assert!(error.contains("which covers 'span.trace_id'"), "{}: {}", path, error);
        }
        for path in ["span.metrics.*", "span.meta.**", "span.parent_id"] {
            assert!(build(path).is_ok(), "{}", path);
        }
    }

    #[tokio::test]
    async fn a_split_trace_fails_on_the_trace_id_of_the_spans_on_both_sides() {
        // The comparison puts span 11 in a trace of its own: the span IDs match, and the trace counts differ.
        let mut split = fixture_spans(true);
        split[1]["trace_id"] = json!(3);

        let report = compare(
            "{telemetry: traces, comparison: app, baseline: peer}",
            &tracing(split, &fixture_groups()),
            &fixture_traces(),
        )
        .await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(
            report.message.contains(
                "span 11 (service=\"db\" name=\"db.query\" resource=\"SELECT 1\"):\n    trace_id: baseline=1 \
                 comparison=3"
            ),
            "{}",
            report.message
        );
    }

    #[tokio::test]
    async fn a_span_on_one_side_only_leaves_the_other_spans_and_the_stats_compared() {
        let fields = "{telemetry: traces, comparison: app, baseline: peer, traces: {require_ssi: false, \
            min_stats_groups: 1, known_differences: [{gap: gap/top-level, issue: 1, paths: [span.metrics._top_level], \
            kinds: [missing], when: {resource: [GET /]}, reason: no top-level mark}]}}";
        let mut baseline_spans = spans_with_metrics(10, json!({"_top_level": 1.0}));
        baseline_spans.push(span_json("api", "web.request", "GET /lost", 3, 30, false));
        let baseline = tracing(baseline_spans, &fixture_groups());

        // The comparison lacks one span and holds another, a field of a span on both sides differs, and so does a
        // stats group. The known difference still covers only its field.
        let mut comparison_spans = spans_with_metrics(11, json!({"_dd.measured": 1.0}));
        comparison_spans.push(span_json("api", "web.request", "GET /new", 4, 40, false));
        let comparison = tracing(
            comparison_spans,
            &[
                stats_group_json(1, "api", "web.request", "GET /"),
                stats_group_json(3, "db", "db.query", "SELECT 2"),
            ],
        );

        let report = compare(fields, &comparison, &baseline).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        for expected in [
            "span 11 (service=\"db\" name=\"db.query\" resource=\"SELECT 1\"):\n    metrics._dd.measured: \
             baseline=<missing> comparison=1.0",
            "1 span(s) only in baseline, 1 only in comparison.",
            "span 30 (trace_id=3 service=\"api\" name=\"web.request\" resource=\"GET /lost\"):\n    only in \
             baseline",
            "span 40 (trace_id=4 service=\"api\" name=\"web.request\" resource=\"GET /new\"):\n    only in \
             comparison",
            "stats group service=\"db\" name=\"db.query\" resource=\"SELECT 1\":\n    only in baseline",
            "stats group service=\"db\" name=\"db.query\" resource=\"SELECT 2\":\n    only in comparison",
        ] {
            assert!(report.message.contains(expected), "{}:\n{}", expected, report.message);
        }
        assert!(!report.message.contains("span 10"), "{}", report.message);
        assert!(!report.message.contains("no longer occurs"), "{}", report.message);

        // The spans on one side only come before the spans whose fields differ, so the samples keep them.
        let position = |subject: &str| report.message.find(subject).expect("the message names the span");
        assert!(position("span 30 ") < position("span 11 "), "{}", report.message);
        assert!(position("span 40 ") < position("span 11 "), "{}", report.message);
    }

    #[tokio::test]
    async fn a_known_span_difference_is_stale_for_each_named_span_that_no_longer_differs() {
        let fields = "{telemetry: traces, comparison: app, baseline: peer, traces: {require_ssi: false, \
            min_stats_groups: 2, known_differences: [{gap: gap/top-level, issue: 1, paths: [span.metrics._top_level], \
            kinds: [missing], when: {resource: [GET /, SELECT 1]}, reason: no top-level mark}]}}";
        let baseline = tracing(spans_with_metrics(10, json!({"_top_level": 1.0})), &fixture_groups());

        let report = compare(fields, &fixture_traces(), &baseline).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(
            report.message.contains(
                "path `span.metrics._top_level` when `resource` is \"SELECT 1\" (no top-level mark). Remove it \
                 from the case."
            ),
            "{}",
            report.message
        );
        assert!(!report.message.contains("is \"GET /\""), "{}", report.message);
    }

    /// A spans-only step that expects `traces`.
    fn direct(traces: &str) -> String {
        format!(
            "{{telemetry: traces, comparison: app, baseline: peer, otlp_direct_analysis_mode: true, \
             traces: {{{}}}}}",
            traces
        )
    }

    /// The fixture spans without the span `span_id`.
    fn spans_without(span_id: u64) -> Vec<Value> {
        let mut spans = fixture_spans(false);
        spans.retain(|span| span["span_id"] != span_id);
        spans
    }

    #[tokio::test]
    async fn a_span_that_a_side_holds_fails_the_absent_check_and_names_the_side() {
        let both = tracing(fixture_spans(false), &[]);

        // Neither side holds the span, and each holds the named sibling.
        let fields = direct("spans: [{service: api, name: web.request}], absent: [{service: api, name: dropped}]");
        let report = compare(&fields, &both, &both).await;
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);

        // Both sides hold the two spans of the service.
        let fields = direct(
            "spans: [{service: db, name: db.query}, {service: api, name: web.request}], \
             absent: [{service: db, name: db.query}, {service: api, name: web.request}]",
        );
        let report = compare(&fields, &both, &both).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        for side in ["comparison", "baseline"] {
            for expected in [
                format!(
                    "The {} holds 1 span(s) that meet `absent[0]` (service=db, name=db.query), but it must hold \
                     none.",
                    side
                ),
                format!(
                    "The {} holds 2 span(s) that meet `absent[1]` (service=api, name=web.request), but it must hold \
                     none.",
                    side
                ),
            ] {
                assert!(report.message.contains(&expected), "{}: {}", expected, report.message);
            }
        }

        // One side holds the span: only that side is named, and the entry matches the resource too.
        let without_x = tracing(spans_without(20), &[]);
        let fields = direct("spans: [{service: api, resource: GET /}], absent: [{service: api, resource: GET /x}]");
        let report = compare(&fields, &both, &without_x).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(
            report
                .message
                .contains("The comparison holds 1 span(s) that meet `absent[0]` (service=api, resource=GET /x)"),
            "{}",
            report.message
        );
        assert!(!report.message.contains("The baseline holds"), "{}", report.message);

        // A span of the same service with another resource does not meet the entry.
        let fields =
            direct("spans: [{service: api, name: web.request}], absent: [{service: api, resource: GET /missing}]");
        let report = compare(&fields, &both, &both).await;
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);
    }

    /// The fixture spans with `meta` on the span `span_id`.
    fn spans_with_meta(span_id: u64, meta: Value) -> Vec<Value> {
        let mut spans = fixture_spans(false);
        for span in spans.iter_mut().filter(|span| span["span_id"] == span_id) {
            span["meta"] = meta.clone();
        }
        spans
    }

    #[tokio::test]
    async fn a_meta_tag_that_each_side_holds_as_expected_passes() {
        let scrubbed = tracing(spans_with_meta(11, json!({"sql.query": "SELECT ?"})), &[]);

        let fields = direct(
            "spans: [{service: db, name: db.query, meta: {sql.query: 'SELECT ?'}}, \
             {service: db, meta: {sql.query: {equals: 'SELECT ?', not_contains: canary}}}, \
             {service: api, name: web.request}]",
        );
        let report = compare(&fields, &scrubbed, &scrubbed).await;
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);
    }

    #[tokio::test]
    async fn a_meta_tag_that_a_side_leaks_fails_the_spans_check_and_shows_the_value_of_each_side() {
        let scrubbed = tracing(spans_with_meta(11, json!({"sql.query": "SELECT ?"})), &[]);
        let leaked = tracing(spans_with_meta(11, json!({"sql.query": "SELECT 'canary'"})), &[]);
        let fields = direct("spans: [{service: db, name: db.query, meta: {sql.query: 'SELECT ?'}}]");

        // One side leaks: only that side is named, with the value that it holds and the value that it must hold.
        let report = compare(&fields, &leaked, &scrubbed).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(
            report.message.contains(
                "The comparison holds no span that meets `spans[0]` (service=db, name=db.query, meta.sql.query = \
                 SELECT ?): 1 span(s) have that service, name and resource, and 1 fail `meta`: span 11 has \
                 `meta.sql.query` is `SELECT 'canary'`, not `SELECT ?`."
            ),
            "{}",
            report.message
        );
        assert!(!report.message.contains("The baseline holds"), "{}", report.message);

        // Both sides leak the same value: the equality of the sides does not excuse it, and both are named.
        let report = compare(&fields, &leaked, &leaked).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        for side in ["baseline", "comparison"] {
            assert!(
                report
                    .message
                    .contains(&format!("The {} holds no span that meets `spans[0]`", side)),
                "{}: {}",
                side,
                report.message
            );
        }
    }

    #[tokio::test]
    async fn a_meta_tag_that_holds_a_forbidden_part_or_is_missing_fails() {
        let leaked = tracing(spans_with_meta(11, json!({"url": "https://user:canary@host/"})), &[]);
        let fields = direct("spans: [{service: db, meta: {url: {not_contains: canary}}}]");
        let report = compare(&fields, &leaked, &leaked).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(
            report
                .message
                .contains("`meta.url` is `https://user:canary@host/`, and holds `canary`"),
            "{}",
            report.message
        );

        // A tag that a side drops fails too, so a misspelled key does not pass for a scrubbed one.
        let dropped = tracing(fixture_spans(false), &[]);
        let report = compare(&fields, &dropped, &dropped).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(
            report.message.contains("span 11 has `meta.url` is missing"),
            "{}",
            report.message
        );
    }

    #[tokio::test]
    async fn a_long_meta_value_is_cut_in_the_failure() {
        let long = "x".repeat(500);
        let held = tracing(spans_with_meta(11, json!({"body": long})), &[]);
        let fields = direct("spans: [{service: db, meta: {body: short}}]");
        let report = compare(&fields, &held, &held).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(
            report
                .message
                .contains(&format!("is `{}...`, not `short`", "x".repeat(160))),
            "{}",
            report.message
        );
        assert!(!report.message.contains(&"x".repeat(161)), "{}", report.message);

        // A value of several lines stays on one line.
        let held = tracing(spans_with_meta(11, json!({"body": "GET a\nDEL a"})), &[]);
        let report = compare(&fields, &held, &held).await;
        assert!(
            report.message.contains("is `GET a\\nDEL a`, not `short`"),
            "{}",
            report.message
        );
    }

    #[tokio::test]
    async fn an_entry_with_a_side_holds_only_that_side() {
        let scrubbed = tracing(spans_with_meta(11, json!({"db.statement": "SELECT ?"})), &[]);
        let leaked = tracing(spans_with_meta(11, json!({"db.statement": "SELECT 'canary'"})), &[]);

        // The baseline holds the scrubbed value, and the comparison leaks it, as a known difference says.
        let baseline_only = direct("spans: [{service: db, side: baseline, meta: {db.statement: 'SELECT ?'}}]");
        let report = compare(&baseline_only, &leaked, &scrubbed).await;
        assert!(
            !report.message.contains("The comparison holds no span"),
            "{}",
            report.message
        );
        assert!(
            !report.message.contains("The baseline holds no span"),
            "{}",
            report.message
        );

        // The same entry fails when the baseline is the side that leaks.
        let report = compare(&baseline_only, &scrubbed, &leaked).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(
            report.message.contains(
                "The baseline holds no span that meets `spans[0]` (service=db, meta.db.statement = SELECT ?)"
            ),
            "{}",
            report.message
        );
        assert!(
            !report.message.contains("The comparison holds no span"),
            "{}",
            report.message
        );
    }

    #[tokio::test]
    async fn an_entry_for_one_side_fails_once_the_other_side_meets_it_too() {
        let scrubbed = tracing(spans_with_meta(11, json!({"db.statement": "SELECT ?"})), &[]);
        let leaked = tracing(spans_with_meta(11, json!({"db.statement": "SELECT 'canary'"})), &[]);
        let fields = direct("spans: [{service: db, side: baseline, meta: {db.statement: 'SELECT ?'}}]");

        // The comparison leaks, so the entry still sets the sides apart. Only the difference between the sides fails.
        let report = compare(&fields, &leaked, &scrubbed).await;
        assert!(!report.message.contains("Drop `side`"), "{}", report.message);
        assert!(!report.message.contains("holds no span"), "{}", report.message);

        // The comparison scrubs too, so the entry no longer needs `side`.
        let report = compare(&fields, &scrubbed, &scrubbed).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(
            report.message.contains(
                "The comparison meets `spans[0]` (service=db, meta.db.statement = SELECT ?) too, but the entry is \
                 `side: baseline`. Drop `side`"
            ),
            "{}",
            report.message
        );
        assert!(!report.message.contains("The baseline"), "{}", report.message);
    }

    #[tokio::test]
    async fn a_meta_expectation_holds_every_span_that_it_selects() {
        let scrub = |extra: Option<&str>| {
            let mut spans = spans_with_meta(11, json!({"sql.query": "SELECT ?"}));
            if let Some(query) = extra {
                // A duplicate of the span, in the same service with the same name and resource.
                let mut duplicate = span_json("db", "db.query", "SELECT 1", 1, 12, false);
                duplicate["meta"] = json!({"sql.query": query});
                spans.push(duplicate);
            }
            tracing(spans, &[])
        };
        let fields = direct("spans: [{service: db, name: db.query, meta: {sql.query: {not_contains: canary}}}]");

        let report = compare(&fields, &scrub(Some("SELECT ?")), &scrub(Some("SELECT ?"))).await;
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);

        // One scrubbed span doesn't excuse a duplicate that leaks, and the failure names the leaking span.
        let report = compare(&fields, &scrub(Some("SELECT 'canary'")), &scrub(None)).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(
            report
                .message
                .contains("The comparison holds no span that meets `spans[0]`")
                && report
                    .message
                    .contains("2 span(s) have that service, name and resource, and 1 fail `meta`")
                && report.message.contains("span 12 has `meta.sql.query`")
                && !report.message.contains("span 11 has"),
            "{}",
            report.message
        );
        assert!(!report.message.contains("The baseline holds"), "{}", report.message);
    }

    #[tokio::test]
    async fn a_known_span_difference_covers_a_span_on_one_side_only_by_name() {
        let entry = |kind: &str, resources: &str| {
            direct(&format!(
                "known_differences: [{{gap: gap/lost, issue: 1, paths: [spans], kinds: [{}], \
                 when: {{resource: [{}]}}, reason: a side drops the span}}]",
                kind, resources
            ))
        };
        let baseline = tracing(fixture_spans(false), &[]);
        let comparison = tracing(spans_without(11), &[]);

        // The comparison lacks the named span, as the entry says.
        let report = compare(&entry("missing", "SELECT 1"), &comparison, &baseline).await;
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);

        // Another span of one side only is not covered, and the failure names it.
        let comparison = tracing(
            spans_without(11)
                .into_iter()
                .filter(|span| span["span_id"] != 20)
                .collect(),
            &[],
        );
        let report = compare(&entry("missing", "SELECT 1"), &comparison, &baseline).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(
            report.message.contains(
                "span 20 (trace_id=2 service=\"api\" name=\"web.request\" resource=\"GET /x\"):\n    only in baseline"
            ),
            "{}",
            report.message
        );
        assert!(!report.message.contains("span 11 "), "{}", report.message);

        // The kind says which side holds the span: `missing` does not cover a span that only the comparison holds.
        let report = compare(
            &entry("missing", "SELECT 1"),
            &baseline,
            &tracing(spans_without(11), &[]),
        )
        .await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(report.message.contains("only in comparison"), "{}", report.message);
        assert!(report.message.contains("gap/lost (#1)"), "{}", report.message);
        let report = compare(&entry("extra", "SELECT 1"), &baseline, &tracing(spans_without(11), &[])).await;
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);
    }

    #[tokio::test]
    async fn a_known_span_difference_for_a_span_on_one_side_only_is_stale_once_both_sides_hold_it() {
        let fields = direct(
            "known_differences: [{gap: gap/lost, issue: 1, paths: [spans], kinds: [missing], \
             when: {resource: [SELECT 1, GET /x]}, reason: a side drops the span}]",
        );
        let baseline = tracing(fixture_spans(false), &[]);

        // Both sides hold both named spans, so each value of `when` is stale.
        let report = compare(&fields, &baseline, &baseline).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        for resource in ["SELECT 1", "GET /x"] {
            let expected = format!(
                "path `spans` when `resource` is \"{}\" (a side drops the span). Remove it from the case.",
                resource
            );
            assert!(report.message.contains(&expected), "{}", report.message);
        }

        // Once the comparison holds one of them again, only the other is stale.
        let comparison = tracing(spans_without(20), &[]);
        let report = compare(&fields, &comparison, &baseline).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(report.message.contains("is \"SELECT 1\""), "{}", report.message);
        assert!(!report.message.contains("is \"GET /x\""), "{}", report.message);
    }

    #[tokio::test]
    async fn span_expectations_of_a_spans_only_comparison_ignore_stats() {
        let direct = "{telemetry: traces, comparison: app, baseline: peer, otlp_direct_analysis_mode: true, \
            traces: {min_traces: 2, spans: [{service: api, name: web.request}]}}";
        let no_stats = tracing(fixture_spans(false), &[]);

        let report = compare(direct, &no_stats, &no_stats).await;
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);

        let report = compare(direct, &tracing(vec![], &[]), &no_stats).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(
            report.message.contains("The comparison holds 0 traces"),
            "{}",
            report.message
        );
    }

    #[test]
    fn trace_options_that_do_not_fit_the_step_are_rejected() {
        let cases = [
            (
                "{telemetry: traces, comparison: app, baseline: peer, traces: {min_spans: 1}}",
                "'traces' sets expectations or turns off 'require_ssi', so it must set 'min_stats_groups'",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, traces: {require_ssi: false}}",
                "'traces' sets expectations or turns off 'require_ssi', so it must set 'min_stats_groups'",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, otlp_direct_analysis_mode: true, \
                 traces: {min_stats_groups: 1}}",
                "'traces.min_stats_groups' has no effect with 'otlp_direct_analysis_mode'",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, otlp_direct_analysis_mode: true, \
                 traces: {require_ssi: false}}",
                "'traces.require_ssi' has no effect with 'otlp_direct_analysis_mode'",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, traces: {min_series: 1}}",
                "unknown field `min_series`",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, \
                 traces: {min_stats_groups: 1, spans: [{service: api}]}}",
                "a span of service 'api' needs a `name`, a `resource` or a `meta` tag",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, \
                 traces: {min_stats_groups: 1, spans: [{service: api, meta: {}}]}}",
                "a span of service 'api' needs a `name`, a `resource` or a `meta` tag",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, \
                 traces: {min_stats_groups: 1, spans: [{service: api, meta: {k: {}}}]}}",
                "a `meta` expectation needs `equals` or `not_contains`",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, \
                 traces: {min_stats_groups: 1, spans: [{service: api, meta: {k: {equal: x}}}]}}",
                "unknown field `equal`",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, \
                 traces: {min_stats_groups: 1, spans: [{service: api, name: x, side: both}]}}",
                "unknown variant `both`",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, \
                 traces: {min_stats_groups: 1, spans: [{name: web.request}]}}",
                "missing field `service`",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, traces: {absent: [{service: api, name: x}]}}",
                "'traces' sets expectations or turns off 'require_ssi', so it must set 'min_stats_groups'",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, otlp_direct_analysis_mode: true, \
                 traces: {spans: [{service: db, name: x}], absent: [{service: api, name: x}]}}",
                "`absent` names a span of service 'api', but `spans` holds no span of that service",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, \
                 traces: {min_stats_groups: 1, absent: [{service: api}]}}",
                "an `absent` span of service 'api' needs a `name` or a `resource`",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, \
                 traces: {min_stats_groups: 1, spans: [{service: api, name: x}], \
                 absent: [{service: api, name: x, meta: {k: {not_contains: c}}}]}}",
                "an `absent` span of service 'api' takes a `name` and a `resource`, but no `meta` and no `side`",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, \
                 traces: {min_stats_groups: 1, spans: [{service: api, name: x}], \
                 absent: [{service: api, name: x, side: baseline}]}}",
                "an `absent` span of service 'api' takes a `name` and a `resource`, but no `meta` and no `side`",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, \
                 traces: {min_stats_groups: 1, spans: [{service: api, name: x, side: baseline}], \
                 absent: [{service: api, name: gone}]}}",
                "`absent` names a span of service 'api', but `spans` holds no span of that service for both sides",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, traces: {min_traces: 0}}",
                "invalid value: integer `0`",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, traces: {max_spans: 0}}",
                "invalid value: integer `0`",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, traces: {max_traces: 4}}",
                "'traces' sets expectations or turns off 'require_ssi', so it must set 'min_stats_groups'",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, traces: {max_spans: 4}}",
                "'traces' sets expectations or turns off 'require_ssi', so it must set 'min_stats_groups'",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, \
                 traces: {min_stats_groups: 1, min_traces: 5, max_traces: 4}}",
                "'traces.max_traces' is 4, below 'traces.min_traces', which is 5",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, otlp_direct_analysis_mode: true, \
                 traces: {min_spans: 9, max_spans: 8}}",
                "'traces.max_spans' is 8, below 'traces.min_spans', which is 9",
            ),
            (
                "{telemetry: traces, comparison: app, baseline: peer, additional_span_ignore_fields: [start, trace_id]}",
                "'additional_span_ignore_fields' can't hold 'trace_id'",
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
