use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
    time::Duration,
};

use metrics::{Counter, Gauge};
use saluki_core::data_model::{
    event::metric::{context::Context, MetricValues},
    tags::{Tag, TagSet},
};
use saluki_metrics::MetricsBuilder;
use smallvec::SmallVec;

#[derive(Clone)]
struct MetricTypedGauge {
    for_counter: Gauge,
    for_gauge: Gauge,
    for_rate: Gauge,
    for_set: Gauge,
    for_histogram: Gauge,
    for_distribution: Gauge,
}

impl MetricTypedGauge {
    pub fn new(builder: &MetricsBuilder, name: &'static str) -> Self {
        Self {
            for_counter: builder.register_gauge_with_tags(name, ["metric_type:counter"]),
            for_gauge: builder.register_gauge_with_tags(name, ["metric_type:gauge"]),
            for_rate: builder.register_gauge_with_tags(name, ["metric_type:rate"]),
            for_set: builder.register_gauge_with_tags(name, ["metric_type:set"]),
            for_histogram: builder.register_gauge_with_tags(name, ["metric_type:histogram"]),
            for_distribution: builder.register_gauge_with_tags(name, ["metric_type:distribution"]),
        }
    }

    #[cfg(test)]
    pub fn noop() -> Self {
        Self {
            for_counter: Gauge::noop(),
            for_gauge: Gauge::noop(),
            for_rate: Gauge::noop(),
            for_set: Gauge::noop(),
            for_histogram: Gauge::noop(),
            for_distribution: Gauge::noop(),
        }
    }

    pub fn for_values(&self, values: &MetricValues) -> &Gauge {
        match values {
            MetricValues::Counter(_) => &self.for_counter,
            MetricValues::Gauge(_) => &self.for_gauge,
            MetricValues::Rate(_, _) => &self.for_rate,
            MetricValues::Set(_) => &self.for_set,
            MetricValues::Histogram(_) => &self.for_histogram,
            MetricValues::Distribution(_) => &self.for_distribution,
        }
    }
}

/// Number of series and sketches flushed in a single flush.
#[derive(Default)]
pub struct FlushCounts {
    series: u64,
    sketches: u64,
}

#[derive(Clone)]
pub struct Telemetry {
    active_contexts: Gauge,
    active_contexts_by_type: MetricTypedGauge,
    active_contexts_bytes_by_type: MetricTypedGauge,
    events_dropped: Counter,
    flushes: Counter,
    series_flushed: Counter,
    sketches_flushed: Counter,
    last_flush_series: Gauge,
    last_flush_sketches: Gauge,
    last_flush_duration: Gauge,
    origin_contexts: Option<OriginContextTelemetry>,
}

#[derive(Clone)]
struct OriginContextTelemetry {
    builder: MetricsBuilder,
    contexts: Arc<Mutex<HashMap<OriginTagSetKey, OriginContextGauge>>>,
}

