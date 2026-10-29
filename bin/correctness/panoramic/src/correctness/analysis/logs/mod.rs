//! Logs analysis.
//!
//! Logs are paired between the baseline and the comparison by an identity key made of a few stable fields (by default,
//! the message, the input timestamp, and the trace identifiers), rather than by all of their properties, so that a
//! single property regression still pairs the right logs and shows up as a field difference. Each pair is then diffed
//! field by field, keeping track of where each field was found: in the envelope, among other top-level keys, or in the
//! content parsed from the envelope `message`.

use std::{
    borrow::Borrow,
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use saluki_error::{generic_error, GenericError};
use serde::Deserialize;
use serde_json::Value;
use stele::Log;
use tracing::{error, info, warn};

pub use self::ratchet::KnownDifference;
use self::ratchet::Ratchet;
use crate::intake::CollectedData;

mod ratchet;

/// Envelope fields held as raw values, in the order they're compared.
const RAW_ENVELOPE_FIELDS: &[&str] = &["message", "status", "hostname", "service", "ddsource", "timestamp"];

/// Maximum number of differing pairs, missing logs, and extra logs to include in the error details for each category.
const SAMPLE_MISMATCH_LIMIT: usize = 20;

/// Maximum difference between the two sides' encode-time `envelope.timestamp` values for them to count as equal.
///
/// Each side sets this field when it encodes the payload, so it can't match exactly.
const ENCODE_TIMESTAMP_TOLERANCE_MS: u64 = 60_000;

/// Maximum number of candidate pairs to evaluate when pairing non-identical logs that share an identity key.
const MAX_NEAR_MATCH_CANDIDATES: usize = 10_000;

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
    /// Defaults to `message`, `otel.timestamp`, `otel.trace_id`, and `otel.span_id`.
    pub identity_fields: Vec<String>,

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
            known_differences: Vec::new(),
        }
    }
}

/// Where a property was found in a log.
///
/// Envelope fields are always at the top level, so they have no location of their own.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum FieldLocation {
    /// A top-level key other than the envelope fields.
    EnvelopeProperties,

    /// A property parsed from the envelope `message`.
    Content,
}

impl FieldLocation {
    /// Returns the path prefix used for fields in this location.
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::EnvelopeProperties => "envelope_properties",
            Self::Content => "content",
        }
    }
}

/// The kind of a difference between two paired logs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiffKind {
    /// Both sides have the field, with different values.
    Value,

    /// Only the baseline has the field.
    Missing,

    /// Only the comparison has the field.
    Extra,

    /// The field is in a different location on each side.
    Location {
        /// Where the baseline has the field.
        baseline: FieldLocation,

        /// Where the comparison has the field.
        comparison: FieldLocation,
    },
}

/// A single difference between two paired logs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldDiff {
    /// Location-qualified path of the field, such as `envelope.status` or `content.otel.trace_id`.
    ///
    /// For location differences, the prefix is the baseline's location.
    pub path: String,

    /// The kind of difference.
    pub kind: DiffKind,

    /// The baseline value, if present.
    pub baseline: Option<Value>,

    /// The comparison value, if present.
    pub comparison: Option<Value>,
}

impl fmt::Display for FieldDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let show = |value: &Option<Value>| value.as_ref().map_or_else(|| "<absent>".to_string(), Value::to_string);
        match &self.kind {
            DiffKind::Value => write!(f, "{}: value differs", self.path)?,
            DiffKind::Missing => write!(f, "{}: missing from comparison", self.path)?,
            DiffKind::Extra => write!(f, "{}: only in comparison", self.path)?,
            DiffKind::Location { baseline, comparison } => write!(
                f,
                "{}: in {} on baseline, {} on comparison",
                self.path,
                baseline.prefix(),
                comparison.prefix()
            )?,
        }
        write!(
            f,
            " (baseline: {}, comparison: {})",
            show(&self.baseline),
            show(&self.comparison)
        )
    }
}

/// A baseline log paired with a comparison log, along with their differences.
pub struct LogPair {
    /// Index of the baseline log.
    pub baseline: usize,

    /// Index of the comparison log.
    pub comparison: usize,

    /// Differences between the two logs.
    pub diffs: Vec<FieldDiff>,
}

