use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use datadog_protos::metrics::v3::Payload as V3Payload;
use datadog_protos::metrics::{MetricPayload, SketchPayload};
use saluki_error::GenericError;
use stele::{Metric, MetricValue};
use tracing::{debug, warn};

struct ValidationBatch {
    v2_segments: HashMap<usize, Vec<Metric>>,
    v2_expected: Option<usize>,
    v3_segments: HashMap<usize, Vec<Metric>>,
    v3_expected: Option<usize>,
}

impl ValidationBatch {
    fn new() -> Self {
        Self {
            v2_segments: HashMap::new(),
            v2_expected: None,
            v3_segments: HashMap::new(),
            v3_expected: None,
        }
    }

    fn is_complete(&self) -> bool {
        segments_complete(&self.v2_segments, self.v2_expected) && segments_complete(&self.v3_segments, self.v3_expected)
    }
}

#[derive(Clone)]
struct ValidationPairs {
    pending: Arc<Mutex<HashMap<String, ValidationBatch>>>,
    completed: Arc<Mutex<HashSet<String>>>,
}

impl ValidationPairs {
    fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            completed: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

pub struct ValidationStatus {
    pub failures: Vec<String>,
    pub pending_series_batches: usize,
    pub pending_sketches_batches: usize,
}

impl ValidationStatus {
    pub fn is_ok(&self) -> bool {
        self.failures.is_empty() && self.pending_series_batches == 0 && self.pending_sketches_batches == 0
    }
}

#[derive(Clone)]
pub struct MetricsState {
    metrics: Arc<Mutex<Vec<Metric>>>,
    series_pairs: ValidationPairs,
    sketches_pairs: ValidationPairs,
    validation_failures: Arc<Mutex<Vec<String>>>,
}

impl MetricsState {
    pub fn new() -> Self {
        Self {
            metrics: Arc::new(Mutex::new(Vec::new())),
            series_pairs: ValidationPairs::new(),
            sketches_pairs: ValidationPairs::new(),
            validation_failures: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn dump_metrics(&self) -> Vec<Metric> {
        self.metrics.lock().unwrap().clone()
    }

    pub fn validation_status(&self) -> ValidationStatus {
        let pending_series_batches = self.series_pairs.pending.lock().unwrap().len();
        let pending_sketches_batches = self.sketches_pairs.pending.lock().unwrap().len();
        let failures = self.validation_failures.lock().unwrap().clone();

        ValidationStatus {
            failures,
            pending_series_batches,
            pending_sketches_batches,
        }
    }

    pub fn merge_series_payload(&self, payload: MetricPayload) -> Result<(), GenericError> {
        let metrics = Metric::try_from_series(payload)?;
        self.metrics.lock().unwrap().extend(metrics);
        Ok(())
    }

    pub fn merge_sketch_payload(&self, payload: SketchPayload) -> Result<(), GenericError> {
        let metrics = Metric::try_from_sketch(payload)?;
        self.metrics.lock().unwrap().extend(metrics);
        Ok(())
    }

    pub fn merge_v3_payload(&self, payload: V3Payload) -> Result<(), GenericError> {
        let metrics = Metric::try_from_v3(payload)?;
        self.metrics.lock().unwrap().extend(metrics);
        Ok(())
    }

    pub fn accumulate_v2_series(
        &self, payload: MetricPayload, batch_id: String, batch_seq: usize, batch_len: usize,
    ) -> Result<(), GenericError> {
        let metrics = Metric::try_from_series(payload)?;
        self.accumulate_v2(&self.series_pairs, "series", metrics, batch_id, batch_seq, batch_len)
    }

    pub fn accumulate_v2_sketches(
        &self, payload: SketchPayload, batch_id: String, batch_seq: usize, batch_len: usize,
    ) -> Result<(), GenericError> {
        let metrics = Metric::try_from_sketch(payload)?;
        self.accumulate_v2(
            &self.sketches_pairs,
            "sketches",
            metrics,
            batch_id,
            batch_seq,
            batch_len,
        )
    }

    pub fn accumulate_v3_series_and_merge(
        &self, payload: V3Payload, batch_id: String, batch_seq: usize, batch_len: usize,
    ) -> Result<(), GenericError> {
        let metrics = Metric::try_from_v3(payload)?;
        if !self.accumulate_v3(
            &self.series_pairs,
            "series",
            metrics.clone(),
            batch_id,
            batch_seq,
            batch_len,
        )? {
            self.metrics.lock().unwrap().extend(metrics);
        }
        Ok(())
    }

    pub fn accumulate_v3_sketches_and_merge(
        &self, payload: V3Payload, batch_id: String, batch_seq: usize, batch_len: usize,
    ) -> Result<(), GenericError> {
        let metrics = Metric::try_from_v3(payload)?;
        if !self.accumulate_v3(
            &self.sketches_pairs,
            "sketches",
            metrics.clone(),
            batch_id,
            batch_seq,
            batch_len,
        )? {
            self.metrics.lock().unwrap().extend(metrics);
        }
        Ok(())
    }

    fn accumulate_v2(
        &self, pairs: &ValidationPairs, kind: &str, metrics: Vec<Metric>, batch_id: String, batch_seq: usize,
        batch_len: usize,
    ) -> Result<(), GenericError> {
        if pairs.completed.lock().unwrap().contains(&batch_id) {
            debug!(
                kind,
                batch_id, batch_seq, "Ignoring V2 validation segment for completed batch."
            );
            return Ok(());
        }

        if !self.validate_segment_metadata(kind, "v2", &batch_id, batch_seq, batch_len) {
            return Ok(());
        }

        let (completed_batch, validation_failures) = {
            let mut map = pairs.pending.lock().unwrap();
            let batch = map.entry(batch_id.clone()).or_insert_with(ValidationBatch::new);
            let insert_result = insert_segment(
                kind,
                "v2",
                &batch_id,
                &mut batch.v2_segments,
                &mut batch.v2_expected,
                batch_seq,
                batch_len,
                metrics,
            );
            (
                batch.is_complete().then(|| map.remove(&batch_id).unwrap()),
                insert_result.failures,
            )
        };
        self.record_validation_failures(validation_failures);
        if let Some(batch) = completed_batch {
            self.complete_validation_batch(pairs, kind, batch_id, batch);
        }

        Ok(())
    }

    fn accumulate_v3(
        &self, pairs: &ValidationPairs, kind: &str, metrics: Vec<Metric>, batch_id: String, batch_seq: usize,
        batch_len: usize,
    ) -> Result<bool, GenericError> {
        if pairs.completed.lock().unwrap().contains(&batch_id) {
            debug!(
                kind,
                batch_id, batch_seq, "Ignoring V3 validation segment for completed batch."
            );
            return Ok(true);
        }

        if !self.validate_segment_metadata(kind, "v3", &batch_id, batch_seq, batch_len) {
            return Ok(true);
        }

        let (was_duplicate, completed_batch, validation_failures) = {
            let mut map = pairs.pending.lock().unwrap();
            let batch = map.entry(batch_id.clone()).or_insert_with(ValidationBatch::new);
            let insert_result = insert_segment(
                kind,
                "v3",
                &batch_id,
                &mut batch.v3_segments,
                &mut batch.v3_expected,
                batch_seq,
                batch_len,
                metrics,
            );
            (
                insert_result.was_duplicate,
                batch.is_complete().then(|| map.remove(&batch_id).unwrap()),
                insert_result.failures,
            )
        };
        self.record_validation_failures(validation_failures);
        if let Some(batch) = completed_batch {
            self.complete_validation_batch(pairs, kind, batch_id, batch);
        }

        Ok(was_duplicate)
    }

    fn complete_validation_batch(&self, pairs: &ValidationPairs, kind: &str, batch_id: String, batch: ValidationBatch) {
        let v2_metrics = flatten_segments(batch.v2_segments);
        let v3_metrics = flatten_segments(batch.v3_segments);
        let mismatches = compare_validation_pair(kind, &batch_id, &v2_metrics, &v3_metrics);
        if mismatches > 0 {
            self.record_validation_failure(format!(
                "{kind} validation batch {batch_id} had {mismatches} mismatch(es)"
            ));
        }
        pairs.completed.lock().unwrap().insert(batch_id);
    }

    fn validate_segment_metadata(&self, kind: &str, protocol: &str, batch_id: &str, seq: usize, len: usize) -> bool {
        if len == 0 {
            self.record_validation_failure(format!(
                "{kind} validation batch {batch_id} has invalid {protocol} length 0"
            ));
            return false;
        }
        if seq >= len {
            self.record_validation_failure(format!(
                "{kind} validation batch {batch_id} has invalid {protocol} sequence {seq} for length {len}"
            ));
            return false;
        }
        true
    }

    fn record_validation_failure(&self, failure: String) {
        warn!(failure = %failure, "Metrics validation failure.");
        self.validation_failures.lock().unwrap().push(failure);
    }

    fn record_validation_failures(&self, failures: Vec<String>) {
        for failure in failures {
            self.record_validation_failure(failure);
        }
    }
}

type MetricKey = (String, Vec<String>);
type MetricPoints = Vec<(u64, MetricValue)>;

struct InsertSegmentResult {
    was_duplicate: bool,
    failures: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
fn insert_segment(
    kind: &str, protocol: &str, batch_id: &str, segments: &mut HashMap<usize, Vec<Metric>>,
    expected: &mut Option<usize>, batch_seq: usize, batch_len: usize, metrics: Vec<Metric>,
) -> InsertSegmentResult {
    let mut failures = Vec::new();

    if let Some(previous_len) = *expected {
        if previous_len != batch_len {
            failures.push(format!(
                "{kind} validation batch {batch_id} changed {protocol} length from {previous_len} to {batch_len}"
            ));
        }
    } else {
        *expected = Some(batch_len);
    }

    let was_duplicate = match segments.get(&batch_seq) {
        Some(existing) if existing == &metrics => {
            debug!(
                kind,
                protocol, batch_id, batch_seq, "Ignoring duplicate validation segment."
            );
            true
        }
        Some(_) => {
            failures.push(format!(
                "{kind} validation batch {batch_id} received conflicting {protocol} segment {batch_seq}"
            ));
            true
        }
        None => {
            segments.insert(batch_seq, metrics);
            false
        }
    };

    InsertSegmentResult {
        was_duplicate,
        failures,
    }
}

fn segments_complete(segments: &HashMap<usize, Vec<Metric>>, expected: Option<usize>) -> bool {
    expected.is_some_and(|expected| segments.len() == expected && (0..expected).all(|seq| segments.contains_key(&seq)))
}

fn flatten_segments(segments: HashMap<usize, Vec<Metric>>) -> Vec<Metric> {
    let mut ordered_segments: Vec<_> = segments.into_iter().collect();
    ordered_segments.sort_by_key(|(seq, _)| *seq);
    ordered_segments.into_iter().flat_map(|(_, metrics)| metrics).collect()
}

fn compare_validation_pair(kind: &str, batch_id: &str, v2: &[Metric], v3: &[Metric]) -> usize {
    // Normalize metric context by sorting tags so tag ordering differences don't produce false mismatches.
    let normalize_key = |m: &Metric| -> MetricKey {
        let (name, mut tags) = m.context().clone().into_parts();
        tags.sort();
        (name, tags)
    };
    let format_key = |(name, tags): &MetricKey| -> String {
        if tags.is_empty() {
            name.clone()
        } else {
            format!("{} {{{}}}", name, tags.join(", "))
        }
    };

    let mut v2_map: HashMap<MetricKey, MetricPoints> = HashMap::new();
    for m in v2 {
        v2_map
            .entry(normalize_key(m))
            .or_default()
            .extend(m.values().iter().cloned());
    }

    let mut v3_map: HashMap<MetricKey, MetricPoints> = HashMap::new();
    for m in v3 {
        v3_map
            .entry(normalize_key(m))
            .or_default()
            .extend(m.values().iter().cloned());
    }

    let mut mismatches = 0usize;

    for (ctx, v2_vals) in &v2_map {
        match v3_map.get(ctx) {
            None => {
                let context = format_key(ctx);
                warn!(kind, batch_id, context = %context, "Validation pair: context present in V2 but missing from V3");
                mismatches += 1;
            }
            Some(v3_vals) => {
                if !point_multisets_match(v2_vals, v3_vals) {
                    let context = format_key(ctx);
                    warn!(
                        kind,
                        batch_id,
                        context = %context,
                        v2_points = v2_vals.len(),
                        v3_points = v3_vals.len(),
                        "Validation pair: V2/V3 value mismatch"
                    );
                    mismatches += 1;
                }
            }
        }
    }

    for ctx in v3_map.keys() {
        if !v2_map.contains_key(ctx) {
            let context = format_key(ctx);
            warn!(kind, batch_id, context = %context, "Validation pair: context present in V3 but missing from V2");
            mismatches += 1;
        }
    }

    if mismatches == 0 {
        debug!(kind, batch_id, v2_contexts = v2_map.len(), "Validation pair matches.");
    } else {
        warn!(kind, batch_id, mismatches, "Validation pair has mismatches.");
    }

    mismatches
}

fn point_multisets_match(v2_vals: &MetricPoints, v3_vals: &MetricPoints) -> bool {
    if v2_vals.len() != v3_vals.len() {
        return false;
    }

    let mut unmatched = v3_vals.clone();
    for v2_val in v2_vals {
        let Some(pos) = unmatched
            .iter()
            .position(|v3_val| v2_val.0 == v3_val.0 && v2_val.1 == v3_val.1)
        else {
            return false;
        };
        unmatched.swap_remove(pos);
    }

    unmatched.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_batch_requires_all_expected_sequences() {
        let mut batch = ValidationBatch::new();
        batch.v2_expected = Some(2);
        batch.v2_segments.insert(1, Vec::new());
        batch.v3_expected = Some(1);
        batch.v3_segments.insert(0, Vec::new());

        assert!(!batch.is_complete());

        batch.v2_segments.insert(0, Vec::new());

        assert!(batch.is_complete());
    }

    #[test]
    fn duplicate_segments_do_not_count_twice() {
        let state = MetricsState::new();
        let mut segments = HashMap::new();
        let mut expected = None;

        let first = insert_segment(
            "series",
            "v2",
            "test-batch",
            &mut segments,
            &mut expected,
            0,
            1,
            Vec::new(),
        );
        let duplicate = insert_segment(
            "series",
            "v2",
            "test-batch",
            &mut segments,
            &mut expected,
            0,
            1,
            Vec::new(),
        );

        assert!(!first.was_duplicate);
        assert!(duplicate.was_duplicate);
        assert_eq!(1, segments.len());
        assert!(state.validation_status().is_ok());
    }

    #[test]
    fn points_match_with_same_timestamp_in_different_order() {
        let v2 = vec![
            (10, MetricValue::Gauge { value: 1.0 }),
            (10, MetricValue::Gauge { value: 2.0 }),
        ];
        let v3 = vec![
            (10, MetricValue::Gauge { value: 2.0 }),
            (10, MetricValue::Gauge { value: 1.0 }),
        ];

        assert!(point_multisets_match(&v2, &v3));
    }

    #[test]
    fn points_detect_same_timestamp_value_mismatch() {
        let v2 = vec![
            (10, MetricValue::Gauge { value: 1.0 }),
            (10, MetricValue::Gauge { value: 2.0 }),
        ];
        let v3 = vec![
            (10, MetricValue::Gauge { value: 1.0 }),
            (10, MetricValue::Gauge { value: 3.0 }),
        ];

        assert!(!point_multisets_match(&v2, &v3));
    }
}
