use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt,
    num::NonZeroUsize,
    time::Duration,
};

use saluki_common::collections::FastHashMap;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Deserialize;
use serde_json::{Number, Value};
use stele::{AggregationKey, BucketedClientStatistics, Span};
use tracing::{error, info};
use treediff::value::Key;

use super::expectations::{ExpectedSpan, HeldTraces, TracesExpectations};
use super::join_failures;
use super::ratchet::{path_matches, DiffKind, FieldDiff, KnownDifference, Ratchet};
use crate::steps::analysis::TracesAnalysisOptions;
use crate::steps::intake::CollectedData;

/// Built-in span field paths always ignored when diffing. Configurable additions via `TracesAnalysisOptions::additional_span_ignore_fields`.
static BASE_IGNORED_FIELDS_DIFF: &[&str] = &[
    // Single Step Instrumentation-related metadata, which is inherently non-deterministic and not suitable for direct
    // comparison.
    "meta._dd.install.id",
    "meta._dd.install.time",
    "meta._dd.install.type",
    "agent_metadata.agent_version",
    // Deprecated
    "metrics._sampling_priority_rate_v1",
];

static CUSTOM_FIELD_COMPARATORS: &[(&str, &dyn FieldComparator)] = &[("start", &check_start_diff)];

const SAMPLE_MISMATCH_LIMIT: usize = 5;
const SAMPLE_DIFFS_PER_SPAN: usize = 3;

/// The path of a difference in the set of statistics groups. The other statistics paths are `stats.<field>`.
const STATS_GROUPS_PATH: &str = "stats.groups";

/// The fields of a statistics group that name it in a failure, in the order they're shown. A field at its default value
/// is left out.
const STATS_GROUP_NAME_FIELDS: &[&str] = &[
    "service",
    "name",
    "resource",
    "type_",
    "span_kind",
    "http_method",
    "http_endpoint",
    "http_status_code",
    "grpc_status_code",
    "db_type",
    "is_trace_root",
    "synthetics",
    "peer_tags",
];

trait FieldComparator: Sync {
    fn compare(&self, baseline: &Value, comparison: &Value) -> Result<(), String>;
}

impl<F> FieldComparator for F
where
    F: Fn(&Value, &Value) -> Result<(), String> + Sync,
{
    fn compare(&self, baseline: &Value, comparison: &Value) -> Result<(), String> {
        self(baseline, comparison)
    }
}

/// The prefix of the paths of a traces known difference that cover the trace statistics.
const STATS_PREFIX: &str = "stats.";

/// The prefix of the paths of a traces known difference that cover the fields of a span on both sides.
const SPAN_PREFIX: &str = "span.";

/// The path of a traces known difference that covers a span on one side only.
const SPANS_PATH: &str = "spans";

/// The span field that a split or merged trace differs in, on the spans that both sides hold. Neither a known difference
/// nor `additional_span_ignore_fields` may cover it.
pub const TRACE_ID_FIELD: &str = "trace_id";

/// The fields of a span that the view of a span holds, and that name it in a failure.
const SPAN_VIEW_FIELDS: &[&str] = &["service", "name", "resource"];

/// Options for traces analysis, set in the `traces` section of a `compare` step.
///
/// The expectations (`min_traces`, `min_spans`, `spans`, `absent` and `min_stats_groups`) say what each side must hold.
/// The step checks them first, and still compares the sides when one is unmet. A known difference never excuses an
/// unmet expectation.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TracesOptions {
    /// Trace statistics and span field differences that are expected, each tagged with the gap and the issue that
    /// explain it.
    ///
    /// A matching difference doesn't fail the case, but an entry path that matches nothing for one of the values of
    /// `when` does, so that fixing a gap requires removing its entries.
    #[serde(default)]
    pub known_differences: Vec<KnownDifference>,

    /// Whether each side must hold a span with Single Step Instrumentation metadata.
    ///
    /// The Agent or ADP adds that metadata to the spans that it sends, so the check shows that onboarding ran. Set it
    /// to `false` to compare the stats of a side whose output has no such metadata, and set `min_stats_groups` too:
    /// without the check, two sides with no spans match. Defaults to `true`. It has no effect with
    /// `otlp_direct_analysis_mode`, so the step rejects it there.
    #[serde(default)]
    pub require_ssi: Option<bool>,

    /// Fewest traces that each side holds. A trace is a distinct trace ID among the spans of a side.
    #[serde(default)]
    pub min_traces: Option<NonZeroUsize>,

    /// Fewest spans that each side holds.
    #[serde(default)]
    pub min_spans: Option<NonZeroUsize>,

    /// Spans that each side must hold, one entry for each. An entry has a `service`, and a `name` or a `resource` or
    /// both, and a span of the side must match every field of the entry.
    #[serde(default)]
    pub spans: Vec<ExpectedSpan>,

    /// Spans that neither side may hold, one entry for each, with the fields of an entry of `spans`.
    ///
    /// Name a span that both sides drop, and name a span of the same service in `spans`, one that both sides hold, so
    /// that the check can't pass because the whole service is missing. The step rejects an entry whose service has no
    /// entry in `spans`.
    #[serde(default)]
    pub absent: Vec<ExpectedSpan>,

    /// Fewest groups of APM stats that each side holds. A group is one aggregation key, whatever its buckets.
    ///
    /// A step that compares stats and sets other expectations, or sets `require_ssi` to `false`, must set it, so that two
    /// sides with no stats do not match. It has no effect with `otlp_direct_analysis_mode`, so the step rejects it there.
    #[serde(default)]
    pub min_stats_groups: Option<NonZeroUsize>,
}

