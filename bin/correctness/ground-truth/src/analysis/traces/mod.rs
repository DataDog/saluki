use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use saluki_error::{generic_error, GenericError};
use serde_json::{Number, Value};
use stele::Span;
use tracing::{error, info};
use treediff::value::Key;

use crate::analysis::collected::CollectedData;

static IGNORED_FIELDS_DIFF: &[&str] = &[
    // Single Step Instrumentation-related metadata, which is inherently non-deterministic and not suitable for direct
    // comparison.
    "meta._dd.install.id",
    "meta._dd.install.time",
    "meta._dd.install.type",
];

static CUSTOM_FIELD_COMPARATORS: &[(&str, &dyn FieldComparator)] = &[("start", &check_start_diff)];

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

/// Analyzes traces for correctness.
pub struct TracesAnalyzer {
    baseline_total_traces: usize,
    baseline_spans: HashMap<u64, Span>,
    baseline_ssi_metadata_present: bool,
    comparison_total_traces: usize,
    comparison_spans: HashMap<u64, Span>,
    comparison_ssi_metadata_present: bool,
}

impl TracesAnalyzer {
    /// Creates a new `TracesAnalyzer` instance with the given baseline/comparison data.
    pub fn new(baseline_data: &CollectedData, comparison_data: &CollectedData) -> Result<Self, GenericError> {
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

        Ok(Self {
            baseline_total_traces: baseline_traces.len(),
            baseline_spans,
            baseline_ssi_metadata_present,
            comparison_total_traces: comparison_traces.len(),
            comparison_spans,
            comparison_ssi_metadata_present,
        })
    }

    /// Analyzes the raw spans from both the baseline and comparison targets, comparing them to one another.
    ///
    /// # Errors
    ///
    /// If analysis fails, an error will be returned with specific details.
    pub fn run_analysis(self) -> Result<(), GenericError> {
        // We should have an identical number of traces and spans.
        if self.baseline_total_traces != self.comparison_total_traces {
            return Err(generic_error!(
                "Number of traces do not match: {} (baseline) vs {} (comparison)",
                self.baseline_total_traces,
                self.comparison_total_traces
            ));
        }

        if self.baseline_spans.len() != self.comparison_spans.len() {
            return Err(generic_error!(
                "Number of spans do not match: {} (baseline) vs {} (comparison)",
                self.baseline_spans.len(),
                self.comparison_spans.len()
            ));
        }

        // We should have the same _set_ of spans in both the baseline and comparison targets.
        let baseline_span_ids = self.baseline_spans.keys().collect::<HashSet<_>>();
        let comparison_span_ids = self.comparison_spans.keys().collect::<HashSet<_>>();
        if baseline_span_ids != comparison_span_ids {
            let mut baseline_only_span_ids = baseline_span_ids.difference(&comparison_span_ids).collect::<Vec<_>>();
            baseline_only_span_ids.sort_unstable();

            let mut comparison_only_span_ids = comparison_span_ids.difference(&baseline_span_ids).collect::<Vec<_>>();
            comparison_only_span_ids.sort_unstable();

            return Err(generic_error!(
                "Baseline and comparison targets have non-overlapped set of spans: {} baseline-only spans and {} comparison-only spans.",
                baseline_only_span_ids.len(),
                comparison_only_span_ids.len()
            ));
        }

        info!(
            "Analyzing {} traces ({} spans) from baseline and comparison target.",
            self.baseline_total_traces,
            self.baseline_spans.len()
        );

        // Go through each baseline span, and look for a matching span (by ID) on the comparison target side.
        //
        // If found, compare the spans.
        let mut span_diff_recorder = SpanDifferenceRecorder::new();
        let mut error_count = 0;
        for (baseline_span_id, baseline_span) in self.baseline_spans.iter() {
            match self.comparison_spans.get(baseline_span_id) {
                Some(comparison_span) => {
                    // We serialize both spans to JSON for deep comparison.
                    let baseline_span_json = serde_json::to_value(baseline_span).unwrap();
                    let comparison_span_json = serde_json::to_value(comparison_span).unwrap();

                    span_diff_recorder.clear();
                    treediff::diff(&baseline_span_json, &comparison_span_json, &mut span_diff_recorder);

                    if !span_diff_recorder.is_empty() {
                        error!("Detected mismatched span ({}):", baseline_span_id);
                        for span_diff in span_diff_recorder.diffs() {
                            error!("- {}", span_diff);
                        }
                        error!("");

                        error_count += 1;
                    }
                }
                None => {
                    error!("Failed to find matching comparison span ({}).", baseline_span_id);
                    error_count += 1;
                }
            }
        }

        // Ensure that we observe at least one span on each side where Single Step Instrumentation-related metadata is
        // present.
        if !self.baseline_ssi_metadata_present {
            error!("No Single Step Instrumentation metadata found in baseline spans.");
            error_count += 1;
        }
        if !self.comparison_ssi_metadata_present {
            error!("No Single Step Instrumentation metadata found in comparison spans.");
            error_count += 1;
        }

        if error_count > 0 {
            Err(GenericError::msg(
                "Detected mismatched spans between baseline and comparison targets.",
            ))
        } else {
            Ok(())
        }
    }
}

struct SpanDifferenceRecorder {
    key_stack: Vec<Key>,
    detected_diffs: Vec<String>,
    ignored_fields: HashSet<&'static str>,
    custom_field_comparators: HashMap<&'static str, &'static dyn FieldComparator>,
}

impl SpanDifferenceRecorder {
    fn new() -> Self {
        let ignored_fields = IGNORED_FIELDS_DIFF.iter().copied().collect::<HashSet<_>>();
        let custom_field_comparators = CUSTOM_FIELD_COMPARATORS.iter().copied().collect::<HashMap<_, _>>();

        SpanDifferenceRecorder {
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

    fn is_empty(&self) -> bool {
        self.detected_diffs.is_empty()
    }

    fn diffs(&self) -> &[String] {
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

        // Render the explanation of the difference, including any additional details we captured.
        let baseline_value_str = maybe_baseline
            .map(|v| v.to_string())
            .unwrap_or_else(|| "<missing>".to_string());
        let comparison_value_str = maybe_comparison
            .map(|v| v.to_string())
            .unwrap_or_else(|| "<missing>".to_string());

        match maybe_diff_detail {
            None => {
                self.detected_diffs.push(format!(
                    "{}: baseline={} comparison={}",
                    field_path, baseline_value_str, comparison_value_str
                ));
            }
            Some(diff_detail) => {
                self.detected_diffs.push(format!(
                    "{}: baseline={} comparison={} ({})",
                    field_path, baseline_value_str, comparison_value_str, diff_detail
                ));
            }
        }
    }

    fn process_difference_with_key(
        &mut self, key: &Key, maybe_baseline: Option<&serde_json::Value>, maybe_comparison: Option<&serde_json::Value>,
    ) {
        self.key_stack.push(key.clone());
        self.process_difference(maybe_baseline, maybe_comparison);
        self.key_stack.pop();
    }
}

impl<'a> treediff::Delegate<'a, Key, serde_json::Value> for SpanDifferenceRecorder {
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
