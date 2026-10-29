use std::{
    collections::BTreeMap,
    fmt::Write as _,
    sync::{Arc, Mutex},
    time::Duration,
};

use hashbrown::{Equivalent, HashMap};
use metrics::{Counter, Gauge};
use saluki_core::data_model::{
    event::metric::{context::Context, MetricValues},
    tags::TagSet,
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
    contexts: Arc<Mutex<OriginContexts>>,
}

// Reserve one series for overflow. Slots are not reused while they have live contexts.
const MAX_ORIGIN_SERIES: usize = 200;
const MAX_ORIGIN_TAGS: usize = 32;
const MAX_ORIGIN_TAG_BYTES: usize = 4096;
const ORIGIN_METRIC: &str = "aggregate_active_contexts_by_origin";
const OVERFLOW_LABEL: &str = "origin_overflow";

// A context remembers its original admission so overflow decrements cannot affect a newly admitted group.
#[derive(Clone, Copy, Default)]
pub(super) struct OriginContextId(u16);

#[derive(Default)]
struct OriginContexts {
    indices: HashMap<Arc<OriginTagSetKey>, usize, foldhash::quality::RandomState>,
    slots: Vec<Option<OriginContextGauge>>,
    overflow: Option<(Gauge, usize)>,
}

struct OriginContextGauge {
    key: Arc<OriginTagSetKey>,
    gauge: Gauge,
    count: usize,
}

#[derive(Eq, Hash, PartialEq)]
struct OriginTagSetKey(Box<[Box<str>]>);

#[derive(Hash)]
struct OriginTagSetRef<'a>(SmallVec<[&'a str; MAX_ORIGIN_TAGS]>);

impl Equivalent<Arc<OriginTagSetKey>> for OriginTagSetRef<'_> {
    fn equivalent(&self, key: &Arc<OriginTagSetKey>) -> bool {
        self.0.iter().copied().eq(key.0.iter().map(AsRef::as_ref))
    }
}

impl<'a> OriginTagSetRef<'a> {
    fn new(tags: &'a TagSet) -> Option<Self> {
        let mut canonical = SmallVec::new();
        let mut bytes = 0usize;
        for tag in tags {
            // An empty tag has no label representation, so it must not distinguish origins either.
            if tag.as_str().is_empty() {
                continue;
            }
            bytes = bytes.checked_add(tag.as_str().len())?;
            if canonical.len() == MAX_ORIGIN_TAGS || bytes > MAX_ORIGIN_TAG_BYTES {
                return None;
            }
            canonical.push(tag.as_str());
        }
        canonical.sort_unstable();
        canonical.dedup();
        Some(Self(canonical))
    }

    fn to_owned(&self) -> Arc<OriginTagSetKey> {
        Arc::new(OriginTagSetKey(
            self.0.iter().map(|tag| Box::<str>::from(*tag)).collect(),
        ))
    }
}

// Budget canonical strings, encoded label strings (up to twice the source bytes plus reserved names),
// and their retained registry/resolver/exposition copies. Allow eight copies and 256 bytes of metadata
// per label for label arrays, string headers, hash-table slack, handles, and allocation headers.
// The per-context admission ID is already included in AggregatedMetric's size.
pub(super) const ORIGIN_TELEMETRY_BYTES: usize = MAX_ORIGIN_SERIES
    * (MAX_ORIGIN_TAG_BYTES + 8 * (2 * MAX_ORIGIN_TAG_BYTES + MAX_ORIGIN_TAGS * 256))
    + size_of::<OriginContexts>();

const BARE_TAGS_LABEL: &str = "origin_bare_tags";
const ESCAPED_LABEL_PREFIX: &str = "U__";

// Each key becomes one label whose value is its sorted values joined by commas. Bare and empty-valued tags have no
// usable label value, so they are listed together under a reserved label instead.
fn encode_origin_tags<'a, I>(tags: I) -> SmallVec<[String; 8]>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut values_by_key: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut bare_tags = Vec::new();
    for tag in tags {
        match tag.split_once(':') {
            Some((key, value)) if !value.is_empty() => values_by_key.entry(key).or_default().push(value),
            _ => bare_tags.push(tag),
        }
    }

    let mut encoded_tags = SmallVec::new();
    if !bare_tags.is_empty() {
        encoded_tags.push(format!("{BARE_TAGS_LABEL}:{}", join_label_values(bare_tags)));
    }
    for (key, values) in values_by_key {
        encoded_tags.push(format!("{}:{}", encode_origin_label_key(key), join_label_values(values)));
    }
    encoded_tags
}

