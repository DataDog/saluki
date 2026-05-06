use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use datadog_protos::metrics::v3::Payload as V3Payload;
use datadog_protos::metrics::{MetricPayload, SketchPayload};
use saluki_error::GenericError;
use stele::{Metric, MetricValue};
use tracing::{debug, warn};

struct ValidationBatch {
    v2_metrics: Vec<Metric>,
    v2_received: usize,
    v2_expected: Option<usize>,
    v3_metrics: Option<Vec<Metric>>,
}

impl ValidationBatch {
    fn new() -> Self {
        Self {
            v2_metrics: Vec::new(),
            v2_received: 0,
            v2_expected: None,
            v3_metrics: None,
        }
    }

    fn is_complete(&self) -> bool {
        self.v3_metrics.is_some() && self.v2_expected.is_some_and(|expected| self.v2_received >= expected)
    }
}

#[derive(Clone)]
pub struct MetricsState {
    metrics: Arc<Mutex<Vec<Metric>>>,
    series_pairs: Arc<Mutex<HashMap<String, ValidationBatch>>>,
    sketches_pairs: Arc<Mutex<HashMap<String, ValidationBatch>>>,
}

impl MetricsState {
    pub fn new() -> Self {
        Self {
            metrics: Arc::new(Mutex::new(Vec::new())),
            series_pairs: Arc::new(Mutex::new(HashMap::new())),
            sketches_pairs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn dump_metrics(&self) -> Vec<Metric> {
        self.metrics.lock().unwrap().clone()
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
        &self, payload: MetricPayload, batch_id: String, batch_len: usize,
    ) -> Result<(), GenericError> {
        let metrics = Metric::try_from_series(payload)?;
        self.accumulate_v2(&self.series_pairs, "series", metrics, batch_id, batch_len)
    }

    pub fn accumulate_v2_sketches(
        &self, payload: SketchPayload, batch_id: String, batch_len: usize,
    ) -> Result<(), GenericError> {
        let metrics = Metric::try_from_sketch(payload)?;
        self.accumulate_v2(&self.sketches_pairs, "sketches", metrics, batch_id, batch_len)
    }

    pub fn accumulate_v3_series_and_merge(&self, payload: V3Payload, batch_id: String) -> Result<(), GenericError> {
        let metrics = Metric::try_from_v3(payload)?;
        if !self.accumulate_v3(&self.series_pairs, "series", metrics.clone(), batch_id)? {
            self.metrics.lock().unwrap().extend(metrics);
        }
        Ok(())
    }

    pub fn accumulate_v3_sketches_and_merge(&self, payload: V3Payload, batch_id: String) -> Result<(), GenericError> {
        let metrics = Metric::try_from_v3(payload)?;
        if !self.accumulate_v3(&self.sketches_pairs, "sketches", metrics.clone(), batch_id)? {
            self.metrics.lock().unwrap().extend(metrics);
        }
        Ok(())
    }

    fn accumulate_v2(
        &self, pairs: &Arc<Mutex<HashMap<String, ValidationBatch>>>, kind: &str, metrics: Vec<Metric>,
        batch_id: String, batch_len: usize,
    ) -> Result<(), GenericError> {
        let mut map = pairs.lock().unwrap();
        let batch = map.entry(batch_id.clone()).or_insert_with(ValidationBatch::new);
        batch.v2_metrics.extend(metrics);
        batch.v2_received += 1;
        batch.v2_expected = Some(batch_len);
        if batch.is_complete() {
            let batch = map.remove(&batch_id).unwrap();
            compare_validation_pair(kind, &batch_id, &batch.v2_metrics, batch.v3_metrics.as_deref().unwrap());
        }
        Ok(())
    }

    fn accumulate_v3(
        &self, pairs: &Arc<Mutex<HashMap<String, ValidationBatch>>>, kind: &str, metrics: Vec<Metric>, batch_id: String,
    ) -> Result<bool, GenericError> {
        let mut map = pairs.lock().unwrap();
        let batch = map.entry(batch_id.clone()).or_insert_with(ValidationBatch::new);
        let was_duplicate = batch.v3_metrics.replace(metrics).is_some();
        if was_duplicate {
            warn!(
                kind,
                batch_id, "V3 metrics arrived twice for the same batch ID, overwriting."
            );
        }
        if batch.is_complete() {
            let batch = map.remove(&batch_id).unwrap();
            compare_validation_pair(kind, &batch_id, &batch.v2_metrics, batch.v3_metrics.as_deref().unwrap());
        }
        Ok(was_duplicate)
    }
}

type MetricKey = (String, Vec<String>);
type MetricPoints = Vec<(u64, MetricValue)>;

fn compare_validation_pair(kind: &str, batch_id: &str, v2: &[Metric], v3: &[Metric]) {
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
                let mut v2_sorted = v2_vals.clone();
                let mut v3_sorted = v3_vals.clone();
                v2_sorted.sort_by_key(|(ts, _)| *ts);
                v3_sorted.sort_by_key(|(ts, _)| *ts);
                let matches = v2_sorted.len() == v3_sorted.len()
                    && v2_sorted
                        .iter()
                        .zip(&v3_sorted)
                        .all(|((ts_a, val_a), (ts_b, val_b))| ts_a == ts_b && val_a == val_b);
                if !matches {
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
}