/// The result of pairing and diffing the logs of both sides.
pub struct LogsComparison {
    /// Paired logs, ordered by baseline index.
    pub pairs: Vec<LogPair>,

    /// Indexes of baseline logs with no comparison counterpart.
    pub missing: Vec<usize>,

    /// Indexes of comparison logs with no baseline counterpart.
    pub extra: Vec<usize>,
}

/// Pairs the logs of both sides by identity key, and diffs each pair.
///
/// Within a group of logs sharing an identity key, identical logs are paired first. The remaining logs are paired
/// greedily by the smallest number of differences, with ties broken by index, so the result is deterministic.
pub fn compare_logs<L: Borrow<Log>>(baseline: &[L], comparison: &[L], identity_fields: &[String]) -> LogsComparison {
    let mut buckets = BTreeMap::<String, (Vec<usize>, Vec<usize>)>::new();
    for (idx, log) in baseline.iter().map(Borrow::borrow).enumerate() {
        buckets
            .entry(identity_key(log, identity_fields))
            .or_default()
            .0
            .push(idx);
    }
    for (idx, log) in comparison.iter().map(Borrow::borrow).enumerate() {
        buckets
            .entry(identity_key(log, identity_fields))
            .or_default()
            .1
            .push(idx);
    }

    let mut result = LogsComparison {
        pairs: Vec::new(),
        missing: Vec::new(),
        extra: Vec::new(),
    };
    for (baseline_idxs, comparison_idxs) in buckets.into_values() {
        pair_bucket(baseline, comparison, baseline_idxs, comparison_idxs, &mut result);
    }

    result.pairs.sort_by_key(|pair| pair.baseline);
    result.missing.sort_unstable();
    result.extra.sort_unstable();
    result
}

/// Returns a key that is equal for two logs exactly when `diff_logs` finds no difference between them.
///
/// Like `diff_logs`, it ignores the raw `message` string of a log whose content parsed, since that string is only the
/// serialization of the content, so whitespace and key order in it don't matter.
fn exact_match_key(log: &Log) -> String {
    let message = if log.content_properties().is_some() {
        None
    } else {
        log.message()
    };
    // Only present fields are keyed, so an absent field and an explicit `null` stay apart.
    let envelope = RAW_ENVELOPE_FIELDS
        .iter()
        .filter_map(|field| {
            let value = match *field {
                "message" => message,
                field => log.envelope_field(field),
            };
            value.map(|value| (*field, value))
        })
        .collect::<BTreeMap<_, _>>();
    serde_json::to_string(&(
        envelope,
        log.ddtags(),
        log.envelope_properties(),
        log.content_properties(),
    ))
    .unwrap_or_default()
}

fn identity_key(log: &Log, identity_fields: &[String]) -> String {
    let view = log.semantic_view();
    let key = identity_fields
        .iter()
        .map(|field| view.get(field).cloned().unwrap_or(Value::Null))
        .collect::<Vec<_>>();
    Value::Array(key).to_string()
}