fn join_label_values(mut values: Vec<&str>) -> String {
    values.sort_unstable();
    values.dedup();
    let mut joined = String::new();
    for (index, value) in values.into_iter().enumerate() {
        if index > 0 {
            joined.push(',');
        }
        for c in value.chars() {
            if matches!(c, ',' | '\\') {
                joined.push('\\');
            }
            joined.push(c);
        }
    }
    joined
}

// Keys that aren't plain label names, or that clash with labels we add, use Prometheus's reversible `U__` escaping.
fn encode_origin_label_key(key: &str) -> String {
    let mut chars = key.chars();
    let plain = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !key.starts_with("__")
        && !key.starts_with(ESCAPED_LABEL_PREFIX)
        && !matches!(
            key,
            "component_id" | "component_type" | "emitter" | OVERFLOW_LABEL | BARE_TAGS_LABEL
        );
    if plain {
        return key.to_owned();
    }

    let mut escaped = String::from(ESCAPED_LABEL_PREFIX);
    for c in key.chars() {
        match c {
            '_' => escaped.push_str("__"),
            c if c.is_ascii_alphanumeric() => escaped.push(c),
            c => {
                let _ = write!(escaped, "_{:x}_", u32::from(c));
            }
        }
    }
    escaped
}

impl OriginContexts {
    fn increment(&mut self, builder: &MetricsBuilder, tags: &TagSet) -> OriginContextId {
        let Some(key) = OriginTagSetRef::new(tags) else {
            return self.increment_overflow(builder);
        };
        if key.0.is_empty() {
            return OriginContextId::default();
        }
        if let Some(&index) = self.indices.get(&key) {
            let entry = self.slots[index].as_mut().expect("occupied origin slot");
            entry.count += 1;
            entry.gauge.set(entry.count as f64);
            return OriginContextId(index as u16 + 2);
        }
        let index = match self.slots.iter().position(Option::is_none) {
            Some(index) => index,
            None if self.slots.len() < MAX_ORIGIN_SERIES - 1 => {
                self.slots.push(None);
                self.slots.len() - 1
            }
            None => return self.increment_overflow(builder),
        };
        let labels = encode_origin_tags(key.0.iter().copied());
        let gauge = builder.register_gauge_with_tags(ORIGIN_METRIC, labels);
        gauge.set(1.0);
        let key = key.to_owned();
        self.indices.insert(Arc::clone(&key), index);
        self.slots[index] = Some(OriginContextGauge { key, gauge, count: 1 });
        OriginContextId(index as u16 + 2)
    }

    fn increment_overflow(&mut self, builder: &MetricsBuilder) -> OriginContextId {
        let (gauge, count) = self
            .overflow
            .get_or_insert_with(|| (builder.register_gauge_with_tags(ORIGIN_METRIC, ["origin_overflow:true"]), 0));
        *count += 1;
        gauge.set(*count as f64);
        OriginContextId(1)
    }

