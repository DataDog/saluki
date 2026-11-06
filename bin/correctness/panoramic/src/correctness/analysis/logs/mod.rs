//! Logs analysis.
//!
//! Logs are paired between the baseline and the comparison by an identity key made of a few stable fields (by default,
//! the message, the input timestamp, and the trace identifiers), rather than by all of their properties, so that a
//! single property regression still pairs the right logs and shows up as a field difference. Each pair is then diffed
//! field by field, keeping track of where each field was found: in the envelope, among other top-level keys, or in the
//! content parsed from the envelope `message`. Differences covered by the case's known differences are allowed.
//!
//! The headers of the requests that carry the logs are compared separately, per side rather than per log, since the two
//! sides batch logs into requests differently. See the `headers` module.

use std::{
    collections::BTreeMap,
    ops::RangeInclusive,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use saluki_error::{generic_error, GenericError};
use serde::Deserialize;
use serde_json::Value;
use stele::{diff_logs, FieldDiff, Log, LogsRequest};
use tracing::{error, info};

pub use self::headers::HeaderRule;
use self::headers::{compare_headers, default_header_rules};
pub use self::ratchet::KnownDifference;
use self::ratchet::Ratchet;
use crate::intake::CollectedData;

mod headers;
mod ratchet;

/// Maximum number of differing pairs, missing logs, and extra logs to include in the error details for each category.
const SAMPLE_MISMATCH_LIMIT: usize = 20;

/// Maximum difference between the two sides' encode-time `envelope.timestamp` values for them to count as equal.
///
/// Each side sets this field when it encodes the payload, so it can't match exactly.
const ENCODE_TIMESTAMP_TOLERANCE_MS: u64 = 60_000;

/// How long before the analysis a timestamp header may be and still count as plausible.
///
/// Logs analysis runs right after the run that produced the logs, and runs take minutes, so this rejects zero values and
/// values in the wrong unit while leaving ample room for slow runs.
const TIMESTAMP_HEADER_MAX_AGE: Duration = Duration::from_secs(3_600);

/// How far after the analysis a timestamp header may be and still count as plausible, to allow for clock skew.
const TIMESTAMP_HEADER_MAX_SKEW: Duration = Duration::from_secs(60);

fn default_identity_fields() -> Vec<String> {
    ["message", "otel.timestamp", "otel.trace_id", "otel.span_id"]
        .into_iter()
        .map(String::from)
        .collect()
}

/// Options for logs analysis, set in the `logs` section of a correctness case.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogsAnalysisOptions {
    /// Minimum number of logs each side must produce.
    ///
    /// Both sides must always produce at least one log.
    pub min_count: Option<usize>,

    /// Keys of the semantic view (see `stele::Log::semantic_view`) used to pair logs between the two sides.
    ///
    /// Defaults to `message`, `otel.timestamp`, `otel.trace_id`, and `otel.span_id`. Logs that share an identity key
    /// are paired in content order, which only works reliably for duplicates, so the key should be close to unique.
    pub identity_fields: Vec<String>,

    /// Headers of the logs requests to compare between the two sides. Other headers are ignored.
    ///
    /// Defaults to `DD-PROTOCOL`, `DD-EVP-ORIGIN`, and `Content-Encoding`, compared exactly; `DD-EVP-ORIGIN-VERSION`,
    /// compared by presence; and `dd-message-timestamp` and `dd-current-timestamp`, compared as timestamps. Setting
    /// this replaces the defaults rather than extending them, and an empty list compares no headers at all.
    /// Differences are reported at `headers.<name>`, with the name lowercased, and can be covered by known
    /// differences. A header difference belongs to no log, so an entry scoped with `when` never covers one, and its
    /// header paths are reported as stale.
    pub headers: Vec<HeaderRule>,

    /// Differences that are expected, each tagged with the gap that explains it.
    ///
    /// A matching difference doesn't fail the case, but an entry path that matches nothing does, so that fixing a
    /// gap requires removing its entries.
    pub known_differences: Vec<KnownDifference>,
}

impl Default for LogsAnalysisOptions {
    fn default() -> Self {
        Self {
            min_count: None,
            identity_fields: default_identity_fields(),
            headers: default_header_rules(),
            known_differences: Vec::new(),
        }
    }
}

/// A baseline log paired with a comparison log, along with their differences.
struct LogPair {
    baseline: usize,
    comparison: usize,
    diffs: Vec<FieldDiff>,
}