impl TracesOptions {
    /// Checks the options against the other options of the step.
    ///
    /// # Errors
    ///
    /// If an entry names no issue, has a path outside the trace statistics, the span fields and the spans on one side
    /// only, has a path that covers the trace ID of a span, or has a span path and doesn't scope itself to spans by
    /// service, name or resource, or covers whole spans without `kinds` or without a `name` or `resource` in `when`, or
    /// if an `absent` entry has a service that no `spans` entry has, or if `otlp_direct_analysis_mode` skips the statistics comparison that an option or an
    /// entry's path applies to, or if a step that compares stats sets expectations or turns off `require_ssi` and sets
    /// no `min_stats_groups`, an error is returned.
    pub fn validate(&self, otlp_direct_analysis_mode: bool) -> Result<(), String> {
        if otlp_direct_analysis_mode {
            let compares_nothing = "which compares no stats";
            if let Some(entry) = self
                .known_differences
                .iter()
                .find(|entry| entry.paths.iter().any(|path| path.starts_with(STATS_PREFIX)))
            {
                return Err(format!(
                    "'otlp_direct_analysis_mode' skips the trace stats comparison, so known difference '{}' can't \
                     cover a '{}*' path",
                    entry.gap, STATS_PREFIX
                ));
            }
            if self.require_ssi.is_some() {
                return Err(format!(
                    "'traces.require_ssi' has no effect with 'otlp_direct_analysis_mode', {} and checks no Single \
                     Step Instrumentation metadata",
                    compares_nothing
                ));
            }
            if self.min_stats_groups.is_some() {
                return Err(format!(
                    "'traces.min_stats_groups' has no effect with 'otlp_direct_analysis_mode', {}",
                    compares_nothing
                ));
            }
        } else if self.min_stats_groups.is_none()
            && (self.require_ssi == Some(false)
                || self.min_traces.is_some()
                || self.min_spans.is_some()
                || !self.spans.is_empty()
                || !self.absent.is_empty())
        {
            return Err(
                "'traces' sets expectations or turns off 'require_ssi', so it must set 'min_stats_groups' while \
                 the step compares stats. Set 'otlp_direct_analysis_mode' to compare spans only"
                    .to_string(),
            );
        }
        for entry in &self.known_differences {
            if entry.issue.is_none() {
                return Err(format!("known difference '{}' names no 'issue'", entry.gap));
            }
            if let Some(path) = entry.paths.iter().find(|path| {
                !path.starts_with(STATS_PREFIX) && !path.starts_with(SPAN_PREFIX) && path.as_str() != SPANS_PATH
            }) {
                return Err(format!(
                    "known difference '{}' has the path '{}': a traces known difference covers only '{}*' and '{}*' \
                     paths, and '{}'",
                    entry.gap, path, STATS_PREFIX, SPAN_PREFIX, SPANS_PATH
                ));
            }
            let trace_id = format!("{}{}", SPAN_PREFIX, TRACE_ID_FIELD);
            if let Some(path) = entry.paths.iter().find(|path| path_matches(path, &trace_id)) {
                return Err(format!(
                    "known difference '{}' has the path '{}', which covers '{}': a split trace shows up only there",
                    entry.gap, path, trace_id
                ));
            }
            let scoped = !entry.when.is_empty()
                && entry
                    .when
                    .keys()
                    .all(|field| SPAN_VIEW_FIELDS.contains(&field.as_str()));
            if entry
                .paths
                .iter()
                .any(|path| path.starts_with(SPAN_PREFIX) || path.as_str() == SPANS_PATH)
                && !scoped
            {
                return Err(format!(
                    "known difference '{}' covers spans, so its `when` must name the spans by {}, and nothing else",
                    entry.gap,
                    SPAN_VIEW_FIELDS.join(", ")
                ));
            }
            // A span on one side only is excused as a whole, with all of its fields. The entry must not excuse every
            // span of a service, nor both a lost span ID's `missing` copy and its `extra` copy.
            if entry.paths.iter().any(|path| path.as_str() == SPANS_PATH)
                && (entry.kinds.is_empty() || !(entry.when.contains_key("name") || entry.when.contains_key("resource")))
            {
                return Err(format!(
                    "known difference '{}' covers whole spans, so it must set `kinds` and its `when` must name the \
                     spans by `name` or `resource`",
                    entry.gap
                ));
            }
        }
        if let Some(unexpected) = self
            .absent
            .iter()
            .find(|unexpected| !self.spans.iter().any(|span| span.service() == unexpected.service()))
        {
            return Err(format!(
                "`absent` names a span of service '{}', but `spans` holds no span of that service. Add one that both \
                 sides hold, so that the check can't pass because the whole service is missing",
                unexpected.service()
            ));
        }
        Ok(())
    }