    fn decrement(&mut self, id: OriginContextId) {
        match id.0 {
            0 => {}
            1 => {
                let (gauge, count) = self.overflow.as_mut().expect("overflow was admitted");
                *count -= 1;
                gauge.set(*count as f64);
                if *count == 0 {
                    self.overflow = None;
                }
            }
            slot => {
                let index = usize::from(slot - 2);
                let entry = self.slots[index].as_mut().expect("origin was admitted");
                entry.count -= 1;
                entry.gauge.set(entry.count as f64);
                if entry.count == 0 {
                    // Dropping the last handle lets idle eviction retire the series once its final zero is flushed.
                    let entry = self.slots[index].take().expect("occupied origin slot");
                    self.indices.remove(&entry.key);
                }
            }
        }
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

    pub fn increment_contexts(&self, context: &Context, values: &MetricValues) -> OriginContextId {
        self.active_contexts.increment(1);
        self.active_contexts_by_type.for_values(values).increment(1);
        self.active_contexts_bytes_by_type
            .for_values(values)
            .increment(context.size_of() as f64);
        match &self.origin_contexts {
            Some(origin) if !context.origin_tags().is_empty() => origin
                .contexts
                .lock()
                .unwrap()
                .increment(&origin.builder, context.origin_tags()),
            _ => OriginContextId::default(),
        }
    }

    pub fn decrement_contexts(&self, context: &Context, values: &MetricValues, origin_id: OriginContextId) {
        self.active_contexts.decrement(1);
        self.active_contexts_by_type.for_values(values).decrement(1);
        self.active_contexts_bytes_by_type
            .for_values(values)
            .decrement(context.size_of() as f64);
        if let Some(origin) = &self.origin_contexts {
            origin.contexts.lock().unwrap().decrement(origin_id);
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
    use saluki_metrics::test::TestRecorder;

    use super::*;

    fn tag_set(tags: &[&str]) -> TagSet {
        let mut result = TagSet::default();
        for tag in tags {
            result.insert_tag(tag.to_string());
        }
        result
    }

    fn unescape_label_key(name: &str) -> String {
        let Some(escaped) = name.strip_prefix(ESCAPED_LABEL_PREFIX) else {
            return name.to_owned();
        };
        let mut key = String::new();
        let mut chars = escaped.chars();
        while let Some(c) = chars.next() {
            if c != '_' {
                key.push(c);
                continue;
            }
            let code: String = chars.by_ref().take_while(|c| *c != '_').collect();
            if code.is_empty() {
                key.push('_');
            } else {
                key.push(char::from_u32(u32::from_str_radix(&code, 16).unwrap()).unwrap());
            }
        }
        key
    }

    fn split_label_values(joined: &str) -> Vec<String> {
        let mut values = vec![String::new()];
        let mut chars = joined.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => values.last_mut().unwrap().push(chars.next().unwrap()),
                ',' => values.push(String::new()),
                c => values.last_mut().unwrap().push(c),
            }
        }
        values
    }

    // Inverts `encode_origin_tags`, returning the sorted canonical tags.
    fn decode_origin_labels(encoded: &[String]) -> Vec<String> {
        let mut tags = Vec::new();
        for label in encoded {
            let (name, value) = label.split_once(':').unwrap();
            let values = split_label_values(value);
            if name == BARE_TAGS_LABEL {
                tags.extend(values);
            } else {
                let key = unescape_label_key(name);
                tags.extend(values.into_iter().map(|value| format!("{key}:{value}")));
            }
        }
        tags.sort_unstable();
        tags
    }

    fn canonical_labels(tags: &TagSet) -> (Vec<String>, SmallVec<[String; 8]>) {
        let key = OriginTagSetRef::new(tags).expect("within origin limits");
        let canonical = key.0.iter().map(|tag| tag.to_string()).collect();
        (canonical, encode_origin_tags(key.0.iter().copied()))
    }

    #[test]
    fn bare_empty_reserved_and_multivalued_tags_have_distinct_labels() {
        let tags = tag_set(&[
            "",
            "bare",
            "bare:",
            "kube_service:a",
            "kube_service:b",
            "kube_service_0:literal",
            "origin_overflow:true",
            "origin_bare_tags:user",
            "component_id:user",
            "component_type:user",
            "emitter:user",
            "U__escaped:user",
            "team.name:value",
            "availability-zone:us-east-1a",
        ]);
        let (canonical, encoded) = canonical_labels(&tags);
        assert_eq!(decode_origin_labels(&encoded), canonical);
        assert_eq!(
            encoded.as_slice(),
            [
                "origin_bare_tags:bare,bare:",
                "U__U____escaped:user",
                "U__availability_2d_zone:us-east-1a",
                "U__component__id:user",
                "U__component__type:user",
                "U__emitter:user",
                "kube_service:a,b",
                "kube_service_0:literal",
                "U__origin__bare__tags:user",
                "U__origin__overflow:true",
                "U__team_2e_name:value",
            ]
        );
    }

    #[test]
    fn origin_budget_preserves_overflow_routing_when_slots_are_reused() {
        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);
        let builder = MetricsBuilder::default();
        let mut contexts = OriginContexts::default();
        let ids: Vec<_> = (0..MAX_ORIGIN_SERIES - 1)
            .map(|i| contexts.increment(&builder, &tag_set(&[&format!("container:{i}")])))
            .collect();
        let overflow = contexts.increment(&builder, &tag_set(&["container:new"]));
        assert_eq!(overflow.0, 1);
        assert_eq!(contexts.indices.len(), MAX_ORIGIN_SERIES - 1);
        contexts.decrement(ids[0]);
        let admitted = contexts.increment(&builder, &tag_set(&["container:new"]));
        assert_eq!(admitted.0, ids[0].0);
        contexts.decrement(overflow);
        assert!(contexts.overflow.is_none());
        assert_eq!(contexts.slots[0].as_ref().unwrap().count, 1);
        assert_eq!(recorder.gauge((ORIGIN_METRIC, &[("container", "new")])), Some(1.0));
        contexts.decrement(admitted);
        assert!(contexts.slots[0].is_none());
        for id in ids.into_iter().skip(1) {
            contexts.decrement(id);
        }
        assert!(contexts.indices.is_empty());
    }