fn pair_bucket<L: Borrow<Log>>(
    baseline: &[L], comparison: &[L], baseline_idxs: Vec<usize>, comparison_idxs: Vec<usize>,
    result: &mut LogsComparison,
) {
    // Pair identical logs first. This keeps large groups of duplicates cheap, since they never reach the quadratic
    // pairing below.
    let canonical = exact_match_key;
    let mut identical = BTreeMap::<String, Vec<usize>>::new();
    for &idx in comparison_idxs.iter().rev() {
        identical
            .entry(canonical(comparison[idx].borrow()))
            .or_default()
            .push(idx);
    }

    let mut remaining_baseline = Vec::new();
    for &idx in &baseline_idxs {
        match identical.get_mut(&canonical(baseline[idx].borrow())).and_then(Vec::pop) {
            Some(comparison_idx) => result.pairs.push(LogPair {
                baseline: idx,
                comparison: comparison_idx,
                diffs: Vec::new(),
            }),
            None => remaining_baseline.push(idx),
        }
    }
    let mut remaining_comparison = identical.into_values().flatten().collect::<Vec<_>>();
    remaining_comparison.sort_unstable();

    let mut selected = Vec::new();
    let candidate_count = remaining_baseline.len().saturating_mul(remaining_comparison.len());
    if candidate_count > MAX_NEAR_MATCH_CANDIDATES {
        // Pairing by fewest differences is quadratic in the size of the group, so very large groups are paired in
        // index order instead. Increasing the number of identity fields keeps groups small.
        warn!(
            baseline = remaining_baseline.len(),
            comparison = remaining_comparison.len(),
            "Too many non-identical logs share an identity key to pair them by fewest differences. Pairing them in \
             order instead."
        );
        selected.extend(
            remaining_baseline
                .iter()
                .copied()
                .zip(remaining_comparison.iter().copied()),
        );
    } else {
        // Pair greedily by the smallest number of differences. Only the counts are kept for each candidate, and the
        // diffs are recomputed for the selected pairs.
        let mut candidates = Vec::with_capacity(candidate_count);
        for &b in &remaining_baseline {
            for &c in &remaining_comparison {
                let count = diff_logs(baseline[b].borrow(), comparison[c].borrow()).len();
                candidates.push((count, b, c));
            }
        }
        candidates.sort_unstable();

        let mut taken_baseline = BTreeSet::new();
        let mut taken_comparison = BTreeSet::new();
        for (_, b, c) in candidates {
            if !taken_baseline.contains(&b) && !taken_comparison.contains(&c) {
                taken_baseline.insert(b);
                taken_comparison.insert(c);
                selected.push((b, c));
            }
        }
    }

    let paired_baseline = selected.iter().map(|(b, _)| *b).collect::<BTreeSet<_>>();
    let paired_comparison = selected.iter().map(|(_, c)| *c).collect::<BTreeSet<_>>();
    result.pairs.extend(selected.into_iter().map(|(b, c)| LogPair {
        baseline: b,
        comparison: c,
        diffs: diff_logs(baseline[b].borrow(), comparison[c].borrow()),
    }));

    result
        .missing
        .extend(remaining_baseline.into_iter().filter(|b| !paired_baseline.contains(b)));
    result.extra.extend(
        remaining_comparison
            .into_iter()
            .filter(|c| !paired_comparison.contains(c)),
    );
}

/// Diffs two logs field by field.
pub fn diff_logs(baseline: &Log, comparison: &Log) -> Vec<FieldDiff> {
    let mut diffs = Vec::new();

    // When both sides have content, the raw `message` string is just its serialization, so compare the content
    // properties instead to avoid reporting every content difference twice.
    let both_have_content = baseline.content_properties().is_some() && comparison.content_properties().is_some();
    for field in RAW_ENVELOPE_FIELDS {
        if *field == "message" && both_have_content {
            continue;
        }
        let baseline_value = baseline.envelope_field(field);
        let comparison_value = comparison.envelope_field(field);
        if *field == "timestamp" && encode_timestamps_match(baseline_value, comparison_value) {
            continue;
        }
        diff_optional(
            format!("envelope.{}", field),
            baseline_value,
            comparison_value,
            &mut diffs,
        );
    }

    for tag in baseline.ddtags().difference(comparison.ddtags()) {
        diffs.push(FieldDiff {
            path: "envelope.ddtags".to_string(),
            kind: DiffKind::Missing,
            baseline: Some(Value::String(tag.clone())),
            comparison: None,
        });
    }
    for tag in comparison.ddtags().difference(baseline.ddtags()) {
        diffs.push(FieldDiff {
            path: "envelope.ddtags".to_string(),
            kind: DiffKind::Extra,
            baseline: None,
            comparison: Some(Value::String(tag.clone())),
        });
    }

    diff_properties(baseline, comparison, &mut diffs);
    diffs
}

type PropertyLocations<'a> = [(FieldLocation, &'a BTreeMap<String, Value>); 2];

fn property_locations<'a>(log: &'a Log, empty: &'a BTreeMap<String, Value>) -> PropertyLocations<'a> {
    [
        (FieldLocation::EnvelopeProperties, log.envelope_properties()),
        (FieldLocation::Content, log.content_properties().unwrap_or(empty)),
    ]
}

fn find_property<'a>(locations: &PropertyLocations<'a>, key: &str) -> Vec<(FieldLocation, &'a Value)> {
    locations
        .iter()
        .filter_map(|(location, props)| props.get(key).map(|value| (*location, value)))
        .collect()
}

fn property_at<'a>(found: &[(FieldLocation, &'a Value)], location: FieldLocation) -> Option<&'a Value> {
    found
        .iter()
        .find(|(found_location, _)| *found_location == location)
        .map(|(_, value)| *value)
}