    /// Returns what each side must hold.
    pub fn expectations(&self) -> TracesExpectations {
        TracesExpectations {
            min_traces: self.min_traces,
            min_spans: self.min_spans,
            spans: self.spans.clone(),
            absent: self.absent.clone(),
            min_stats_groups: self.min_stats_groups,
        }
    }
}

/// Analyzes traces for correctness.
pub struct TracesAnalyzer {
    baseline_total_traces: usize,
    baseline_spans: HashMap<u64, Span>,
    baseline_ssi_metadata_present: bool,
    baseline_trace_stats: FastHashMap<AggregationKey, Value>,
    comparison_total_traces: usize,
    comparison_spans: HashMap<u64, Span>,
    comparison_ssi_metadata_present: bool,
    comparison_trace_stats: FastHashMap<AggregationKey, Value>,
    options: TracesAnalysisOptions,
    ignored_fields: HashSet<String>,
}

impl TracesAnalyzer {
    /// Creates a new `TracesAnalyzer` instance with the given baseline/comparison data and options.
    pub fn new(
        baseline_data: &CollectedData, comparison_data: &CollectedData, options: TracesAnalysisOptions,
    ) -> Result<Self, GenericError> {
        let mut baseline_traces = HashSet::new();
        let mut baseline_spans = HashMap::new();
        let mut baseline_ssi_metadata_present = false;
        let mut comparison_traces = HashSet::new();
        let mut comparison_spans = HashMap::new();
        let mut comparison_ssi_metadata_present = false;

        for span in baseline_data.spans() {
            if span.get_meta_field("_dd.install.id").is_some() {
                baseline_ssi_metadata_present = true;
            }

            baseline_traces.insert(span.trace_id());
            if baseline_spans.insert(span.span_id(), span.clone()).is_some() {
                return Err(generic_error!(
                    "Duplicate span ID '{}' in baseline data",
                    span.span_id()
                ));
            }
        }

        for span in comparison_data.spans() {
            if span.get_meta_field("_dd.install.id").is_some() {
                comparison_ssi_metadata_present = true;
            }

            comparison_traces.insert(span.trace_id());
            if comparison_spans.insert(span.span_id(), span.clone()).is_some() {
                return Err(generic_error!(
                    "Duplicate span ID '{}' in comparison data",
                    span.span_id()
                ));
            }
        }

        let baseline_trace_stats = get_merged_trace_stats(baseline_data.trace_stats().groups())
            .error_context("Failed to get merged trace statistics from baseline.")?;

        let comparison_trace_stats = get_merged_trace_stats(comparison_data.trace_stats().groups())
            .error_context("Failed to get merged trace statistics from comparison.")?;

        let ignored_fields = BASE_IGNORED_FIELDS_DIFF
            .iter()
            .map(|s| (*s).to_string())
            .chain(options.additional_span_ignore_fields.iter().cloned())
            .collect::<HashSet<_>>();

        Ok(Self {
            baseline_total_traces: baseline_traces.len(),
            baseline_spans,
            baseline_ssi_metadata_present,
            baseline_trace_stats,
            comparison_total_traces: comparison_traces.len(),
            comparison_spans,
            comparison_ssi_metadata_present,
            comparison_trace_stats,
            options,
            ignored_fields,
        })
    }

    /// Analyzes the raw spans from both the baseline and comparison targets, comparing them to one another.
    ///
    /// # Errors
    ///
    /// If analysis fails, an error will be returned with specific details and the full list of mismatches.
    pub fn run_analysis(self) -> Result<(), (GenericError, Vec<String>)> {
        // Each side must hold what the case expects: two sides that hold no spans and no stats are equal. An unmet
        // expectation does not stop the comparison, so the failure reports both, with the expectations first.
        let unmet = self
            .options
            .expectations
            .check(
                &HeldTraces {
                    spans: &self.baseline_spans,
                    traces: self.baseline_total_traces,
                    stats_groups: self.baseline_trace_stats.len(),
                },
                &HeldTraces {
                    spans: &self.comparison_spans,
                    traces: self.comparison_total_traces,
                    stats_groups: self.comparison_trace_stats.len(),
                },
            )
            .err();
        let compared = self.compare().err();
        join_failures([unmet, compared])
    }