    #[test]
    fn origin_budget_limits_bytes_and_tags_before_allocating_owned_keys() {
        let builder = MetricsBuilder::default();
        let mut contexts = OriginContexts::default();
        let long = tag_set(&[&"x".repeat(MAX_ORIGIN_TAG_BYTES + 1)]);
        assert!(OriginTagSetRef::new(&long).is_none());
        let id = contexts.increment(&builder, &long);
        assert_eq!(id.0, 1);
        contexts.decrement(id);
        let mut many = TagSet::default();
        for i in 0..=MAX_ORIGIN_TAGS {
            many.insert_tag(format!("key:{i}"));
        }
        assert!(OriginTagSetRef::new(&many).is_none());
        assert_eq!(contexts.increment(&builder, &many).0, 1);
        assert!(contexts.indices.is_empty());
        assert!(OriginTagSetRef::new(&tag_set(&[&"x".repeat(MAX_ORIGIN_TAG_BYTES)])).is_some());
    }

    #[test]
    fn borrowed_origin_lookup_matches_owned_key_without_cloning_tags() {
        let builder = MetricsBuilder::default();
        let mut contexts = OriginContexts::default();
        let first = contexts.increment(&builder, &tag_set(&["b:2", "a:1"]));
        let second = contexts.increment(&builder, &tag_set(&["a:1", "b:2"]));
        assert_eq!(first.0, second.0);
        assert_eq!(contexts.indices.len(), 1);
        assert_eq!(contexts.slots[0].as_ref().unwrap().count, 2);
        // Empty tags are dropped, leaving no origin to count.
        assert_eq!(contexts.increment(&builder, &tag_set(&[""])).0, 0);
    }

    #[test]
    fn bare_tags_do_not_alias_other_origin_counts() {
        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);
        let builder = MetricsBuilder::default();
        let mut contexts = OriginContexts::default();
        let first = contexts.increment(&builder, &tag_set(&["container:one", "bare"]));
        let second = contexts.increment(&builder, &tag_set(&["container:one"]));
        assert_ne!(first.0, second.0);
        contexts.decrement(first);
        assert_eq!(recorder.gauge((ORIGIN_METRIC, &[("container", "one")])), Some(1.0));
    }

    proptest::proptest! {
        #[test]
        fn property_test_origin_encoding_preserves_identity(tags in proptest::collection::vec("[a-zA-Z0-9_: .,%\\\\\\n\\x00é€\u{10FFFF}-]{0,32}", 0..32)) {
            let tags = tag_set(&tags.iter().map(String::as_str).collect::<Vec<_>>());
            let (canonical, encoded) = canonical_labels(&tags);
            proptest::prop_assert_eq!(decode_origin_labels(&encoded), canonical);
            let names: std::collections::HashSet<_> = encoded.iter().map(|tag| tag.split_once(':').unwrap().0).collect();
            proptest::prop_assert_eq!(names.len(), encoded.len());
            for tag in &encoded {
                let (name, value) = tag.split_once(':').unwrap();
                proptest::prop_assert!(!value.is_empty());
                proptest::prop_assert!(!name.is_empty() && !name.starts_with("__"));
                proptest::prop_assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
                proptest::prop_assert!(!name.as_bytes()[0].is_ascii_digit());
            }
        }
    }

    #[test]
    fn multivalued_origin_tags_share_one_label_in_sorted_order() {
        let mut tags = TagSet::default();
        tags.insert_tag("team.name:platform\"ops\nline");
        tags.insert_tag("team_name:owner");
        tags.insert_tag("kube_service:worker");
        tags.insert_tag("kube_service:api");

        let encoded_tags = encode_origin_tags((&tags).into_iter().map(|tag| tag.as_str()));
        let encoded: Vec<_> = encoded_tags.iter().map(String::as_str).collect();
        assert_eq!(
            encoded,
            vec![
                "kube_service:api,worker",
                "U__team_2e_name:platform\"ops\nline",
                "team_name:owner",
            ]
        );

        let mut reversed_tags = TagSet::default();
        reversed_tags.insert_tag("kube_service:api");
        reversed_tags.insert_tag("kube_service:worker");
        reversed_tags.insert_tag("team.name:platform\"ops\nline");
        reversed_tags.insert_tag("team_name:owner");
        let reversed_encoded = encode_origin_tags((&reversed_tags).into_iter().map(|tag| tag.as_str()));
        let reversed_encoded: Vec<_> = reversed_encoded.iter().map(String::as_str).collect();
        assert_eq!(reversed_encoded, encoded);
    }
}