/// The result of pairing and diffing the logs of both sides.
#[derive(Default)]
struct LogsComparison {
    /// Paired logs, ordered by baseline index.
    pairs: Vec<LogPair>,

    /// Indexes of baseline logs with no comparison counterpart.
    missing: Vec<usize>,

    /// Indexes of comparison logs with no baseline counterpart.
    extra: Vec<usize>,
}

/// Pairs the logs of both sides by identity key, and diffs each pair.
///
/// Logs that share an identity key are sorted by content on each side and paired in that order. This pairs duplicates
/// correctly. Any other group of logs sharing a key may pair wrongly, but that only adds differences, so it fails the
/// case rather than hiding one.
fn compare_logs(baseline: &[&Log], comparison: &[&Log], identity_fields: &[String]) -> LogsComparison {
    let mut groups = BTreeMap::<String, (Vec<usize>, Vec<usize>)>::new();
    for (idx, log) in baseline.iter().enumerate() {
        groups
            .entry(identity_key(log, identity_fields))
            .or_default()
            .0
            .push(idx);
    }
    for (idx, log) in comparison.iter().enumerate() {
        groups
            .entry(identity_key(log, identity_fields))
            .or_default()
            .1
            .push(idx);
    }

    let mut result = LogsComparison::default();
    for (mut baseline_idxs, mut comparison_idxs) in groups.into_values() {
        baseline_idxs.sort_by_cached_key(|&idx| (content_key(baseline[idx]), idx));
        comparison_idxs.sort_by_cached_key(|&idx| (content_key(comparison[idx]), idx));

        for (&b, &c) in baseline_idxs.iter().zip(&comparison_idxs) {
            result.pairs.push(LogPair {
                baseline: b,
                comparison: c,
                diffs: diff_logs(baseline[b], comparison[c], ENCODE_TIMESTAMP_TOLERANCE_MS),
            });
        }
        result.missing.extend(baseline_idxs.iter().skip(comparison_idxs.len()));
        result.extra.extend(comparison_idxs.iter().skip(baseline_idxs.len()));
    }

    result.pairs.sort_by_key(|pair| pair.baseline);
    result.missing.sort_unstable();
    result.extra.sort_unstable();
    result
}

fn identity_key(log: &Log, identity_fields: &[String]) -> String {
    let view = log.semantic_view();
    let key = identity_fields
        .iter()
        .map(|field| view.get(field).cloned().unwrap_or(Value::Null))
        .collect::<Vec<_>>();
    Value::Array(key).to_string()
}

fn content_key(log: &Log) -> String {
    serde_json::to_string(log).unwrap_or_default()
}

/// Analyzes logs for correctness.
pub struct LogsAnalyzer<'a> {
    baseline_requests: &'a [LogsRequest],
    comparison_requests: &'a [LogsRequest],
    baseline_logs: Vec<&'a Log>,
    comparison_logs: Vec<&'a Log>,
    options: LogsAnalysisOptions,
    timestamp_header_range: RangeInclusive<u64>,
}

impl<'a> LogsAnalyzer<'a> {
    /// Creates a new `LogsAnalyzer` from the collected data of both sides.
    pub fn new(
        baseline_data: &'a CollectedData, comparison_data: &'a CollectedData, options: LogsAnalysisOptions,
    ) -> Self {
        Self {
            baseline_requests: baseline_data.log_requests(),
            comparison_requests: comparison_data.log_requests(),
            baseline_logs: baseline_data.logs().collect(),
            comparison_logs: comparison_data.logs().collect(),
            options,
            timestamp_header_range: timestamp_header_range(SystemTime::now()),
        }
    }