    /// Compares the spans, the stats and the SSI metadata of both sides.
    fn compare(self) -> Result<(), (GenericError, Vec<String>)> {
        let mut diff_recorder = DifferenceRecorder::new(self.ignored_fields);

        info!(
            "Analyzing {} traces ({} spans) from baseline and {} traces ({} spans) from comparison target.",
            self.baseline_total_traces,
            self.baseline_spans.len(),
            self.comparison_total_traces,
            self.comparison_spans.len()
        );

        // Every difference is reported: a span or a stats group on one side only does not stop the comparison of the
        // spans and the groups that both sides hold. A field difference that a known difference covers is left out.
        let mut ratchet = Ratchet::new(&self.options.known_differences);
        let span_mismatches = compare_spans(
            &self.baseline_spans,
            &self.comparison_spans,
            &mut diff_recorder,
            &mut ratchet,
        );
        for mismatch in &span_mismatches {
            error!("Detected mismatched span:\n{}", mismatch.render(usize::MAX));
        }

        // Compare the baseline and comparison trace statistics when not in OTLP-direct mode.
        let mut stats_mismatches = Vec::new();
        if !self.options.otlp_direct_analysis_mode {
            info!(
                "Analyzing {} aggregated statistics groups from baseline and {} from comparison target.",
                self.baseline_trace_stats.len(),
                self.comparison_trace_stats.len()
            );

            stats_mismatches = compare_stats(
                &self.baseline_trace_stats,
                &self.comparison_trace_stats,
                &mut diff_recorder,
                &mut ratchet,
            );
            for mismatch in &stats_mismatches {
                error!("Detected mismatched stats group:\n{}", mismatch.render(usize::MAX));
            }
        }
        let mut error_count = span_mismatches.len() + stats_mismatches.len();

        // A known difference that matched nothing is stale: its gap is fixed, or the entry is wrong.
        let stale = ratchet
            .stale_entries()
            .into_iter()
            .map(|stale| format!("  {}", stale))
            .collect::<Vec<_>>();
        for line in &stale {
            error!("{}", line.trim_start());
            error_count += 1;
        }

        // Ensure that we observe at least one span on each side where Single Step Instrumentation-related metadata is
        // present, unless the step is in OTLP-direct mode or the case sets `require_ssi: false`.
        if !self.options.otlp_direct_analysis_mode && self.options.require_ssi {
            if !self.baseline_ssi_metadata_present {
                error!("No Single Step Instrumentation metadata found in baseline spans.");
                error_count += 1;
            }
            if !self.comparison_ssi_metadata_present {
                error!("No Single Step Instrumentation metadata found in comparison spans.");
                error_count += 1;
            }
        }

        if error_count > 0 {
            let mut msg = format!(
                "{} mismatched span(s)/stats group(s) between baseline and comparison.",
                error_count
            );
            let mut all_details = Vec::new();
            let only_in_baseline = self
                .baseline_spans
                .keys()
                .filter(|span_id| !self.comparison_spans.contains_key(span_id))
                .count();
            let only_in_comparison = self
                .comparison_spans
                .keys()
                .filter(|span_id| !self.baseline_spans.contains_key(span_id))
                .count();
            if only_in_baseline + only_in_comparison > 0 {
                let counts = format!(
                    "  {} span(s) only in baseline, {} only in comparison.",
                    only_in_baseline, only_in_comparison
                );
                msg.push('\n');
                msg.push_str(&counts);
                all_details.push(counts);
            }
            for (mismatches, label) in [(&span_mismatches, "span(s)"), (&stats_mismatches, "stats group(s)")] {
                if mismatches.is_empty() {
                    continue;
                }
                let samples = mismatches.iter().take(SAMPLE_MISMATCH_LIMIT).collect::<Vec<_>>();
                msg.push_str(&format!(
                    "\n  (showing {} of {} mismatched {})",
                    samples.len(),
                    mismatches.len(),
                    label
                ));
                for sample in samples {
                    msg.push('\n');
                    msg.push_str(&sample.render(SAMPLE_DIFFS_PER_SPAN));
                }
            }

            if !stale.is_empty() {
                msg.push_str(&format!(
                    "\n  ({} known difference path(s) no longer occur)",
                    stale.len()
                ));
                for line in &stale {
                    msg.push('\n');
                    msg.push_str(line);
                }
            }

            all_details.extend(
                span_mismatches
                    .iter()
                    .chain(&stats_mismatches)
                    .map(|mismatch| mismatch.render(usize::MAX)),
            );
            all_details.extend(stale);

            Err((generic_error!("{}", msg), all_details))
        } else {
            Ok(())
        }
    }
}

/// A difference that `DifferenceRecorder` found, with what a custom comparator said about it.
struct RecordedDiff {
    /// The path of the field, the kind of difference, and the value on each side.
    diff: FieldDiff,

    /// Why the custom comparator of the field rejected the two values, such as a start time difference over the
    /// allowed maximum. A failure shows it after the values. `None` when no custom comparator checks the field.
    detail: Option<String>,
}

impl fmt::Display for RecordedDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let show = |value: &Option<Value>| value.as_ref().map_or_else(|| "<missing>".to_string(), Value::to_string);
        write!(
            f,
            "{}: baseline={} comparison={}",
            self.diff.path,
            show(&self.diff.baseline),
            show(&self.diff.comparison)
        )?;
        if let Some(detail) = &self.detail {
            write!(f, " ({})", detail)?;
        }
        Ok(())
    }
}

struct DifferenceRecorder {
    key_stack: Vec<Key>,
    detected_diffs: Vec<RecordedDiff>,
    ignored_fields: HashSet<String>,
    custom_field_comparators: HashMap<&'static str, &'static dyn FieldComparator>,
}

impl DifferenceRecorder {
    fn new(ignored_fields: HashSet<String>) -> Self {
        let custom_field_comparators = CUSTOM_FIELD_COMPARATORS.iter().copied().collect::<HashMap<_, _>>();

        Self {
            key_stack: Vec::new(),
            detected_diffs: Vec::new(),
            ignored_fields,
            custom_field_comparators,
        }
    }