fn diff_properties(baseline: &Log, comparison: &Log, diffs: &mut Vec<FieldDiff>) {
    let empty = BTreeMap::new();
    let baseline_locations = property_locations(baseline, &empty);
    let comparison_locations = property_locations(comparison, &empty);

    let keys = baseline_locations
        .iter()
        .chain(comparison_locations.iter())
        .flat_map(|(_, props)| props.keys())
        .collect::<BTreeSet<_>>();

    for key in keys {
        let in_baseline = find_property(&baseline_locations, key);
        let in_comparison = find_property(&comparison_locations, key);

        // A key found in exactly one location on each side, but a different one, has moved.
        if let ([(baseline_location, baseline_value)], [(comparison_location, comparison_value)]) =
            (in_baseline.as_slice(), in_comparison.as_slice())
        {
            if baseline_location != comparison_location {
                let path = format!("{}.{}", baseline_location.prefix(), key);
                diffs.push(FieldDiff {
                    path: path.clone(),
                    kind: DiffKind::Location {
                        baseline: *baseline_location,
                        comparison: *comparison_location,
                    },
                    baseline: Some((*baseline_value).clone()),
                    comparison: Some((*comparison_value).clone()),
                });
                diff_values(path, baseline_value, comparison_value, diffs);
                continue;
            }
        }

        for location in [FieldLocation::EnvelopeProperties, FieldLocation::Content] {
            diff_optional(
                format!("{}.{}", location.prefix(), key),
                property_at(&in_baseline, location),
                property_at(&in_comparison, location),
                diffs,
            );
        }
    }
}

/// Returns `true` if both encode-time timestamps are epoch milliseconds within the tolerance of each other.
fn encode_timestamps_match(baseline: Option<&Value>, comparison: Option<&Value>) -> bool {
    match (baseline.and_then(Value::as_u64), comparison.and_then(Value::as_u64)) {
        (Some(baseline), Some(comparison)) => baseline.abs_diff(comparison) <= ENCODE_TIMESTAMP_TOLERANCE_MS,
        _ => false,
    }
}

fn diff_optional(path: String, baseline: Option<&Value>, comparison: Option<&Value>, diffs: &mut Vec<FieldDiff>) {
    match (baseline, comparison) {
        (None, None) => {}
        (Some(baseline), Some(comparison)) => diff_values(path, baseline, comparison, diffs),
        (Some(baseline), None) => diffs.push(FieldDiff {
            path,
            kind: DiffKind::Missing,
            baseline: Some(baseline.clone()),
            comparison: None,
        }),
        (None, Some(comparison)) => diffs.push(FieldDiff {
            path,
            kind: DiffKind::Extra,
            baseline: None,
            comparison: Some(comparison.clone()),
        }),
    }
}

/// Diffs two values, descending into objects and into arrays of equal length.
fn diff_values(path: String, baseline: &Value, comparison: &Value, diffs: &mut Vec<FieldDiff>) {
    match (baseline, comparison) {
        (Value::Object(baseline), Value::Object(comparison)) => {
            let keys = baseline.keys().chain(comparison.keys()).collect::<BTreeSet<_>>();
            for key in keys {
                diff_optional(
                    format!("{}.{}", path, key),
                    baseline.get(key),
                    comparison.get(key),
                    diffs,
                );
            }
        }
        (Value::Array(baseline), Value::Array(comparison)) if baseline.len() == comparison.len() => {
            for (idx, (baseline, comparison)) in baseline.iter().zip(comparison).enumerate() {
                diff_values(format!("{}[{}]", path, idx), baseline, comparison, diffs);
            }
        }
        _ if baseline != comparison => diffs.push(FieldDiff {
            path,
            kind: DiffKind::Value,
            baseline: Some(baseline.clone()),
            comparison: Some(comparison.clone()),
        }),
        _ => {}
    }
}

/// Analyzes logs for correctness.
pub struct LogsAnalyzer<'a> {
    baseline_logs: Vec<&'a Log>,
    comparison_logs: Vec<&'a Log>,
    options: LogsAnalysisOptions,
}