    /// Runs the analysis.
    ///
    /// # Errors
    ///
    /// If either side has no logs or fewer than the minimum count, if any log can't be paired, if any pair or the
    /// compared request headers have a difference that isn't known, or if any known difference is stale, an error is
    /// returned alongside the mismatch details.
    pub fn run_analysis(self) -> Result<(), (GenericError, Vec<String>)> {
        info!(
            "Analyzing {} logs from baseline target, and {} logs from comparison target.",
            self.baseline_logs.len(),
            self.comparison_logs.len()
        );

        let min_count = self.options.min_count.unwrap_or(1).max(1);
        for (side, logs) in [("baseline", &self.baseline_logs), ("comparison", &self.comparison_logs)] {
            if logs.len() < min_count {
                let msg = format!(
                    "The {} target produced {} logs, but at least {} are required.",
                    side,
                    logs.len(),
                    min_count
                );
                error!("{}", msg);
                return Err((generic_error!("{}", msg), vec![msg]));
            }
        }

        let comparison = compare_logs(
            &self.baseline_logs,
            &self.comparison_logs,
            &self.options.identity_fields,
        );

        let mut ratchet = Ratchet::new(&self.options.known_differences);
        let mut unknown = Vec::new();
        for pair in &comparison.pairs {
            let view = self.baseline_logs[pair.baseline].semantic_view();
            let diffs = pair
                .diffs
                .iter()
                .filter(|diff| !ratchet.allows(diff, &view))
                .collect::<Vec<_>>();
            if !diffs.is_empty() {
                unknown.push((pair, diffs));
            }
        }

        // Header differences belong to no log, so entries scoped to logs with `when` never cover them.
        let header_diffs = compare_headers(
            self.baseline_requests,
            self.comparison_requests,
            &self.options.headers,
            &self.timestamp_header_range,
        );
        let no_view = BTreeMap::new();
        let unknown_headers = header_diffs
            .iter()
            .filter(|diff| !ratchet.allows(diff, &no_view))
            .collect::<Vec<_>>();
        let stale = ratchet.stale_entries();

        let details = self.describe(&unknown, &unknown_headers, &comparison, &stale);
        if details.is_empty() {
            info!(
                "All {} logs matched between baseline and comparison, apart from known differences.",
                comparison.pairs.len()
            );
            return Ok(());
        }

        for detail in &details {
            error!("{}", detail);
        }
        Err((
            generic_error!(
                "Logs did not match: {} paired logs have unexpected differences, {} baseline logs are missing from \
                 comparison, {} comparison logs are not in baseline, {} request headers have unexpected differences, \
                 and {} known differences are stale (baseline has {} logs, comparison has {}).",
                unknown.len(),
                comparison.missing.len(),
                comparison.extra.len(),
                unknown_headers.len(),
                stale.len(),
                self.baseline_logs.len(),
                self.comparison_logs.len()
            ),
            details,
        ))
    }

    fn describe(
        &self, unknown: &[(&LogPair, Vec<&FieldDiff>)], unknown_headers: &[&FieldDiff], comparison: &LogsComparison,
        stale: &[String],
    ) -> Vec<String> {
        let mut details = Vec::new();

        for (pair, diffs) in unknown.iter().take(SAMPLE_MISMATCH_LIMIT) {
            let mut detail = format!(
                "Log mismatch (baseline #{}, comparison #{}, identity {}):",
                pair.baseline,
                pair.comparison,
                identity_key(self.baseline_logs[pair.baseline], &self.options.identity_fields)
            );
            for diff in diffs {
                detail.push_str("\n    ");
                detail.push_str(&diff.to_string());
            }
            details.push(detail);
        }

        for (label, idxs, logs) in [
            ("missing from comparison", &comparison.missing, &self.baseline_logs),
            ("only in comparison", &comparison.extra, &self.comparison_logs),
        ] {
            for &idx in idxs.iter().take(SAMPLE_MISMATCH_LIMIT) {
                details.push(format!(
                    "Log {} (#{}): {}",
                    label,
                    idx,
                    serde_json::to_string(&logs[idx].semantic_view()).unwrap_or_default()
                ));
            }
        }

        details.extend(
            unknown_headers
                .iter()
                .map(|diff| format!("Logs request header mismatch: {}", diff)),
        );
        details.extend(stale.iter().cloned());
        details
    }
}