    fn clear(&mut self) {
        self.key_stack.clear();
        self.detected_diffs.clear();
    }

    fn diffs(&self) -> &[RecordedDiff] {
        &self.detected_diffs
    }

    fn get_field_path(&self) -> String {
        let mut field_path = String::new();
        for key_segment in &self.key_stack {
            match key_segment {
                Key::String(key) => {
                    if !field_path.is_empty() {
                        field_path.push('.');
                    }
                    field_path.push_str(key);
                }
                Key::Index(idx) => {
                    field_path.push('[');
                    field_path.push_str(&idx.to_string());
                    field_path.push(']');
                }
            }
        }
        field_path
    }

    fn process_difference(
        &mut self, maybe_baseline: Option<&serde_json::Value>, maybe_comparison: Option<&serde_json::Value>,
    ) {
        let field_path = self.get_field_path();

        // See if this is an ignored field.
        if self.ignored_fields.contains(field_path.as_str()) {
            return;
        }

        // See if we have a special comparison function defined for this field, and use it if so. We only do this
        // when both values are present.
        //
        // Some fields require a looser comparison than what we get from the derived implementation of `PartialEq`.
        let maybe_diff_detail = match (maybe_baseline, maybe_comparison) {
            (Some(baseline), Some(comparison)) => match self.custom_field_comparators.get(field_path.as_str()) {
                Some(comparator) => match comparator.compare(baseline, comparison) {
                    Ok(()) => return,
                    Err(diff_detail) => Some(diff_detail),
                },
                None => None,
            },
            _ => None,
        };

        let kind = match (maybe_baseline, maybe_comparison) {
            (Some(_), None) => DiffKind::Missing,
            (None, Some(_)) => DiffKind::Extra,
            _ => DiffKind::Value,
        };
        self.detected_diffs.push(RecordedDiff {
            diff: FieldDiff {
                path: field_path,
                kind,
                baseline: maybe_baseline.cloned(),
                comparison: maybe_comparison.cloned(),
            },
            detail: maybe_diff_detail,
        });
    }

    fn process_difference_with_key(
        &mut self, key: &Key, maybe_baseline: Option<&serde_json::Value>, maybe_comparison: Option<&serde_json::Value>,
    ) {
        self.key_stack.push(key.clone());
        self.process_difference(maybe_baseline, maybe_comparison);
        self.key_stack.pop();
    }
}

impl<'a> treediff::Delegate<'a, Key, serde_json::Value> for DifferenceRecorder {
    fn push(&mut self, k: &Key) {
        self.key_stack.push(k.clone())
    }

    fn pop(&mut self) {
        self.key_stack.pop();
    }

    fn removed<'b>(&mut self, k: &'b Key, v: &'a serde_json::Value) {
        // Baseline field exists, comparison field missing.
        self.process_difference_with_key(k, Some(v), None);
    }

    fn added<'b>(&mut self, k: &'b Key, v: &'a serde_json::Value) {
        // Comparison field exists, baseline field missing.
        self.process_difference_with_key(k, None, Some(v));
    }

    fn unchanged(&mut self, _v: &'a serde_json::Value) {
        // Nothing to do for unchanged values.
    }

    fn modified(&mut self, v1: &'a serde_json::Value, v2: &'a serde_json::Value) {
        // Field exists in both baseline and comparison, but differs.
        self.process_difference(Some(v1), Some(v2));
    }
}

fn check_start_diff(baseline_value: &Value, comparison_value: &Value) -> Result<(), String> {
    // Depending on how traces/spans are generated, we may end up with a slight difference in the start time.
    //
    // Currently, the primary scenario is that `millstone` starts at different times for the baseline and comparison
    // targets, which means that there will be a slight offset between the start times for a given span. We allow for
    // up to a _four second_ difference, which should generally account for this.
    const MAX_START_DELTA: Duration = Duration::from_millis(4000);

    match (baseline_value, comparison_value) {
        (Value::Number(baseline_start), Value::Number(comparison_start)) => {
            match (baseline_start.as_u64(), comparison_start.as_u64()) {
                (Some(baseline_start_ns), Some(comparison_start_ns)) => {
                    let start_delta = if baseline_start_ns > comparison_start_ns {
                        Duration::from_nanos(baseline_start_ns - comparison_start_ns)
                    } else {
                        Duration::from_nanos(comparison_start_ns - baseline_start_ns)
                    };

                    if start_delta > MAX_START_DELTA {
                        Err(format!(
                            "start time difference greater than maximum allowed: {:?} > {:?}",
                            start_delta, MAX_START_DELTA
                        ))
                    } else {
                        Ok(())
                    }
                }
                _ => Err(format!(
                    "mismatched numerical types: {} vs {}",
                    get_number_type_str(baseline_start),
                    get_number_type_str(comparison_start)
                )),
            }
        }
        _ => Err(format!(
            "mismatched types: {} vs {}",
            get_value_type_str(baseline_value),
            get_value_type_str(comparison_value)
        )),
    }
}