impl<'a> LogsAnalyzer<'a> {
    /// Creates a new `LogsAnalyzer` from the collected data of both sides.
    pub fn new(
        baseline_data: &'a CollectedData, comparison_data: &'a CollectedData, options: LogsAnalysisOptions,
    ) -> Self {
        Self {
            baseline_logs: baseline_data.logs().collect(),
            comparison_logs: comparison_data.logs().collect(),
            options,
        }
    }

    /// Runs the analysis.
    ///
    /// # Errors
    ///
    /// If either side has no logs or fewer than the minimum count, if any log can't be paired, or if any pair differs,
    /// an error is returned alongside the mismatch details.
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
        let mut known_by_gap = BTreeMap::<&str, usize>::new();
        let unknown = comparison
            .pairs
            .iter()
            .map(|pair| {
                let unknown = pair
                    .diffs
                    .iter()
                    .filter(|diff| match ratchet.classify(diff) {
                        Some(gap) => {
                            *known_by_gap.entry(gap).or_default() += 1;
                            false
                        }
                        None => true,
                    })
                    .collect::<Vec<_>>();
                (pair, unknown)
            })
            .filter(|(_, unknown)| !unknown.is_empty())
            .collect::<Vec<_>>();
        let stale = ratchet.stale_entries();

        for (gap, count) in &known_by_gap {
            info!("Ignored {} known differences for {}.", count, gap);
        }