pub(super) struct OriginContextGauge {
    gauge: Gauge,
    count: usize,
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub(super) struct OriginTagSetKey(SmallVec<[Tag; 8]>);

const ORIGIN_LABEL_KEY_PREFIX: &str = "origin_tag_";
const HEX_DIGITS: &[u8; 16] = b"0123456789ABCDEF";

fn encode_origin_tags<'a, I>(tags: I) -> SmallVec<[String; 8]>
where
    I: IntoIterator<Item = &'a Tag>,
{
    let mut values_by_key: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for tag in tags {
        if let Some((key, value)) = tag.as_str().split_once(':') {
            values_by_key.entry(key).or_default().push(value);
        }
    }

    let mut encoded_tags = SmallVec::new();
    for (key, mut values) in values_by_key {
        values.sort_unstable();
        values.dedup();

        if values.len() == 1 {
            let encoded_key = encode_origin_label_key(key, false);
            let encoded_value = encode_origin_label_value(values[0]);
            encoded_tags.push(format!("{encoded_key}:{encoded_value}"));
        } else {
            let encoded_key = encode_origin_label_key(key, true);
            for (index, value) in values.into_iter().enumerate() {
                let encoded_value = encode_origin_label_value(value);
                encoded_tags.push(format!("{encoded_key}_{index}:{encoded_value}"));
            }
        }
    }

    encoded_tags
}

fn encode_origin_label_key(key: &str, force_encoding: bool) -> String {
    let mut chars = key.chars();
    let valid_start = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let valid_rest = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !force_encoding
        && valid_start
        && valid_rest
        && !key.starts_with("__")
        && !key.starts_with(ORIGIN_LABEL_KEY_PREFIX)
    {
        return key.to_owned();
    }

    let mut encoded = String::from(ORIGIN_LABEL_KEY_PREFIX);
    for byte in key.as_bytes() {
        encoded.push(HEX_DIGITS[(byte >> 4) as usize] as char);
        encoded.push(HEX_DIGITS[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn encode_origin_label_value(value: &str) -> String {
    let mut encoded = String::new();
    append_percent_encoded(value, &mut encoded);
    encoded
}

fn append_percent_encoded(value: &str, output: &mut String) {
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            output.push(byte as char);
        } else {
            output.push('%');
            output.push(HEX_DIGITS[(byte >> 4) as usize] as char);
            output.push(HEX_DIGITS[(byte & 0x0f) as usize] as char);
        }
    }
}

impl OriginTagSetKey {
    fn new(tags: &TagSet) -> Self {
        let mut tags: SmallVec<[Tag; 8]> = tags.into_iter().cloned().collect();
        tags.sort_unstable();
        tags.dedup();
        Self(tags)
    }
}

impl Telemetry {
    pub fn new(builder: &MetricsBuilder, origin_telemetry_enabled: bool) -> Self {
        Self {
            active_contexts: builder.register_debug_gauge("aggregate_active_contexts"),
            active_contexts_by_type: MetricTypedGauge::new(builder, "aggregate_active_contexts_by_type"),
            active_contexts_bytes_by_type: MetricTypedGauge::new(builder, "aggregate_active_contexts_bytes_by_type"),
            events_dropped: builder.register_counter_with_tags("component_events_dropped_total", ["intentional:true"]),
            flushes: builder.register_counter("aggregate_flushes_total"),
            series_flushed: builder.register_counter_with_tags("aggregate_flushed_total", ["data_type:series"]),
            sketches_flushed: builder.register_counter_with_tags("aggregate_flushed_total", ["data_type:sketches"]),
            last_flush_series: builder.register_gauge_with_tags("aggregate_last_flush_count", ["data_type:series"]),
            last_flush_sketches: builder.register_gauge_with_tags("aggregate_last_flush_count", ["data_type:sketches"]),
            last_flush_duration: builder.register_gauge("aggregate_last_flush_duration_nanoseconds"),
            origin_contexts: origin_telemetry_enabled.then(|| OriginContextTelemetry {
                builder: builder.clone(),
                contexts: Arc::default(),
            }),
        }
    }

    #[cfg(test)]
    pub fn noop() -> Self {
        Self {
            active_contexts: Gauge::noop(),
            active_contexts_by_type: MetricTypedGauge::noop(),
            active_contexts_bytes_by_type: MetricTypedGauge::noop(),
            events_dropped: Counter::noop(),
            flushes: Counter::noop(),
            series_flushed: Counter::noop(),
            sketches_flushed: Counter::noop(),
            last_flush_series: Gauge::noop(),
            last_flush_sketches: Gauge::noop(),
            last_flush_duration: Gauge::noop(),
            origin_contexts: None,
        }
    }

    pub fn increment_contexts(&self, context: &Context, values: &MetricValues) {
        self.active_contexts.increment(1);
        self.active_contexts_by_type.for_values(values).increment(1);
        self.active_contexts_bytes_by_type
            .for_values(values)
            .increment(context.size_of() as f64);
        self.update_origin_contexts(context.origin_tags(), true);
    }

    pub fn decrement_contexts(&self, context: &Context, values: &MetricValues) {
        self.active_contexts.decrement(1);
        self.active_contexts_by_type.for_values(values).decrement(1);
        self.active_contexts_bytes_by_type
            .for_values(values)
            .decrement(context.size_of() as f64);
        self.update_origin_contexts(context.origin_tags(), false);
    }

    fn update_origin_contexts(&self, origin_tags: &TagSet, increment: bool) {
        let Some(origin_contexts) = &self.origin_contexts else {
            return;
        };
        if origin_tags.is_empty() {
            return;
        }

        let key = OriginTagSetKey::new(origin_tags);
        let mut contexts = origin_contexts.contexts.lock().unwrap();
        if increment {
            let entry = contexts.entry(key.clone()).or_insert_with(|| {
                let tags = encode_origin_tags(&key.0);
                OriginContextGauge {
                    gauge: origin_contexts
                        .builder
                        .register_gauge_with_tags("aggregate_active_contexts_by_origin", tags),
                    count: 0,
                }
            });
            entry.count += 1;
            entry.gauge.set(entry.count as f64);
        } else if let Some(entry) = contexts.get_mut(&key) {
            entry.count = entry.count.saturating_sub(1);
            entry.gauge.set(entry.count as f64);
            if entry.count == 0 {
                contexts.remove(&key);
            }
        }
    }

    pub fn increment_events_dropped(&self) {
        self.events_dropped.increment(1);
    }

    pub fn increment_flushes(&self) {
        self.flushes.increment(1);
    }

    pub fn increment_flushed(&self, values: &MetricValues, counts: &mut FlushCounts) {
        if values.is_serie() {
            self.series_flushed.increment(1);
            counts.series += 1;
        } else if values.is_sketch() {
            self.sketches_flushed.increment(1);
            counts.sketches += 1;
        }
    }

    pub fn record_last_flush_counts(&self, counts: &FlushCounts) {
        self.last_flush_series.set(counts.series as f64);
        self.last_flush_sketches.set(counts.sketches as f64);
    }

    pub fn record_last_flush_duration(&self, duration: Duration) {
        self.last_flush_duration.set(duration.as_nanos() as f64);
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::tags::TagSet;

    use super::encode_origin_tags;

    #[test]
    fn multivalued_origin_tags_have_sorted_unique_label_names() {
        let mut tags = TagSet::default();
        tags.insert_tag("team.name:platform\"ops\nline");
        tags.insert_tag("team_name:owner");
        tags.insert_tag("kube_service:worker");
        tags.insert_tag("kube_service:api");

        let encoded_tags = encode_origin_tags(&tags);
        let encoded: Vec<_> = encoded_tags.iter().map(String::as_str).collect();
        assert_eq!(
            encoded,
            vec![
                "origin_tag_6B7562655F73657276696365_0:api",
                "origin_tag_6B7562655F73657276696365_1:worker",
                "origin_tag_7465616D2E6E616D65:platform%22ops%0Aline",
                "team_name:owner",
            ]
        );

        let mut reversed_tags = TagSet::default();
        reversed_tags.insert_tag("kube_service:api");
        reversed_tags.insert_tag("kube_service:worker");
        reversed_tags.insert_tag("team.name:platform\"ops\nline");
        reversed_tags.insert_tag("team_name:owner");
        let reversed_encoded = encode_origin_tags(&reversed_tags);
        let reversed_encoded: Vec<_> = reversed_encoded.iter().map(String::as_str).collect();
        assert_eq!(reversed_encoded, encoded);
    }
}