fn get_value_type_str(value: &Value) -> &'static str {
    match value {
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Bool(_) => "boolean",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
        Value::Null => "null",
    }
}

fn get_number_type_str(number: &Number) -> &'static str {
    if number.is_f64() {
        "double"
    } else if number.is_i64() {
        "signed-int"
    } else {
        "unsigned-int"
    }
}

fn get_merged_trace_stats(
    groups: &FastHashMap<AggregationKey, BucketedClientStatistics>,
) -> Result<FastHashMap<AggregationKey, Value>, GenericError> {
    let mut merged_trace_stats = FastHashMap::default();
    for (k, v) in groups {
        let merged_stats = match v.merged() {
            Ok(None) => {
                return Err(generic_error!(
                    "Aggregation key '{}' had no associated client statistics.",
                    k
                ))
            }
            Ok(Some(stats)) => stats,
            Err(e) => return Err(e).error_context("Failed to merge client statstics."),
        };

        let merged_stats =
            serde_json::to_value(&merged_stats).error_context("Failed to serialize client statistics.")?;
        merged_trace_stats.insert(k.clone(), merged_stats);
    }

    Ok(merged_trace_stats)
}

/// A span or a statistics group whose differences no known difference covers.
struct Mismatch {
    /// The span or the group, named by its fields.
    subject: String,

    /// The differences, one line each.
    diffs: Vec<String>,
}

impl Mismatch {
    /// Renders the subject and up to `limit` of its differences.
    fn render(&self, limit: usize) -> String {
        let mut text = format!("  {}:", self.subject);
        for diff in self.diffs.iter().take(limit) {
            text.push_str("\n    ");
            text.push_str(diff);
        }
        text
    }
}

/// Compares the spans of both sides by span ID, and returns the spans with differences that no entry of `ratchet`
/// covers.
///
/// A span on one side only is a difference at `spans`: `missing` when only the baseline has it, and `extra` when only
/// the comparison has it. One that no entry covers is a mismatch of its own, which does not stop the comparison of the
/// other spans. These come first, named with their trace ID and in its order, so that the spans of a lost trace stay
/// together. The spans whose fields differ follow, in the order of their IDs. A field that differs in a span on both
/// sides is a difference at `span.<field>`. The view that an entry's `when` matches is the span's service, name and
/// resource, from the baseline, or from the comparison for a span that only it has.
fn compare_spans(
    baseline: &HashMap<u64, Span>, comparison: &HashMap<u64, Span>, recorder: &mut DifferenceRecorder,
    ratchet: &mut Ratchet<'_>,
) -> Vec<Mismatch> {
    let mut one_sided = Vec::new();
    let mut differing = Vec::new();
    for (span_id, baseline_span) in baseline {
        let Some(comparison_span) = comparison.get(span_id) else {
            one_sided.extend(uncovered_one_sided_span(baseline_span, DiffKind::Missing, ratchet));
            continue;
        };
        let baseline_span = serde_json::to_value(baseline_span).unwrap();
        let comparison_span = serde_json::to_value(comparison_span).unwrap();
        recorder.clear();
        treediff::diff(&baseline_span, &comparison_span, recorder);
        let view = span_view(&baseline_span);
        let unknown = recorder
            .diffs()
            .iter()
            .filter(|recorded| {
                let diff = FieldDiff {
                    path: format!("{}{}", SPAN_PREFIX, recorded.diff.path),
                    ..recorded.diff.clone()
                };
                !ratchet.allows(&diff, &view)
            })
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        if !unknown.is_empty() {
            let mismatch = Mismatch {
                subject: format!("span {} ({})", span_id, span_name(&view)),
                diffs: unknown,
            };
            differing.push((*span_id, mismatch));
        }
    }
    for (span_id, comparison_span) in comparison {
        if !baseline.contains_key(span_id) {
            one_sided.extend(uncovered_one_sided_span(comparison_span, DiffKind::Extra, ratchet));
        }
    }

    one_sided.sort_by_key(|(key, _)| *key);
    differing.sort_by_key(|(span_id, _)| *span_id);
    one_sided
        .into_iter()
        .map(|(_, mismatch)| mismatch)
        .chain(differing.into_iter().map(|(_, mismatch)| mismatch))
        .collect()
}

/// Returns the mismatch of a span on one side only, with its trace ID and span ID, unless an entry of `ratchet` covers
/// it. `kind` is `Missing` for a span that only the baseline has, and `Extra` for one that only the comparison has.
fn uncovered_one_sided_span(span: &Span, kind: DiffKind, ratchet: &mut Ratchet<'_>) -> Option<((u64, u64), Mismatch)> {
    let value = serde_json::to_value(span).unwrap();
    let view = span_view(&value);
    let (side, diff) = match kind {
        DiffKind::Missing => (
            "baseline",
            FieldDiff {
                path: SPANS_PATH.to_string(),
                kind,
                baseline: Some(value),
                comparison: None,
            },
        ),
        _ => (
            "comparison",
            FieldDiff {
                path: SPANS_PATH.to_string(),
                kind,
                baseline: None,
                comparison: Some(value),
            },
        ),
    };
    if ratchet.allows(&diff, &view) {
        return None;
    }
    let mismatch = Mismatch {
        subject: format!(
            "span {} (trace_id={} {})",
            span.span_id(),
            span.trace_id(),
            span_name(&view)
        ),
        diffs: vec![format!("only in {}", side)],
    };
    Some(((span.trace_id(), span.span_id()), mismatch))
}