/// Returns the plausible range of timestamp headers, in epoch milliseconds, for an analysis run at `now`.
fn timestamp_header_range(now: SystemTime) -> RangeInclusive<u64> {
    let millis = |time: SystemTime| {
        time.duration_since(UNIX_EPOCH)
            .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
    };
    millis(now - TIMESTAMP_HEADER_MAX_AGE)..=millis(now + TIMESTAMP_HEADER_MAX_SKEW)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use stele::DiffKind;

    use super::*;

    fn log(value: Value) -> Log {
        let payload = Value::Array(vec![value]).to_string();
        Log::try_from_payload(payload.as_bytes())
            .expect("test log should parse")
            .remove(0)
    }

    /// Builds an Agent-shaped log: every property lives in the content.
    fn agent_log(content: Value) -> Log {
        log(json!({"message": content.to_string(), "status": "info", "ddtags": "otel_source:datadog_agent"}))
    }

    fn defaults() -> Vec<String> {
        default_identity_fields()
    }

    fn paths(diffs: &[FieldDiff]) -> Vec<&str> {
        diffs.iter().map(|diff| diff.path.as_str()).collect()
    }

    fn data(logs: Vec<Log>) -> CollectedData {
        CollectedData::with_logs(vec![stele::LogsRequest::new("/api/v2/logs", [], logs)])
    }

    #[test]
    fn identical_logs_in_any_order_match() {
        let a = agent_log(json!({"message": "a", "otel.timestamp": "1"}));
        let b = agent_log(json!({"message": "b", "otel.timestamp": "2"}));

        let result = compare_logs(&[&a, &b], &[&b, &a], &defaults());

        assert!(result.missing.is_empty() && result.extra.is_empty());
        assert_eq!(result.pairs.len(), 2);
        assert_eq!((result.pairs[0].baseline, result.pairs[0].comparison), (0, 1));
        assert_eq!((result.pairs[1].baseline, result.pairs[1].comparison), (1, 0));
        assert!(result.pairs.iter().all(|pair| pair.diffs.is_empty()));

        let baseline = data(vec![agent_log(json!({"message": "a"}))]);
        let comparison = data(vec![agent_log(json!({"message": "a"}))]);
        let analysis = LogsAnalyzer::new(&baseline, &comparison, LogsAnalysisOptions::default());
        assert!(analysis.run_analysis().is_ok());
    }

    #[test]
    fn encode_timestamps_are_compared_with_the_encode_tolerance() {
        let base = 1_793_268_000_000u64;
        let at = |timestamp: u64| log(json!({"message": "{}", "status": "info", "timestamp": timestamp}));

        let result = compare_logs(&[&at(base)], &[&at(base + 30_000)], &defaults());
        assert_eq!(result.pairs.len(), 1);
        assert!(result.pairs[0].diffs.is_empty(), "{:?}", result.pairs[0].diffs);

        let result = compare_logs(
            &[&at(base)],
            &[&at(base + ENCODE_TIMESTAMP_TOLERANCE_MS + 1)],
            &defaults(),
        );
        assert_eq!(result.pairs.len(), 1);
        assert_eq!(paths(&result.pairs[0].diffs), vec!["envelope.timestamp"]);
    }

    #[test]
    fn a_property_regression_still_pairs_and_is_reported_as_a_field_diff() {
        let baseline = agent_log(json!({"message": "a", "otel.timestamp": "1", "attr": {"k": 1, "list": [1, 2]}}));
        let comparison = agent_log(json!({"message": "a", "otel.timestamp": "1", "attr": {"k": 2, "list": [1, 3]}}));

        let result = compare_logs(&[&baseline], &[&comparison], &defaults());

        assert_eq!(result.pairs.len(), 1);
        let diffs = &result.pairs[0].diffs;
        assert_eq!(paths(diffs), vec!["content.attr.k", "content.attr.list"]);
        assert!(diffs.iter().all(|diff| diff.kind == DiffKind::Value));
        assert_eq!(diffs[0].baseline, Some(json!(1)));
        assert_eq!(diffs[0].comparison, Some(json!(2)));
    }

    #[test]
    fn unpaired_logs_are_reported_as_missing_or_extra() {
        let a = agent_log(json!({"message": "a"}));
        let b = agent_log(json!({"message": "b"}));
        let c = agent_log(json!({"message": "c"}));

        let result = compare_logs(&[&a, &b], &[&a, &c], &defaults());
        assert_eq!(result.pairs.len(), 1);
        assert_eq!(result.missing, vec![1]);
        assert_eq!(result.extra, vec![1]);

        let (error, details) = LogsAnalyzer::new(
            &data(vec![a.clone()]),
            &data(vec![a, c]),
            LogsAnalysisOptions::default(),
        )
        .run_analysis()
        .expect_err("an extra log should fail the analysis");
        assert!(
            error.to_string().contains("1 comparison logs are not in baseline"),
            "{}",
            error
        );
        assert_eq!(details.len(), 1);
        assert!(details[0].starts_with("Log only in comparison (#1)"), "{}", details[0]);
    }

    #[test]
    fn duplicate_logs_pair_one_to_one() {
        // Each side's duplicates differ from the other side's, as they do when a known difference applies to all of
        // them, so every pair has the same single difference.
        let b = agent_log(json!({"message": "a", "k": 1}));
        let c = agent_log(json!({"message": "a", "k": 2}));

        let result = compare_logs(&[&b, &b, &b], &[&c, &c], &defaults());

        assert_eq!(result.pairs.len(), 2);
        assert!(result.pairs.iter().all(|pair| paths(&pair.diffs) == vec!["content.k"]));
        assert_eq!(result.missing.len(), 1);
        assert!(result.extra.is_empty());
    }

    #[test]
    fn configured_headers_replace_the_defaults() {
        let options: LogsAnalysisOptions =
            serde_saphyr::from_str("headers: [{name: X-Custom, check: exact}]").expect("options should parse");
        assert_eq!(
            options.headers,
            vec![HeaderRule {
                name: "X-Custom".to_string(),
                check: headers::HeaderCheck::Exact,
            }]
        );

        let options: LogsAnalysisOptions = serde_saphyr::from_str("headers: []").expect("options should parse");
        assert!(options.headers.is_empty());

        let options: LogsAnalysisOptions = serde_saphyr::from_str("min_count: 1").expect("options should parse");
        assert_eq!(options.headers, default_header_rules());

        assert!(serde_saphyr::from_str::<LogsAnalysisOptions>("header: []").is_err());
    }

    #[test]
    fn identity_fields_are_configurable() {
        let baseline = agent_log(json!({"message": "a", "otel.timestamp": "1"}));
        let comparison = agent_log(json!({"message": "a", "otel.timestamp": "2"}));

        let by_default = compare_logs(&[&baseline], &[&comparison], &defaults());
        assert_eq!((by_default.missing.len(), by_default.extra.len()), (1, 1));

        let by_message = compare_logs(&[&baseline], &[&comparison], &["message".to_string()]);
        assert_eq!(paths(&by_message.pairs[0].diffs), vec!["content.otel.timestamp"]);
    }

    #[test]
    fn empty_sides_and_low_counts_fail() {
        let a = agent_log(json!({"message": "a"}));
        let cases = [
            ("both empty", vec![], vec![], None, "baseline target produced 0 logs"),
            (
                "baseline empty",
                vec![],
                vec![a.clone()],
                None,
                "baseline target produced 0 logs",
            ),
            (
                "comparison empty",
                vec![a.clone()],
                vec![],
                None,
                "comparison target produced 0 logs",
            ),
            (
                "below minimum",
                vec![a.clone(), a.clone()],
                vec![a.clone(), a.clone()],
                Some(3),
                "produced 2 logs, but at least 3",
            ),
        ];

        for (name, baseline, comparison, min_count, expected) in cases {
            let options = LogsAnalysisOptions {
                min_count,
                ..LogsAnalysisOptions::default()
            };
            let (error, _) = LogsAnalyzer::new(&data(baseline), &data(comparison), options)
                .run_analysis()
                .expect_err(name);
            assert!(error.to_string().contains(expected), "{}: {}", name, error);
        }
    }

    fn known(gap: &str, paths: &[&str]) -> KnownDifference {
        serde_json::from_value(json!({"gap": gap, "paths": paths, "reason": "test"})).expect("entry should parse")
    }

    #[test]
    fn known_differences_pass_and_unknown_ones_fail() {
        let baseline = data(vec![log(
            json!({"message": "{\"message\":\"a\",\"attr\":{\"k\":1}}", "status": "info"}),
        )]);
        let comparison = data(vec![log(
            json!({"message": "{\"message\":\"a\",\"attr\":{\"k\":2}}", "status": "Info"}),
        )]);
        let options = |known_differences| LogsAnalysisOptions {
            known_differences,
            ..LogsAnalysisOptions::default()
        };

        let all_known = vec![
            known("gap/status-strings", &["envelope.status"]),
            known("gap/as-string-coercion", &["content.attr.*"]),
        ];
        assert!(LogsAnalyzer::new(&baseline, &comparison, options(all_known))
            .run_analysis()
            .is_ok());

        // An entry for a parent field doesn't cover its nested fields.
        let too_narrow = vec![
            known("gap/status-strings", &["envelope.status"]),
            known("gap/as-string-coercion", &["content.attr"]),
        ];
        let (error, details) = LogsAnalyzer::new(&baseline, &comparison, options(too_narrow))
            .run_analysis()
            .expect_err("an unexpected nested difference should fail");
        assert!(
            error.to_string().contains("1 paired logs have unexpected differences"),
            "{}",
            error
        );
        assert!(details[0].contains("content.attr.k: Value"), "{}", details[0]);
        assert!(!details[0].contains("envelope.status"), "{}", details[0]);
    }

    #[test]
    fn stale_known_differences_fail() {
        let a = data(vec![agent_log(json!({"message": "a"}))]);
        let options = LogsAnalysisOptions {
            known_differences: vec![known("gap/event-name", &["content.otel.event_name"])],
            ..LogsAnalysisOptions::default()
        };

        let (error, details) = LogsAnalyzer::new(&a, &a, options)
            .run_analysis()
            .expect_err("a stale entry should fail");
        assert!(error.to_string().contains("1 known differences are stale"), "{}", error);
        assert_eq!(details.len(), 1);
        assert!(details[0].contains("gap/event-name"), "{}", details[0]);
    }

    fn data_with_headers(headers: &[(&str, &str)], logs: Vec<Log>) -> CollectedData {
        CollectedData::with_logs(vec![stele::LogsRequest::new(
            "/api/v2/logs",
            headers.iter().copied(),
            logs,
        )])
    }

    #[test]
    fn request_header_differences_go_through_the_ratchet() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after the epoch")
            .as_millis()
            .to_string();
        let a = agent_log(json!({"message": "a"}));
        let baseline = data_with_headers(
            &[
                ("DD-PROTOCOL", "agent-json"),
                ("dd-current-timestamp", &now),
                ("Content-Encoding", "zstd"),
                ("User-Agent", "datadog-agent/7.70.0"),
            ],
            vec![a.clone()],
        );
        let comparison = data_with_headers(
            &[("content-encoding", "gzip"), ("user-agent", "agent-data-plane/0.1.0")],
            vec![a],
        );
        let options = |known_differences| LogsAnalysisOptions {
            known_differences,
            ..LogsAnalysisOptions::default()
        };

        // Unexpected header differences fail the case, even when every log matches.
        let (error, details) = LogsAnalyzer::new(&baseline, &comparison, options(vec![]))
            .run_analysis()
            .expect_err("missing headers should fail");
        assert!(
            error
                .to_string()
                .contains("3 request headers have unexpected differences"),
            "{}",
            error
        );
        assert_eq!(
            details,
            vec![
                r#"Logs request header mismatch: headers.dd-protocol: Missing (baseline: ["agent-json"], comparison: <absent>)"#,
                r#"Logs request header mismatch: headers.dd-current-timestamp: Missing (baseline: ["<timestamp>"], comparison: <absent>)"#,
                r#"Logs request header mismatch: headers.content-encoding: Value (baseline: ["zstd"], comparison: ["gzip"])"#,
            ]
        );

        let entry = |gap: &str, paths: &[&str], kinds: &[&str]| {
            serde_json::from_value::<KnownDifference>(
                json!({"gap": gap, "paths": paths, "kinds": kinds, "reason": "test"}),
            )
            .expect("entry should parse")
        };
        let missing = entry(
            "gap/missing-headers",
            &["headers.dd-protocol", "headers.dd-current-timestamp"],
            &["missing"],
        );
        let encoding = entry("gap/compression", &["headers.content-encoding"], &["value"]);
        assert!(
            LogsAnalyzer::new(&baseline, &comparison, options(vec![missing.clone(), encoding.clone()]))
                .run_analysis()
                .is_ok()
        );

        // An entry that matches no header difference is stale.
        let unused = entry("gap/missing-headers", &["headers.dd-evp-origin"], &["missing"]);
        let (error, details) = LogsAnalyzer::new(&baseline, &comparison, options(vec![missing, encoding, unused]))
            .run_analysis()
            .expect_err("an unused header entry should be stale");
        assert!(error.to_string().contains("1 known differences are stale"), "{}", error);
        assert!(details[0].contains("`headers.dd-evp-origin`"), "{}", details[0]);

        // Entries scoped to logs never cover header differences.
        let mut scoped = entry("gap/compression", &["headers.content-encoding"], &[]);
        scoped.when = BTreeMap::from([("message".to_string(), vec![json!("a")])]);
        let (error, _) = LogsAnalyzer::new(&baseline, &comparison, options(vec![scoped]))
            .run_analysis()
            .expect_err("a scoped entry shouldn't cover a header difference");
        assert!(
            error
                .to_string()
                .contains("3 request headers have unexpected differences"),
            "{}",
            error
        );
    }

    #[test]
    fn timestamp_headers_are_plausible_within_the_run() {
        let now = UNIX_EPOCH + Duration::from_millis(1_793_268_000_000);
        let range = timestamp_header_range(now);

        assert_eq!(*range.start(), 1_793_268_000_000 - 3_600_000);
        assert_eq!(*range.end(), 1_793_268_000_000 + 60_000);
    }
}
