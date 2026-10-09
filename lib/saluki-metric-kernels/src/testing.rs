//! Property-testing strategies for metrics.
//!
//! The strategies favor the edge cases that metric semantics hinge on, rather than uniformly random values:
//!
//! - names that are empty, longer than the intake's 350-byte limit, without any ASCII letter, or with multi-byte UTF-8,
//!   alongside ordinary names and names that need normalizing;
//! - an unset host as well as an empty one;
//! - bare tags, tags with an empty value, values containing a colon, and repeated tags, in both tags and origin tags;
//! - every metric kind, with and without timestamps, and rates with zero and non-zero intervals.
//!
//! Names, tag keys, and tag values are drawn largely from small vocabularies, which strategies for parameters (name
//! lists, prefixes, tag rules) share, so that generated parameters match generated metrics often enough to be
//! interesting.

use std::time::Duration;

use proptest::{collection::vec, prelude::*, sample::select};
use saluki_core::data_model::{
    event::metric::{
        context::Context, Histogram, HistogramPoints, Metric, MetricValues, SampleRate, ScalarPoints, SetPoints,
        SketchPoints,
    },
    tags::{Tag, TagSet},
};
use stringtheory::MetaString;

/// Metric names that exercise normalization, shared between metric and parameter strategies.
pub const NAME_VOCABULARY: &[&str] = &[
    "app.requests",
    "app.errors",
    "app.requests.count",
    "my metric-name",
    "my_metric_name",
    "1app.requests",
    "café.requests",
    "caf.requests",
    "🍣.metric",
    "demo.debug.trace",
    "",
    "123",
    "...",
    "_",
];

/// Prefixes that overlap with [`NAME_VOCABULARY`], including the empty prefix.
pub const PREFIX_VOCABULARY: &[&str] = &["", "app.", "app.requests", "my", "my_", "caf", "demo.", "1", "🍣"];

/// Tag keys used by tag and parameter strategies, including the empty key.
pub const TAG_KEY_VOCABULARY: &[&str] = &["env", "team", "canary", "region", ""];

/// Tag values used by tag and parameter strategies, including the empty value and a value containing a colon.
pub const TAG_VALUE_VOCABULARY: &[&str] = &["prod", "staging", "", "us:east", "platform"];

/// Hosts used by metric and parameter strategies, including the empty host.
pub const HOST_VOCABULARY: &[&str] = &["web-1", "web-2", "db", ""];

/// Returns a strategy for metric names.
pub fn arb_metric_name() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => select(NAME_VOCABULARY).prop_map(str::to_string),
        // Already normalized.
        2 => "[a-z][a-z0-9_]{0,6}(\\.[a-z0-9_]{1,6}){0,3}",
        // Printable ASCII, most of which needs normalizing.
        2 => "[ -~]{0,16}",
        // No ASCII letter, so no intake name.
        1 => "[0-9._ -]{1,8}",
        // Multi-byte UTF-8.
        1 => "[a-zé🍣ü. _]{1,10}",
        // Around the 350-byte limit.
        1 => "[a-z]{345,355}",
    ]
}

/// Returns a strategy for one tag, as a string.
pub fn arb_tag() -> impl Strategy<Value = String> {
    let key = || select(TAG_KEY_VOCABULARY);
    prop_oneof![
        // A bare tag.
        key().prop_map(str::to_string),
        // A key and a value, which may be empty or contain a colon.
        (key(), select(TAG_VALUE_VOCABULARY)).prop_map(|(key, value)| format!("{}:{}", key, value)),
        // Anything short.
        "[a-z:]{0,6}",
    ]
}

/// Returns a strategy for a list of tags, which may repeat a tag.
pub fn arb_tags() -> impl Strategy<Value = Vec<String>> {
    (vec(arb_tag(), 0..5), any::<bool>()).prop_map(|(mut tags, repeat)| {
        if repeat {
            if let Some(first) = tags.first().cloned() {
                tags.push(first);
            }
        }
        tags
    })
}

/// Returns a strategy for a host: unset, empty, or a name.
pub fn arb_host() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        1 => Just(None),
        2 => select(HOST_VOCABULARY).prop_map(|host| Some(host.to_string())),
    ]
}

/// Returns a strategy for an optional timestamp, in seconds since the Unix epoch. `0` means untimestamped.
fn arb_timestamp() -> impl Strategy<Value = u64> {
    prop_oneof![Just(0u64), 1_600_000_000u64..1_800_000_000u64]
}

fn arb_scalar_points() -> impl Strategy<Value = ScalarPoints> {
    vec((arb_timestamp(), -1e6f64..1e6f64), 1..4).prop_map(|points| ScalarPoints::from(&points[..]))
}

/// Returns a strategy for metric values of every kind.
pub fn arb_metric_values() -> impl Strategy<Value = MetricValues> {
    let interval = select(vec![0u64, 1, 10, 15, 60]).prop_map(Duration::from_secs);
    prop_oneof![
        arb_scalar_points().prop_map(MetricValues::Counter),
        (arb_scalar_points(), interval).prop_map(|(points, interval)| MetricValues::Rate(points, interval)),
        arb_scalar_points().prop_map(MetricValues::Gauge),
        vec((arb_timestamp(), select(TAG_VALUE_VOCABULARY)), 1..4).prop_map(|members| {
            let mut points = SetPoints::from((members[0].0, members[0].1));
            for (timestamp, member) in &members[1..] {
                points.merge(SetPoints::from((*timestamp, *member)));
            }
            MetricValues::Set(points)
        }),
        (
            vec((-1e6f64..1e6f64, select(vec![1.0, 0.5, 0.1])), 1..4),
            arb_timestamp()
        )
            .prop_map(|(samples, timestamp)| {
                let mut histogram = Histogram::default();
                for (value, rate) in samples {
                    histogram.insert(value, SampleRate::try_from(rate).unwrap_or(SampleRate::unsampled()));
                }
                let mut values = MetricValues::Histogram(HistogramPoints::from(histogram));
                values.set_timestamp(timestamp);
                values
            }),
        (vec(-1e6f64..1e6f64, 1..4), arb_timestamp()).prop_map(|(samples, timestamp)| {
            let mut values = MetricValues::Distribution(SketchPoints::from(&samples[..]));
            values.set_timestamp(timestamp);
            values
        }),
    ]
}

/// Returns a strategy for metrics.
pub fn arb_metric() -> impl Strategy<Value = Metric> {
    (
        arb_metric_name(),
        arb_host(),
        arb_tags(),
        arb_tags(),
        arb_metric_values(),
    )
        .prop_map(|(name, host, tags, origin_tags, values)| {
            let tags = tags.into_iter().map(Tag::from).collect::<TagSet>();
            let origin_tags = origin_tags.into_iter().map(Tag::from).collect::<TagSet>();
            let context = Context::from_parts(name, tags)
                .with_origin_tags(origin_tags)
                .with_host(host.map(MetaString::from));
            Metric::from_parts(context, values, Default::default())
        })
}