/// Compares the statistics groups of both sides, and returns the groups with differences that no entry of `ratchet`
/// covers, in the order of their names.
///
/// A group on one side only is a difference at `stats.groups`: `missing` when only the baseline has it, and `extra` when
/// only the comparison has it. A field that differs in a group on both sides is a difference at `stats.<field>`, however
/// deep inside the field the difference is. The view that an entry's `when` matches is the group's fields, from the side that has the group, or the baseline.
fn compare_stats(
    baseline: &FastHashMap<AggregationKey, Value>, comparison: &FastHashMap<AggregationKey, Value>,
    recorder: &mut DifferenceRecorder, ratchet: &mut Ratchet<'_>,
) -> Vec<Mismatch> {
    let mut mismatches = Vec::new();
    for (key, baseline_group) in baseline {
        let view = stats_group_view(baseline_group);
        let unknown = match comparison.get(key) {
            Some(comparison_group) => {
                recorder.clear();
                treediff::diff(baseline_group, comparison_group, recorder);
                recorder
                    .diffs()
                    .iter()
                    .filter(|recorded| {
                        let diff = FieldDiff {
                            path: stats_field_path(&recorded.diff.path),
                            ..recorded.diff.clone()
                        };
                        !ratchet.allows(&diff, &view)
                    })
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            }
            None => {
                let diff = FieldDiff {
                    path: STATS_GROUPS_PATH.to_string(),
                    kind: DiffKind::Missing,
                    baseline: Some(baseline_group.clone()),
                    comparison: None,
                };
                if ratchet.allows(&diff, &view) {
                    Vec::new()
                } else {
                    vec!["only in baseline".to_string()]
                }
            }
        };
        if !unknown.is_empty() {
            mismatches.push(Mismatch {
                subject: format!("stats group {}", stats_group_name(baseline_group)),
                diffs: unknown,
            });
        }
    }

    for (key, comparison_group) in comparison {
        if baseline.contains_key(key) {
            continue;
        }
        let diff = FieldDiff {
            path: STATS_GROUPS_PATH.to_string(),
            kind: DiffKind::Extra,
            baseline: None,
            comparison: Some(comparison_group.clone()),
        };
        if !ratchet.allows(&diff, &stats_group_view(comparison_group)) {
            mismatches.push(Mismatch {
                subject: format!("stats group {}", stats_group_name(comparison_group)),
                diffs: vec!["only in comparison".to_string()],
            });
        }
    }

    mismatches.sort_by(|a, b| a.subject.cmp(&b.subject));
    mismatches
}

/// Returns the `stats.<field>` path for the path of a difference inside a statistics group. The path of a nested or
/// array difference, such as `ok_summary[3]`, is cut at the field, because a glob segment cannot match `ok_summary[3]`.
fn stats_field_path(path: &str) -> String {
    let field = path.find(['.', '[']).map_or(path, |end| &path[..end]);
    format!("{STATS_PREFIX}{field}")
}

/// The service, name and resource of a span, as the view that a known difference's `when` matches.
fn span_view(span: &Value) -> BTreeMap<String, Value> {
    SPAN_VIEW_FIELDS
        .iter()
        .filter_map(|field| Some((field.to_string(), span.get(field)?.clone())))
        .collect()
}