        let details = self.describe(&unknown, &comparison, &stale);
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
                 comparison, {} comparison logs are not in baseline, and {} known differences are stale (baseline \
                 has {} logs, comparison has {}).",
                unknown.len(),
                comparison.missing.len(),
                comparison.extra.len(),
                stale.len(),
                self.baseline_logs.len(),
                self.comparison_logs.len()
            ),
            details,
        ))
    }

    fn describe(
        &self, unknown: &[(&LogPair, Vec<&FieldDiff>)], comparison: &LogsComparison, stale: &[String],
    ) -> Vec<String> {
        let mut details = Vec::new();

        for (pair, diffs) in unknown.iter().take(SAMPLE_MISMATCH_LIMIT) {
            let mut detail = format!(
                "Log mismatch (baseline #{}, comparison #{}, message {}):",
                pair.baseline,
                pair.comparison,
                describe_message(self.baseline_logs[pair.baseline])
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

        details.extend(stale.iter().cloned());
        details
    }
}

fn describe_message(log: &Log) -> String {
    log.display_message()
        .map_or_else(|| "<absent>".to_string(), Value::to_string)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

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

        let result = compare_logs(&[a.clone(), b.clone()], &[b, a], &defaults());

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
    fn a_property_regression_still_pairs_and_is_reported_as_a_field_diff() {
        let baseline = agent_log(json!({"message": "a", "otel.timestamp": "1", "attr": {"k": 1, "list": [1, 2]}}));
        let comparison = agent_log(json!({"message": "a", "otel.timestamp": "1", "attr": {"k": 2, "list": [1, 3]}}));

        let result = compare_logs(&[baseline], &[comparison], &defaults());

        assert_eq!(result.pairs.len(), 1);
        let diffs = &result.pairs[0].diffs;
        assert_eq!(paths(diffs), vec!["content.attr.k", "content.attr.list[1]"]);
        assert!(diffs.iter().all(|diff| diff.kind == DiffKind::Value));
        assert_eq!(diffs[0].baseline, Some(json!(1)));
        assert_eq!(diffs[0].comparison, Some(json!(2)));
    }

    #[test]
    fn envelope_fields_and_tags_are_diffed() {
        let baseline =
            log(json!({"message": "{\"message\":\"a\"}", "status": "info", "hostname": "h", "ddtags": "a:1,b:2"}));
        let comparison =
            log(json!({"message": "{\"message\":\"a\"}", "status": "Info", "service": "s", "ddtags": "b:2, c:3"}));

        let diffs = diff_logs(&baseline, &comparison);

        let summary = diffs
            .iter()
            .map(|diff| (diff.path.as_str(), diff.kind.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            vec![
                ("envelope.status", DiffKind::Value),
                ("envelope.hostname", DiffKind::Missing),
                ("envelope.service", DiffKind::Extra),
                ("envelope.ddtags", DiffKind::Missing),
                ("envelope.ddtags", DiffKind::Extra),
            ]
        );
        assert_eq!(diffs[3].baseline, Some(json!("a:1")));
        assert_eq!(diffs[4].comparison, Some(json!("c:3")));
    }

    #[test]
    fn a_property_that_moved_out_of_the_content_is_a_location_diff() {
        // The Agent keeps properties in the content, while ADP currently puts them at the top level.
        let baseline = log(json!({"message": "{\"message\":\"a\",\"otel.trace_id\":\"1\",\"attr\":\"x\"}"}));
        let comparison = log(json!({"message": "{\"message\":\"a\"}", "otel.trace_id": "1", "attr": "y"}));

        let diffs = diff_logs(&baseline, &comparison);

        assert_eq!(diffs.len(), 3, "{:?}", diffs);
        let moved = DiffKind::Location {
            baseline: FieldLocation::Content,
            comparison: FieldLocation::EnvelopeProperties,
        };
        assert_eq!((diffs[0].path.as_str(), &diffs[0].kind), ("content.attr", &moved));
        // A moved property whose value also changed gets a second, value diff.
        assert_eq!(
            (diffs[1].path.as_str(), &diffs[1].kind),
            ("content.attr", &DiffKind::Value)
        );
        assert_eq!(
            (diffs[2].path.as_str(), &diffs[2].kind),
            ("content.otel.trace_id", &moved)
        );
    }

    #[test]
    fn a_property_in_both_locations_on_one_side_is_diffed_per_location() {
        let baseline = log(json!({"message": "{\"k\":1}"}));
        let comparison = log(json!({"message": "{\"k\":1}", "k": 1}));

        let diffs = diff_logs(&baseline, &comparison);

        assert_eq!(paths(&diffs), vec!["envelope_properties.k"]);
        assert_eq!(diffs[0].kind, DiffKind::Extra);
    }

    #[test]
    fn raw_message_is_compared_unless_both_sides_have_content() {
        let content = log(json!({"message": "{\"message\":\"a\"}"}));
        let plain = log(json!({"message": "a"}));

        let diffs = diff_logs(&content, &plain);
        assert_eq!(paths(&diffs), vec!["envelope.message", "content.message"]);
        assert_eq!(diffs[0].kind, DiffKind::Value);
        assert_eq!(diffs[1].kind, DiffKind::Missing);

        assert!(diff_logs(&plain, &plain.clone()).is_empty());
    }

    #[test]
    fn unpaired_logs_are_reported_as_missing_or_extra() {
        let a = agent_log(json!({"message": "a"}));
        let b = agent_log(json!({"message": "b"}));
        let c = agent_log(json!({"message": "c"}));

        let result = compare_logs(&[a.clone(), b], &[a.clone(), c.clone()], &defaults());
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
        let a = agent_log(json!({"message": "a"}));

        let result = compare_logs(&[a.clone(), a.clone(), a.clone()], &[a.clone(), a], &defaults());

        assert_eq!(result.pairs.len(), 2);
        assert!(result.pairs.iter().all(|pair| pair.diffs.is_empty()));
        assert_eq!(result.missing, vec![2]);
        assert!(result.extra.is_empty());
    }

    #[test]
    fn competing_near_matches_pair_by_fewest_differences() {
        // All four logs share an identity key. Baseline #0 is closest to comparison #1 and baseline #1 to comparison
        // #0, so pairing by index would report more differences than necessary.
        let b0 = agent_log(json!({"message": "m", "x": 1, "y": 1}));
        let b1 = agent_log(json!({"message": "m", "x": 2, "y": 2}));
        let c0 = agent_log(json!({"message": "m", "x": 2, "y": 3}));
        let c1 = agent_log(json!({"message": "m", "x": 1, "y": 4}));

        let result = compare_logs(&[b0, b1], &[c0, c1], &defaults());

        let pairs = result
            .pairs
            .iter()
            .map(|pair| (pair.baseline, pair.comparison, paths(&pair.diffs)))
            .collect::<Vec<_>>();
        assert_eq!(pairs, vec![(0, 1, vec!["content.y"]), (1, 0, vec!["content.y"])]);
    }

    #[test]
    fn very_large_groups_of_near_matches_pair_in_order() {
        // 101 × 101 candidates is over the limit, so the logs are paired in index order without evaluating them.
        let side = |label: &str| {
            (0..101)
                .map(|i| agent_log(json!({"message": "m", "n": i, "side": label})))
                .collect::<Vec<_>>()
        };

        let result = compare_logs(&side("baseline"), &side("comparison"), &defaults());

        assert_eq!(result.pairs.len(), 101);
        assert!(result.missing.is_empty() && result.extra.is_empty());
        assert!(result.pairs.iter().all(|pair| pair.baseline == pair.comparison));
        assert_eq!(paths(&result.pairs[0].diffs), vec!["content.side"]);
    }

    #[test]
    fn reformatted_content_still_pairs_as_identical() {
        // More logs share an identity key than near-match pairing evaluates, so they must pair as identical, even
        // though one side serializes the same content with whitespace and in a different key order.
        let compact = (0..101)
            .map(|i| log(json!({"message": json!({"message": "m", "n": i}).to_string()})))
            .collect::<Vec<_>>();
        let reformatted = (0..101)
            .rev()
            .map(|i| log(json!({"message": format!("{{ \"n\": {}, \"message\": \"m\" }}", i)})))
            .collect::<Vec<_>>();

        let result = compare_logs(&compact, &reformatted, &defaults());

        assert_eq!(result.pairs.len(), 101);
        assert!(result.missing.is_empty() && result.extra.is_empty());
        assert!(result.pairs.iter().all(|pair| pair.diffs.is_empty()));
    }

    #[test]
    fn an_explicit_null_envelope_field_is_not_identical_to_an_absent_one() {
        let absent = log(json!({"message": "a"}));
        let null = log(json!({"message": "a", "status": null}));

        let result = compare_logs(&[absent], &[null], &defaults());

        assert_eq!(result.pairs.len(), 1);
        assert_eq!(paths(&result.pairs[0].diffs), vec!["envelope.status"]);
        assert_eq!(result.pairs[0].diffs[0].kind, DiffKind::Extra);
    }

    #[test]
    fn identity_fields_are_configurable() {
        let baseline = agent_log(json!({"message": "a", "otel.timestamp": "1"}));
        let comparison = agent_log(json!({"message": "a", "otel.timestamp": "2"}));

        let by_default = compare_logs(
            std::slice::from_ref(&baseline),
            std::slice::from_ref(&comparison),
            &defaults(),
        );
        assert_eq!((by_default.missing.len(), by_default.extra.len()), (1, 1));

        let by_message = compare_logs(&[baseline], &[comparison], &["message".to_string()]);
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
        assert!(details[0].contains("content.attr.k: value differs"), "{}", details[0]);
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

    #[test]
    fn encode_timestamps_match_within_the_tolerance() {
        let with_timestamp = |timestamp: Value| log(json!({"message": "a", "timestamp": timestamp}));
        let base = 1_793_268_000_000u64;

        let cases = [
            ("equal", json!(base), true),
            ("just inside", json!(base + ENCODE_TIMESTAMP_TOLERANCE_MS), true),
            ("just outside", json!(base + ENCODE_TIMESTAMP_TOLERANCE_MS + 1), false),
            (
                "earlier, just outside",
                json!(base - ENCODE_TIMESTAMP_TOLERANCE_MS - 1),
                false,
            ),
            ("not a number", json!(base.to_string()), false),
        ];
        for (name, comparison, matches) in cases {
            let diffs = diff_logs(&with_timestamp(json!(base)), &with_timestamp(comparison));
            assert_eq!(diffs.is_empty(), matches, "{}: {:?}", name, diffs);
        }

        // A missing timestamp is still a difference.
        let diffs = diff_logs(&with_timestamp(json!(base)), &log(json!({"message": "a"})));
        assert_eq!(paths(&diffs), vec!["envelope.timestamp"]);
        assert_eq!(diffs[0].kind, DiffKind::Missing);
    }

    #[test]
    fn a_large_reordered_input_matches() {
        let logs = (0..10_000)
            .map(|i| agent_log(json!({"message": format!("log {}", i % 1000), "otel.timestamp": i.to_string()})))
            .collect::<Vec<_>>();
        let mut reversed = logs.clone();
        reversed.reverse();

        let result = compare_logs(&logs, &reversed, &defaults());

        assert_eq!(result.pairs.len(), 10_000);
        assert!(result.missing.is_empty() && result.extra.is_empty());
        assert!(result
            .pairs
            .iter()
            .all(|pair| pair.diffs.is_empty() && pair.comparison == 9_999 - pair.baseline));
    }
}