/// Names a span by its view, as `service="..." name="..." resource="..."`.
fn span_name(view: &BTreeMap<String, Value>) -> String {
    SPAN_VIEW_FIELDS
        .iter()
        .filter_map(|field| Some(format!("{}={}", field, view.get(*field)?)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The fields of a statistics group, as the view that a known difference's `when` matches.
fn stats_group_view(group: &Value) -> BTreeMap<String, Value> {
    group
        .as_object()
        .map(|fields| {
            fields
                .iter()
                .map(|(field, value)| (field.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Names a statistics group by the fields in `STATS_GROUP_NAME_FIELDS` that are not at their default value.
fn stats_group_name(group: &Value) -> String {
    let is_default = |value: &Value| match value {
        Value::Null => true,
        Value::Bool(value) => !value,
        Value::Number(value) => value.as_u64() == Some(0),
        Value::String(value) => value.is_empty(),
        Value::Array(values) => values.is_empty(),
        Value::Object(fields) => fields.is_empty(),
    };
    STATS_GROUP_NAME_FIELDS
        .iter()
        .filter_map(|field| {
            let value = group.get(field)?;
            (!is_default(value)).then(|| format!("{}={}", field, value))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn key(value: u64) -> AggregationKey {
        value.to_string().parse().unwrap()
    }

    fn group(resource: &str, hits: u64) -> Value {
        json!({"service": "svc", "name": "op", "resource": resource, "hits": hits, "errors": 0})
    }

    fn known(gap: &str, paths: &[&str], kinds: &[&str]) -> KnownDifference {
        serde_json::from_value(json!({"gap": gap, "issue": 1, "paths": paths, "kinds": kinds, "reason": "test"}))
            .expect("entry should parse")
    }

    fn compare(
        baseline: &[(u64, Value)], comparison: &[(u64, Value)], entries: &[KnownDifference],
    ) -> (Vec<String>, Vec<String>) {
        let side = |groups: &[(u64, Value)]| {
            groups
                .iter()
                .map(|(k, group)| (key(*k), group.clone()))
                .collect::<FastHashMap<_, _>>()
        };
        let mut recorder = DifferenceRecorder::new(HashSet::new());
        let mut ratchet = Ratchet::new(entries);
        let mismatches = compare_stats(&side(baseline), &side(comparison), &mut recorder, &mut ratchet);
        let rendered = mismatches.iter().map(|mismatch| mismatch.render(usize::MAX)).collect();
        (rendered, ratchet.stale_entries())
    }

    #[test]
    fn every_stats_difference_is_reported() {
        let (mismatches, _) = compare(
            &[(1, group("a", 2)), (2, group("b", 1))],
            &[(1, group("a", 1)), (3, group("c", 1))],
            &[],
        );

        assert_eq!(
            mismatches,
            vec![
                "  stats group service=\"svc\" name=\"op\" resource=\"a\":\n    hits: baseline=2 comparison=1",
                "  stats group service=\"svc\" name=\"op\" resource=\"b\":\n    only in baseline",
                "  stats group service=\"svc\" name=\"op\" resource=\"c\":\n    only in comparison",
            ]
        );
    }

    #[test]
    fn known_stats_differences_are_allowed_by_path_and_kind() {
        let entries = [
            known("gap/missing-groups", &["stats.groups"], &["missing"]),
            known("gap/fewer-hits", &["stats.hits"], &["value"]),
        ];

        let (mismatches, stale) = compare(
            &[(1, group("a", 2)), (2, group("b", 1))],
            &[(1, group("a", 1))],
            &entries,
        );
        assert!(mismatches.is_empty(), "{:?}", mismatches);
        assert!(stale.is_empty(), "{:?}", stale);

        // A group only in the comparison is `extra`, which the entry does not cover.
        let (mismatches, _) = compare(
            &[(1, group("a", 2))],
            &[(1, group("a", 1)), (3, group("c", 1))],
            &entries,
        );
        assert_eq!(mismatches.len(), 1, "{:?}", mismatches);
        assert!(mismatches[0].contains("only in comparison"), "{}", mismatches[0]);
    }

    #[test]
    fn a_known_stats_difference_covers_nested_differences_of_the_field() {
        let sketch = |bins: Value| json!({"service": "svc", "name": "op", "ok_summary": bins});
        let entries = [known("gap/sketch", &["stats.ok_summary"], &["value"])];

        let (mismatches, stale) = compare(
            &[(1, sketch(json!([1, 2, 3, 4]))), (2, sketch(json!({"bins": [1, 2]})))],
            &[(1, sketch(json!([1, 2, 3, 5]))), (2, sketch(json!({"bins": [1, 3]})))],
            &entries,
        );

        assert!(mismatches.is_empty(), "{:?}", mismatches);
        assert!(stale.is_empty(), "{:?}", stale);
    }

    #[test]
    fn known_stats_differences_can_be_scoped_to_groups() {
        let mut entry = known("gap/fewer-hits", &["stats.hits"], &["value"]);
        entry.when = BTreeMap::from([("resource".to_string(), vec![json!("a")])]);

        let (mismatches, stale) = compare(
            &[(1, group("a", 2)), (2, group("b", 2))],
            &[(1, group("a", 1)), (2, group("b", 1))],
            &[entry],
        );

        assert_eq!(mismatches.len(), 1, "{:?}", mismatches);
        assert!(mismatches[0].contains("resource=\"b\""), "{}", mismatches[0]);
        assert!(stale.is_empty(), "{:?}", stale);
    }

    #[test]
    fn a_known_stats_difference_is_stale_for_each_named_group_that_no_longer_differs() {
        let mut entry = known("gap/fewer-hits", &["stats.hits"], &["value"]);
        entry.when = BTreeMap::from([("resource".to_string(), vec![json!("a"), json!("b")])]);

        let (mismatches, stale) = compare(
            &[(1, group("a", 2)), (2, group("b", 1))],
            &[(1, group("a", 1)), (2, group("b", 1))],
            &[entry],
        );

        assert!(mismatches.is_empty(), "{:?}", mismatches);
        assert_eq!(stale.len(), 1, "{:?}", stale);
        assert!(
            stale[0].contains("path `stats.hits` when `resource` is \"b\""),
            "{}",
            stale[0]
        );
    }

    #[test]
    fn a_known_stats_difference_that_matches_nothing_is_stale() {
        let entries = [known("gap/missing-groups", &["stats.groups"], &["missing"])];

        let (mismatches, stale) = compare(&[(1, group("a", 1))], &[(1, group("a", 1))], &entries);

        assert!(mismatches.is_empty(), "{:?}", mismatches);
        assert_eq!(stale.len(), 1, "{:?}", stale);
        assert!(stale[0].contains("gap/missing-groups (#1)"), "{}", stale[0]);
    }
}
