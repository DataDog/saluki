//! Tests of the aggregation state machine and its core: the native tests that moved here with the state machine, tests
//! of the decisions the core took over from the native run loop, and differential tests against the frozen
//! pre-extraction state machine.

use std::{collections::BTreeSet, num::NonZeroU64, time::Duration};

use float_cmp::ApproxEqRatio as _;
use proptest::{collection::vec, prelude::*};
use saluki_core::{
    components::ComponentContext,
    data_model::{
        event::{
            metric::{context::Context, *},
            Event,
        },
        tags::{Tag, TagSet},
    },
    topology::{interconnect::Dispatcher, EventsBuffer, OutputName},
};
use saluki_metrics::{test::TestRecorder, MetricsBuilder};
use stringtheory::MetaString;
use tokio::sync::mpsc;

use super::{
    reference,
    state::{align_to_bucket_start, is_bucket_closed, AggregationState, ContextLimit, FlushCursor, Insertion},
    *,
};

/// The state machine together with the parameters the native state machine was created with, which the native tests
/// below were written against: it passes them to every call that takes them.
struct TestState {
    state: AggregationState,
    context_limit: usize,
    counter_expiry: Option<NonZeroU64>,
    hist_config: HistogramConfiguration,
}

impl TestState {
    fn new(
        bucket_width_secs: NonZeroU64, context_limit: usize, counter_expiry: Option<NonZeroU64>,
        hist_config: HistogramConfiguration, telemetry: AggregateTelemetry,
    ) -> Self {
        Self {
            state: AggregationState::new(bucket_width_secs, telemetry),
            context_limit,
            counter_expiry,
            hist_config,
        }
    }

    fn insert(&mut self, timestamp: u64, metric: Metric) -> bool {
        let limit = ContextLimit::exclusive(self.context_limit);
        matches!(self.state.insert(timestamp, metric, limit), Insertion::Inserted { .. })
    }

    fn start_flush(&self, current_time: u64, flush_open_buckets: bool) -> FlushCursor {
        self.state
            .start_flush(current_time, flush_open_buckets, self.counter_expiry)
    }

    fn flush_next<F: FnMut(Metric)>(&mut self, cursor: &mut FlushCursor, sink: F) -> bool {
        self.state.flush_next(cursor, &self.hist_config, None, None, sink)
    }

    fn finish_flush(&mut self, cursor: FlushCursor) {
        self.state
            .finish_flush(cursor, ContextLimit::exclusive(self.context_limit));
    }
}

impl std::ops::Deref for TestState {
    type Target = AggregationState;

    fn deref(&self) -> &AggregationState {
        &self.state
    }
}

const BUCKET_WIDTH_SECS: NonZeroU64 = NonZeroU64::new(10).expect("not zero");
const BUCKET_WIDTH: Duration = Duration::from_secs(BUCKET_WIDTH_SECS.get());
const COUNTER_EXPIRE_SECS: u64 = 20;
const COUNTER_EXPIRE: Option<NonZeroU64> = NonZeroU64::new(COUNTER_EXPIRE_SECS);

/// Gets the bucket start timestamp for the given step.
const fn bucket_ts(step: u64) -> u64 {
    align_to_bucket_start(insert_ts(step), BUCKET_WIDTH_SECS)
}

/// Gets the insert timestamp for the given step.
const fn insert_ts(step: u64) -> u64 {
    (BUCKET_WIDTH_SECS.get() * (step + 1)) - 2
}

/// Gets the flush timestamp for the given step.
const fn flush_ts(step: u64) -> u64 {
    BUCKET_WIDTH_SECS.get() * (step + 1)
}

/// Flushes the state at `timestamp`, counting open buckets as closed for idle-counter zeros, and returns what it
/// emitted, sorted by name.
fn get_flushed_metrics(timestamp: u64, state: &mut TestState) -> Vec<Metric> {
    let mut cursor = state.start_flush(timestamp, true);
    let mut metrics = Vec::new();
    while state.flush_next(&mut cursor, |metric| metrics.push(metric)) {}
    state.finish_flush(cursor);

    metrics.sort_by(|a, b| a.context().name().cmp(b.context().name()));
    metrics
}

macro_rules! compare_points {
    (scalar, $expected:expr, $actual:expr, $error_ratio:literal) => {
        for (idx, (expected_value, actual_value)) in $expected.into_iter().zip($actual.into_iter()).enumerate() {
            let (expected_ts, expected_point) = expected_value;
            let (actual_ts, actual_point) = actual_value;

            assert_eq!(
                expected_ts, actual_ts,
                "timestamp for value #{} does not match: {:?} (expected) vs {:?} (actual)",
                idx, expected_ts, actual_ts
            );
            assert!(
                expected_point.approx_eq_ratio(&actual_point, $error_ratio),
                "point for value #{} does not match: {} (expected) vs {} (actual)",
                idx,
                expected_point,
                actual_point
            );
        }
    };
    (distribution, $expected:expr, $actual:expr) => {
        for (idx, (expected_value, actual_value)) in $expected.into_iter().zip($actual.into_iter()).enumerate() {
            let (expected_ts, expected_sketch) = expected_value;
            let (actual_ts, actual_sketch) = actual_value;

            assert_eq!(
                expected_ts, actual_ts,
                "timestamp for value #{} does not match: {:?} (expected) vs {:?} (actual)",
                idx, expected_ts, actual_ts
            );
            assert_eq!(
                expected_sketch, actual_sketch,
                "sketch for value #{} does not match: {:?} (expected) vs {:?} (actual)",
                idx, expected_sketch, actual_sketch
            );
        }
    };
}

macro_rules! assert_flushed_scalar_metric {
    ($original:expr, $actual:expr, [$($ts:expr => $value:expr),+]) => {
        assert_flushed_scalar_metric!($original, $actual, [$($ts => $value),+], error_ratio => 0.000001);
    };
    ($original:expr, $actual:expr, [$($ts:expr => $value:expr),+], error_ratio => $error_ratio:literal) => {
        let actual_metric = $actual;

        assert_eq!($original.context(), actual_metric.context(), "expected context ({}) and actual context ({}) do not match", $original.context(), actual_metric.context());

        let expected_points = ScalarPoints::from([$(($ts, $value)),+]);

        match actual_metric.values() {
            MetricValues::Counter(ref actual_points) | MetricValues::Gauge(ref actual_points) | MetricValues::Rate(ref actual_points, _) => {
                assert_eq!(expected_points.len(), actual_points.len(), "expected and actual values have different number of points");
                compare_points!(scalar, expected_points, actual_points, $error_ratio);
            },
            _ => panic!("only counters, rates, and gauges are supported in assert_flushed_scalar_metric"),
        }
    };
}

macro_rules! assert_flushed_distribution_metric {
    ($original:expr, $actual:expr, [$($ts:expr => $value:expr),+]) => {
        assert_flushed_distribution_metric!($original, $actual, [$($ts => $value),+], error_ratio => 0.000001);
    };
    ($original:expr, $actual:expr, [$($ts:expr => $value:expr),+], error_ratio => $error_ratio:literal) => {
        let actual_metric = $actual;

        assert_eq!($original.context(), actual_metric.context());

        match actual_metric.values() {
            MetricValues::Distribution(ref actual_points) => {
                let expected_points = SketchPoints::from([$(($ts, $value)),+]);
                assert_eq!(expected_points.len(), actual_points.len(), "expected and actual values have different number of points");

                compare_points!(distribution, &expected_points, actual_points);
            },
            _ => panic!("only distributions are supported in assert_flushed_distribution_metric"),
        }
    };
}

// Native tests of the state machine, moved here with it.

#[test]
fn aggregate_metric_type_matches_every_metric_shape() {
    let cases = [
        (MetricValues::counter(1.0), AggregateMetricType::Counter),
        (
            MetricValues::rate(1.0, Duration::from_secs(10)),
            AggregateMetricType::Rate,
        ),
        (MetricValues::gauge(1.0), AggregateMetricType::Gauge),
        (MetricValues::set("value"), AggregateMetricType::Set),
        (MetricValues::histogram([1.0]), AggregateMetricType::Histogram),
        (
            MetricValues::distribution(&[1.0][..]),
            AggregateMetricType::Distribution,
        ),
    ];

    for (values, expected) in cases {
        assert_eq!(AggregateMetricType::from(&values), expected);
    }
}

#[test]
fn snapshot_contexts_preserves_full_context_shape_and_unit() {
    let mut state = TestState::new(
        BUCKET_WIDTH_SECS,
        10,
        COUNTER_EXPIRE,
        HistogramConfiguration::default(),
        AggregateTelemetry::noop(),
    );

    let histogram_context = Context::from_static_parts("request.duration", &["env:prod"])
        .with_host(Some(MetaString::from_static("host-a")))
        .with_origin_tags(TagSet::from(Tag::from_static("container:one")));
    let gauge_context = Context::from_static_parts("request.duration", &["env:prod"])
        .with_host(Some(MetaString::from_static("host-b")))
        .with_origin_tags(TagSet::from(Tag::from_static("container:two")));
    let histogram = Metric::from_parts(
        histogram_context.clone(),
        MetricValues::histogram([12.0]),
        MetricMetadata::default().with_unit(MetaString::from_static("millisecond")),
    );
    let gauge = Metric::gauge(gauge_context.clone(), 2.0);

    assert!(state.insert(insert_ts(1), histogram));
    assert!(state.insert(insert_ts(1), gauge));

    let mut snapshot = state.snapshot_contexts();
    assert_eq!(snapshot.len(), 2);
    assert!(snapshot.capacity() >= state.len());
    snapshot.sort_by(|a, b| a.context().host().cmp(&b.context().host()));

    assert_eq!(snapshot[0].context(), &histogram_context);
    assert_eq!(snapshot[0].context().tags().len(), 1);
    assert_eq!(snapshot[0].context().origin_tags().len(), 1);
    assert_eq!(snapshot[0].metric_type(), AggregateMetricType::Histogram);
    assert_eq!(snapshot[0].unit(), Some("millisecond"));

    assert_eq!(snapshot[1].context(), &gauge_context);
    assert_eq!(snapshot[1].metric_type(), AggregateMetricType::Gauge);
    assert_eq!(snapshot[1].unit(), None);
}

#[test]
fn snapshot_contexts_follows_ordinary_context_lifecycle() {
    let mut state = TestState::new(
        BUCKET_WIDTH_SECS,
        10,
        COUNTER_EXPIRE,
        HistogramConfiguration::default(),
        AggregateTelemetry::noop(),
    );
    let context = Context::from_static_name("active.gauge");

    assert!(state.insert(insert_ts(1), Metric::gauge(context.clone(), 1.0)));
    let active_snapshot = state.snapshot_contexts();
    assert_eq!(active_snapshot.len(), 1);
    assert_eq!(active_snapshot[0].context(), &context);
    assert_eq!(active_snapshot[0].metric_type(), AggregateMetricType::Gauge);

    let _ = get_flushed_metrics(flush_ts(1), &mut state);
    assert!(state.snapshot_contexts().is_empty());
}

#[test]
fn snapshot_contexts_retains_idle_counter_until_expiry() {
    let mut state = TestState::new(
        BUCKET_WIDTH_SECS,
        10,
        COUNTER_EXPIRE,
        HistogramConfiguration::default(),
        AggregateTelemetry::noop(),
    );
    let context = Context::from_static_name("sparse.counter");

    assert!(state.insert(insert_ts(1), Metric::counter(context.clone(), 1.0)));
    let _ = get_flushed_metrics(flush_ts(1), &mut state);
    let first_snapshot = state.snapshot_contexts();
    assert_eq!(first_snapshot.len(), 1);
    assert_eq!(first_snapshot[0].context(), &context);
    assert_eq!(first_snapshot[0].metric_type(), AggregateMetricType::Counter);

    let _ = get_flushed_metrics(flush_ts(2), &mut state);
    let second_snapshot = state.snapshot_contexts();
    assert_eq!(second_snapshot.len(), 1);
    assert_eq!(second_snapshot[0].context(), &context);
    assert_eq!(second_snapshot[0].metric_type(), AggregateMetricType::Counter);

    let _ = get_flushed_metrics(flush_ts(3), &mut state);
    assert!(state.snapshot_contexts().is_empty());
}

#[test]
fn bucket_is_closed() {
    // Cases are defined as:
    // (current time, bucket start, bucket width, flush open buckets, expected result)
    let cases = [
        // Bucket goes from [995, 1005), current time of 1000, so bucket is open.
        (1000, 995, BUCKET_WIDTH_SECS, false, false),
        (1000, 995, BUCKET_WIDTH_SECS, true, true),
        // Bucket goes from [1000, 1010), current time of 1000, so bucket is open.
        (1000, 1000, BUCKET_WIDTH_SECS, false, false),
        (1000, 1000, BUCKET_WIDTH_SECS, true, true),
        // Bucket goes from [1000, 1010), current time of 1010, so bucket is closed.
        (1010, 1000, BUCKET_WIDTH_SECS, false, true),
        (1010, 1000, BUCKET_WIDTH_SECS, true, true),
    ];

    for (current_time, bucket_start, bucket_width_secs, flush_open_buckets, expected) in cases {
        let expected_reason = if expected {
            "closed, was open"
        } else {
            "open, was closed"
        };

        assert_eq!(
            is_bucket_closed(current_time, bucket_start, bucket_width_secs, flush_open_buckets),
            expected,
            "expected bucket to be {} (current_time={}, bucket_start={}, bucket_width={}, flush_open_buckets={})",
            expected_reason,
            current_time,
            bucket_start,
            bucket_width_secs,
            flush_open_buckets
        );
    }
}

#[test]
fn context_limit() {
    // Create our aggregation state with a context limit of 2.
    let mut state = TestState::new(
        BUCKET_WIDTH_SECS,
        2,
        COUNTER_EXPIRE,
        HistogramConfiguration::default(),
        AggregateTelemetry::noop(),
    );

    // Create four unique gauges, and insert all of them. The third and fourth should fail because we've reached
    // the context limit.
    let input_metrics = [
        Metric::gauge("metric1", 1.0),
        Metric::gauge("metric2", 2.0),
        Metric::gauge("metric3", 3.0),
        Metric::gauge("metric4", 4.0),
    ];

    assert!(!state.context_limit_breached());

    assert!(state.insert(insert_ts(1), input_metrics[0].clone()));
    assert!(state.insert(insert_ts(1), input_metrics[1].clone()));
    assert!(!state.context_limit_breached());

    assert!(!state.insert(insert_ts(1), input_metrics[2].clone()));
    assert!(state.context_limit_breached());
    assert!(!state.insert(insert_ts(1), input_metrics[3].clone()));
    assert!(state.context_limit_breached());

    // We should only see the first two gauges after flushing. The flush should also clear the breached flag since
    // contexts drop below the limit.
    let flushed_metrics = get_flushed_metrics(flush_ts(1), &mut state);
    assert_eq!(flushed_metrics.len(), 2);
    assert_eq!(input_metrics[0].context(), flushed_metrics[0].context());
    assert_eq!(input_metrics[1].context(), flushed_metrics[1].context());
    assert!(!state.context_limit_breached());

    // We should be able to insert the third and fourth gauges now as the first two have been flushed, and along
    // with them, their contexts should no longer be tracked in the aggregation state:
    assert!(state.insert(insert_ts(2), input_metrics[2].clone()));
    assert!(state.insert(insert_ts(2), input_metrics[3].clone()));

    let flushed_metrics = get_flushed_metrics(flush_ts(2), &mut state);
    assert_eq!(flushed_metrics.len(), 2);
    assert_eq!(input_metrics[2].context(), flushed_metrics[0].context());
    assert_eq!(input_metrics[3].context(), flushed_metrics[1].context());
}

#[test]
fn context_limit_with_zero_value_counters() {
    // We test here to ensure that zero-value counters contribute to the context limit.
    let mut state = TestState::new(
        BUCKET_WIDTH_SECS,
        2,
        COUNTER_EXPIRE,
        HistogramConfiguration::default(),
        AggregateTelemetry::noop(),
    );

    // Create our input metrics.
    let input_metrics = [
        Metric::counter("metric1", 1.0),
        Metric::counter("metric2", 2.0),
        Metric::counter("metric3", 3.0),
    ];

    assert!(state.insert(insert_ts(1), input_metrics[0].clone()));
    assert!(state.insert(insert_ts(1), input_metrics[1].clone()));

    // Flush the aggregation state, and observe they're both present.
    let flushed_metrics = get_flushed_metrics(flush_ts(1), &mut state);
    assert_eq!(flushed_metrics.len(), 2);
    assert_flushed_scalar_metric!(&input_metrics[0], &flushed_metrics[0], [bucket_ts(1) => 1.0]);
    assert_flushed_scalar_metric!(&input_metrics[1], &flushed_metrics[1], [bucket_ts(1) => 2.0]);

    // Flush _again_ to ensure that we then emit zero-value variants for both counters.
    let flushed_metrics = get_flushed_metrics(flush_ts(2), &mut state);
    assert_eq!(flushed_metrics.len(), 2);
    assert_flushed_scalar_metric!(&input_metrics[0], &flushed_metrics[0], [bucket_ts(2) => 0.0]);
    assert_flushed_scalar_metric!(&input_metrics[1], &flushed_metrics[1], [bucket_ts(2) => 0.0]);

    // Now try to insert a third counter, which should fail because we've reached the context limit.
    assert!(!state.insert(insert_ts(3), input_metrics[2].clone()));

    // Flush the aggregation state, and observe that we only see the two original counters.
    let flushed_metrics = get_flushed_metrics(flush_ts(3), &mut state);
    assert_eq!(flushed_metrics.len(), 2);
    assert_flushed_scalar_metric!(&input_metrics[0], &flushed_metrics[0], [bucket_ts(3) => 0.0]);
    assert_flushed_scalar_metric!(&input_metrics[1], &flushed_metrics[1], [bucket_ts(3) => 0.0]);

    // With a fourth flush interval, the two counters should now have expired, and thus be dropped and no longer
    // contributing to the context limit.
    let flushed_metrics = get_flushed_metrics(flush_ts(4), &mut state);
    assert_eq!(flushed_metrics.len(), 0);

    // Now we should be able to insert the third counter, and it should be the only one present after flushing.
    assert!(state.insert(insert_ts(5), input_metrics[2].clone()));

    let flushed_metrics = get_flushed_metrics(flush_ts(5), &mut state);
    assert_eq!(flushed_metrics.len(), 1);
    assert_flushed_scalar_metric!(&input_metrics[2], &flushed_metrics[0], [bucket_ts(5) => 3.0]);
}

#[test]
fn zero_value_counters() {
    // We're testing that we properly emit and expire zero-value counters in all relevant scenarios.
    let mut state = TestState::new(
        BUCKET_WIDTH_SECS,
        10,
        COUNTER_EXPIRE,
        HistogramConfiguration::default(),
        AggregateTelemetry::noop(),
    );

    // Create two unique counters, and insert both of them.
    let input_metrics = [Metric::counter("metric1", 1.0), Metric::counter("metric2", 2.0)];

    assert!(state.insert(insert_ts(1), input_metrics[0].clone()));
    assert!(state.insert(insert_ts(1), input_metrics[1].clone()));

    // Flush the aggregation state, and observe they're both present.
    let flushed_metrics = get_flushed_metrics(flush_ts(1), &mut state);
    assert_eq!(flushed_metrics.len(), 2);
    assert_flushed_scalar_metric!(&input_metrics[0], &flushed_metrics[0], [bucket_ts(1) => 1.0]);
    assert_flushed_scalar_metric!(&input_metrics[1], &flushed_metrics[1], [bucket_ts(1) => 2.0]);

    // Perform our second flush, which should have them as zero-value counters.
    let flushed_metrics = get_flushed_metrics(flush_ts(2), &mut state);
    assert_eq!(flushed_metrics.len(), 2);
    assert_flushed_scalar_metric!(&input_metrics[0], &flushed_metrics[0], [bucket_ts(2) => 0.0]);
    assert_flushed_scalar_metric!(&input_metrics[1], &flushed_metrics[1], [bucket_ts(2) => 0.0]);

    // Now, we'll pretend to skip a flush period and add updates to them again after that.
    assert!(state.insert(insert_ts(4), input_metrics[0].clone()));
    assert!(state.insert(insert_ts(4), input_metrics[1].clone()));

    // Flush the aggregation state, and observe that we have two zero-value counters for the flush period we
    // skipped, but that we see them appear again in the fourth flush period.
    let flushed_metrics = get_flushed_metrics(flush_ts(4), &mut state);
    assert_eq!(flushed_metrics.len(), 2);
    assert_flushed_scalar_metric!(&input_metrics[0], &flushed_metrics[0], [bucket_ts(3) => 0.0, bucket_ts(4) => 1.0]);
    assert_flushed_scalar_metric!(&input_metrics[1], &flushed_metrics[1], [bucket_ts(3) => 0.0, bucket_ts(4) => 2.0]);

    // Now we'll skip multiple flush periods and ensure that we emit zero-value counters up until the point they
    // expire. As our zero-value counter expiration is 20 seconds, this is two flush periods, so we skip by three
    // flush periods, and we should only see the counters emitted for the first two.
    let flushed_metrics = get_flushed_metrics(flush_ts(7), &mut state);
    assert_eq!(flushed_metrics.len(), 2);
    assert_flushed_scalar_metric!(&input_metrics[0], &flushed_metrics[0], [bucket_ts(5) => 0.0, bucket_ts(6) => 0.0]);
    assert_flushed_scalar_metric!(&input_metrics[1], &flushed_metrics[1], [bucket_ts(5) => 0.0, bucket_ts(6) => 0.0]);
}

#[test]
fn merge_identical_timestamped_values_on_flush() {
    // We're testing that we properly emit and expire zero-value counters in all relevant scenarios.
    let mut state = TestState::new(
        BUCKET_WIDTH_SECS,
        10,
        COUNTER_EXPIRE,
        HistogramConfiguration::default(),
        AggregateTelemetry::noop(),
    );

    // Create one multi-value counter, and insert it.
    let input_metric = Metric::counter("metric1", [1.0, 2.0, 3.0, 4.0, 5.0]);

    assert!(state.insert(insert_ts(1), input_metric.clone()));

    // Flush the aggregation state, and observe the metric is present _and_ that we've properly merged all of the
    // values within the same timestamp.
    let flushed_metrics = get_flushed_metrics(flush_ts(1), &mut state);
    assert_eq!(flushed_metrics.len(), 1);
    assert_flushed_scalar_metric!(&input_metric, &flushed_metrics[0], [bucket_ts(1) => 15.0]);
}

#[test]
fn histogram_statistics() {
    // We're testing that we properly emit individual metrics (min, max, sum, etc) for a histogram.
    let hist_config = HistogramConfiguration::from_statistics(
        &[
            HistogramStatistic::Count,
            HistogramStatistic::Sum,
            HistogramStatistic::Percentile {
                q: 0.5,
                suffix: "p50".into(),
            },
        ],
        false,
        "".into(),
    );
    let mut state = TestState::new(
        BUCKET_WIDTH_SECS,
        10,
        COUNTER_EXPIRE,
        hist_config,
        AggregateTelemetry::noop(),
    );

    // Create one multi-value histogram and insert it.
    let input_metric = Metric::histogram("metric1", [1.0, 2.0, 3.0, 4.0, 5.0]);
    assert!(state.insert(insert_ts(1), input_metric.clone()));

    // Flush the aggregation state, and observe that we've emitted all of the configured distribution statistics in
    // the form of three metrics: count, sum, and p50.
    let flushed_metrics = get_flushed_metrics(flush_ts(1), &mut state);
    assert_eq!(flushed_metrics.len(), 3);

    // Create versions of the metric for each of the statistics we're expecting to emit. The values themselves don't
    // matter here, but we do need a `Metric` for it to compare the context to.
    let count_metric = Metric::rate("metric1.count", 0.0, Duration::from_secs(BUCKET_WIDTH_SECS.get()));
    let sum_metric = Metric::gauge("metric1.sum", 0.0);
    let p50_metric = Metric::gauge("metric1.p50", 0.0);

    // We use a less strict error ratio (how much the expected vs actual) for the percentile check, as we generally
    // expect the value to be somewhat off the exact value due to the lossy nature of `DDSketch`.
    assert_flushed_scalar_metric!(count_metric, &flushed_metrics[0], [bucket_ts(1) => 5.0]);
    assert_flushed_scalar_metric!(p50_metric, &flushed_metrics[1], [bucket_ts(1) => 3.0], error_ratio => 0.0025);
    assert_flushed_scalar_metric!(sum_metric, &flushed_metrics[2], [bucket_ts(1) => 15.0]);
}

#[test]
fn histogram_statistics_unit_propagation() {
    // We're testing that the unit from the input histogram metadata propagates to all flushed output metrics.
    let hist_config = HistogramConfiguration::from_statistics(
        &[
            HistogramStatistic::Count,
            HistogramStatistic::Sum,
            HistogramStatistic::Percentile {
                q: 0.5,
                suffix: "p50".into(),
            },
        ],
        false,
        "".into(),
    );
    let mut state = TestState::new(
        BUCKET_WIDTH_SECS,
        10,
        COUNTER_EXPIRE,
        hist_config,
        AggregateTelemetry::noop(),
    );

    // Build a histogram with unit = "millisecond", simulating what arrives from a DogStatsD `ms` metric.
    let context = Context::from_static_parts("metric1", &[]);
    let metadata = MetricMetadata::default().with_unit(MetaString::from_static("millisecond"));
    let input_metric = Metric::from_parts(
        context,
        MetricValues::histogram([1.0_f64, 2.0, 3.0, 4.0, 5.0]),
        metadata,
    );
    assert!(state.insert(insert_ts(1), input_metric));

    let flushed_metrics = get_flushed_metrics(flush_ts(1), &mut state);
    assert_eq!(flushed_metrics.len(), 3);

    // Counts are dimensionless: the `.count` series must drop the unit while all other
    // aggregate series carry the unit from the input histogram.
    for metric in &flushed_metrics {
        let name = metric.context().name();
        if name.ends_with(".count") {
            assert_eq!(
                metric.metadata().unit(),
                None,
                "flushed metric '{}' should be dimensionless",
                name
            );
        } else {
            assert_eq!(
                metric.metadata().unit(),
                Some("millisecond"),
                "flushed metric '{}' should carry unit='millisecond'",
                name
            );
        }
    }
}

#[test]
fn distributions() {
    // We're testing that we pass through distributions untouched.
    let mut state = TestState::new(
        BUCKET_WIDTH_SECS,
        10,
        COUNTER_EXPIRE,
        HistogramConfiguration::default(),
        AggregateTelemetry::noop(),
    );

    // Create one multi-value distribution, with server-side aggregation, and insert it.
    let values = [1.0, 2.0, 3.0, 4.0, 5.0];
    let input_metric = Metric::distribution("metric1", &values[..]);

    assert!(state.insert(insert_ts(1), input_metric.clone()));

    // Flush the aggregation state, and observe that we've emitted the original distribution.
    let flushed_metrics = get_flushed_metrics(flush_ts(1), &mut state);
    assert_eq!(flushed_metrics.len(), 1);

    assert_flushed_distribution_metric!(&input_metric, &flushed_metrics[0], [bucket_ts(1) => &values[..]]);
}

#[test]
fn histogram_copy_to_distribution() {
    let hist_config = HistogramConfiguration::from_statistics(
        &[
            HistogramStatistic::Count,
            HistogramStatistic::Sum,
            HistogramStatistic::Percentile {
                q: 0.5,
                suffix: "p50".into(),
            },
        ],
        true,
        "dist_prefix.".into(),
    );
    let mut state = TestState::new(
        BUCKET_WIDTH_SECS,
        10,
        COUNTER_EXPIRE,
        hist_config,
        AggregateTelemetry::noop(),
    );

    // Create one multi-value histogram and insert it.
    let values = [1.0, 2.0, 3.0, 4.0, 5.0];
    let input_metric = Metric::histogram("metric1", values);
    assert!(state.insert(insert_ts(1), input_metric.clone()));

    // Flush the aggregation state, and observe that we've emitted all of the configured distribution statistics in
    // the form of three metrics: count, sum, and p50 as well as the additional metric from copying the histogram.
    let flushed_metrics = get_flushed_metrics(flush_ts(1), &mut state);
    assert_eq!(flushed_metrics.len(), 4);

    // Create versions of the metric for each of the statistics we're expecting to emit. The values themselves don't
    // matter here, but we do need a `Metric` for it to compare the context to.
    let count_metric = Metric::rate("metric1.count", 0.0, BUCKET_WIDTH);
    let sum_metric = Metric::gauge("metric1.sum", 0.0);
    let p50_metric = Metric::gauge("metric1.p50", 0.0);
    let expected_distribution = Metric::distribution("dist_prefix.metric1", &values[..]);

    // We use a less strict error ratio (how much the expected vs actual) for the percentile check, as we generally
    // expect the value to be somewhat off the exact value due to the lossy nature of `DDSketch`.
    assert_flushed_distribution_metric!(expected_distribution, &flushed_metrics[0], [bucket_ts(1) => &values[..]]);
    assert_flushed_scalar_metric!(count_metric, &flushed_metrics[1], [bucket_ts(1) => 5.0]);
    assert_flushed_scalar_metric!(p50_metric, &flushed_metrics[2], [bucket_ts(1) => 3.0], error_ratio => 0.0025);
    assert_flushed_scalar_metric!(sum_metric, &flushed_metrics[3], [bucket_ts(1) => 15.0]);
}

#[test]
fn nonaggregated_counters_to_rate() {
    let counter_value = 42.0;

    // Create a basic aggregation state.
    let mut state = TestState::new(
        BUCKET_WIDTH_SECS,
        10,
        COUNTER_EXPIRE,
        HistogramConfiguration::default(),
        AggregateTelemetry::noop(),
    );

    // Create a simple non-aggregated counter, and insert it.
    let input_metric = Metric::counter("metric1", counter_value);
    assert!(state.insert(insert_ts(1), input_metric.clone()));

    // Flush the aggregation state, and observe that we've emitted the expected counter and that it has the right
    // value, but specifically that it's a rate with an interval that matches our configured bucket width:
    let flushed_metrics = get_flushed_metrics(flush_ts(1), &mut state);
    assert_eq!(flushed_metrics.len(), 1);
    let flushed_metric = &flushed_metrics[0];

    assert_flushed_scalar_metric!(&input_metric, flushed_metric, [bucket_ts(1) => counter_value]);
    assert_eq!(flushed_metric.values().as_str(), "rate");
}

#[test]
fn telemetry() {
    // TODO: We don't check `component_events_dropped_total` here as it's set directly in the aggregate
    // component future rather than `AggregationState`, which is harder to drive overall and would have
    // required even more boilerplate.
    //
    // Leaving that as a future improvement.

    let recorder = TestRecorder::default();
    let _local = metrics::set_default_local_recorder(&recorder);

    let builder = MetricsBuilder::default();
    let telemetry = AggregateTelemetry::new(&builder);

    let mut state = TestState::new(
        BUCKET_WIDTH_SECS,
        2,
        COUNTER_EXPIRE,
        HistogramConfiguration::default(),
        telemetry,
    );

    // Make sure our telemetry is registered at default values.
    assert_eq!(recorder.gauge("aggregate_active_contexts"), Some(0.0));
    assert_eq!(
        recorder.counter(("component_events_dropped_total", &[("intentional", "true")])),
        Some(0)
    );
    for metric_type in &["counter", "gauge", "rate", "set", "histogram", "distribution"] {
        assert_eq!(
            recorder.gauge(("aggregate_active_contexts_by_type", &[("metric_type", *metric_type)])),
            Some(0.0)
        );
    }

    // Insert a counter with a non-timestamped value.
    assert!(state.insert(insert_ts(1), Metric::counter("metric1", 42.0)));
    assert_eq!(recorder.gauge("aggregate_active_contexts"), Some(1.0));
    assert_eq!(
        recorder.gauge(("aggregate_active_contexts_by_type", &[("metric_type", "counter")])),
        Some(1.0)
    );

    // Insert a gauge with a timestamped value.
    assert!(state.insert(insert_ts(1), Metric::gauge("metric2", (insert_ts(1), 42.0))));
    assert_eq!(recorder.gauge("aggregate_active_contexts"), Some(2.0));
    assert_eq!(
        recorder.gauge(("aggregate_active_contexts_by_type", &[("metric_type", "gauge")])),
        Some(1.0)
    );

    // We've reached our context limit at this point, so the next metric should not be inserted.
    assert!(!state.insert(insert_ts(1), Metric::counter("metric3", 42.0)));
    assert_eq!(recorder.gauge("aggregate_active_contexts"), Some(2.0));
    assert_eq!(
        recorder.gauge(("aggregate_active_contexts_by_type", &[("metric_type", "counter")])),
        Some(1.0)
    );

    // Now let's flush the state which should flush the gauge entirely, reducing the context count, but not flush
    // the counter, since it'll be in zero-value mode.
    let _ = get_flushed_metrics(flush_ts(1), &mut state);
    assert_eq!(recorder.gauge("aggregate_active_contexts"), Some(1.0));
    assert_eq!(
        recorder.gauge(("aggregate_active_contexts_by_type", &[("metric_type", "counter")])),
        Some(1.0)
    );
    assert_eq!(
        recorder.gauge(("aggregate_active_contexts_by_type", &[("metric_type", "gauge")])),
        Some(0.0)
    );
}

#[test]
fn state_usage_counts_one_entry_per_context() {
    assert_eq!(
        STATE_ENTRY_BYTES,
        std::mem::size_of::<Context>() + std::mem::size_of::<super::state::AggregatedMetric>()
    );
}

// The decisions the core took over from the native run loop.

fn core(context_limit: usize, counter_expiry: Option<u64>, flush_open_windows: bool) -> AggregateCore {
    AggregateCore::new(
        AggregateParams {
            window: BUCKET_WIDTH_SECS,
            context_limit,
            counter_expiry: counter_expiry.and_then(NonZeroU64::new),
            flush_open_windows,
            histograms: HistogramConfiguration::default(),
        },
        AggregateTelemetry::noop(),
    )
}

fn flush_names(core: &mut AggregateCore, now: u64, last: bool) -> Option<Vec<String>> {
    let mut names = Vec::new();
    core.flush_into(now, last, |metric| names.push(metric.context().name().to_string()))?;
    names.sort();
    Some(names)
}

#[test]
fn an_empty_core_is_not_flushed() {
    let mut core = core(10, Some(300), false);
    assert!(core.is_empty());
    assert!(core.flush(1_700_000_015, false).is_none());
    assert_eq!(flush_names(&mut core, 1_700_000_015, true), None);

    core.admit(1_700_000_016, [Metric::gauge("g", 1.0)]);
    assert_eq!(core.len(), 1);
    assert_eq!(
        flush_names(&mut core, 1_700_000_030, false),
        Some(vec!["g".to_string()])
    );
    assert!(core.is_empty());
}

#[test]
fn a_drain_or_a_discard_of_an_empty_core_forgets_its_flushes_and_a_flush_does_not() {
    // A flush of an empty core does not run, so the next flush adds idle-counter zeros back to the last flush that ran,
    // as native aggregation does. A drain or a discard ends the core's timeline whether or not it holds anything, so
    // the next flush adds none.
    const B: u64 = 1_700_000_000;
    let flush_params = FlushParams {
        counter_expiry: NonZeroU64::new(300),
        flush_open_windows: false,
        histograms: HistogramConfiguration::default(),
        context_limit: 10,
        filter: None,
    };
    type EmptyPass = fn(&mut AggregateCore, &FlushParams) -> bool;
    let cases: [(&str, EmptyPass, usize); 3] = [
        ("a flush", |core, _| core.flush(B + 300, false).is_none(), 61),
        ("a drain", |core, params| core.begin_drain(params).is_none(), 1),
        ("a discard", |core, params| core.begin_discard(params).is_none(), 1),
    ];
    for (case, empty_pass, points) in cases {
        let mut core = core(10, Some(300), false);
        core.admit(B + 1, [Metric::gauge("g", 1.0)]);
        assert_eq!(flush_names(&mut core, B + 10, false), Some(vec!["g".to_string()]));
        assert!(
            empty_pass(&mut core, &flush_params),
            "{}: an empty core starts no pass",
            case
        );

        // A counter seen much later: zeros at every bucket from the flush at B+10 through the one before B+620, or
        // only its own point.
        core.admit(B + 605, [Metric::counter("c", 1.0)]);
        let mut emitted = Vec::new();
        core.flush_into(B + 620, false, |metric| emitted.push(metric));
        assert_eq!(emitted.len(), 1, "{}", case);
        assert_eq!(emitted[0].values().len(), points, "{}", case);
    }
}

#[test]
fn admitting_counts_drops_and_reports_the_limit_once_per_breach() {
    let recorder = TestRecorder::default();
    let _local = metrics::set_default_local_recorder(&recorder);
    let mut core = AggregateCore::new(
        AggregateParams {
            window: BUCKET_WIDTH_SECS,
            context_limit: 2,
            counter_expiry: None,
            flush_open_windows: false,
            histograms: HistogramConfiguration::default(),
        },
        AggregateTelemetry::new(&MetricsBuilder::default()),
    );
    let dropped = || recorder.counter(("component_events_dropped_total", &[("intentional", "true")]));

    let outcome = core.admit(
        1_700_000_001,
        [
            Metric::gauge("a", 1.0),
            Metric::gauge("b", 1.0),
            Metric::gauge("c", ScalarPoints::from([1.0, 2.0, 3.0])),
            Metric::gauge("a", 2.0),
            Metric::gauge("d", 1.0),
        ],
    );
    assert_eq!(
        outcome,
        AdmitOutcome {
            accepted: 3,
            dropped: 2,
            dropped_points: 4,
            limit_reached: true,
            ..AdmitOutcome::default()
        }
    );
    assert!(core.context_limit_breached());
    assert_eq!(dropped(), Some(2));

    // Still breached: further drops are counted, but the limit is not reported again.
    let outcome = core.admit(1_700_000_002, [Metric::gauge("e", 1.0)]);
    assert_eq!(
        outcome,
        AdmitOutcome {
            accepted: 0,
            dropped: 1,
            dropped_points: 1,
            limit_reached: false,
            ..AdmitOutcome::default()
        }
    );
    assert_eq!(dropped(), Some(3));

    // A flush that empties the core clears the breach, and a later drop reports it again.
    assert_eq!(
        core.flush_into(1_700_000_010, false, |_| {}),
        Some(FlushOutcome {
            recovered: true,
            ..FlushOutcome::default()
        })
    );
    assert!(!core.context_limit_breached());
    let outcome = core.admit(
        1_700_000_011,
        [
            Metric::gauge("x", 1.0),
            Metric::gauge("y", 1.0),
            Metric::gauge("z", 1.0),
        ],
    );
    assert!(outcome.limit_reached);
    assert_eq!(recorder.counter("aggregate_flushes_total"), Some(1));
}

#[test]
fn flush_open_windows_applies_only_to_the_last_flush() {
    // A counter seen in [10, 20) and flushed at 21 is idle afterwards. At 37, [20, 30) is closed and [30, 40) is
    // open: a last flush with `flush_open_windows` counts the open bucket as closed for its zeros, but emits only
    // closed buckets either way, so the open bucket's zero is merged and never emitted.
    for (flush_open_windows, last) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut core = core(10, Some(300), flush_open_windows);
        core.admit(1_700_000_012, [Metric::counter("c", 1.0)]);
        let mut emitted = Vec::new();
        core.flush_into(1_700_000_021, false, |metric| emitted.push(metric));
        assert_eq!(emitted.len(), 1);

        let mut emitted = Vec::new();
        core.flush_into(1_700_000_037, last, |metric| emitted.push(metric));
        assert_eq!(emitted.len(), 1);
        assert_eq!(
            emitted[0].values(),
            &MetricValues::rate(ScalarPoints::from([(1_700_000_020, 0.0)]), BUCKET_WIDTH),
            "flush_open_windows={} last={}",
            flush_open_windows,
            last
        );
        // The open bucket's zero is held only when it was counted as closed.
        let held = core.held_points();
        assert_eq!(
            held,
            if flush_open_windows && last { (1, 1) } else { (0, 0) },
            "flush_open_windows={} last={}",
            flush_open_windows,
            last
        );
    }
}

#[test]
fn a_flush_visits_one_context_per_step() {
    let mut core = core(10, Some(300), false);
    core.admit(
        1_700_000_001,
        [
            Metric::gauge("a", 1.0),
            Metric::gauge("b", 1.0),
            Metric::gauge("c", 1.0),
        ],
    );
    let mut flush = core.flush(1_700_000_010, false).expect("the core holds contexts");
    let mut steps = 0;
    let mut names = BTreeSet::new();
    loop {
        let mut emitted = Vec::new();
        if !flush.emit_next(|metric| emitted.push(metric)) {
            assert!(emitted.is_empty());
            break;
        }
        steps += 1;
        assert_eq!(emitted.len(), 1);
        names.insert(emitted[0].context().name().to_string());
    }
    assert_eq!(steps, 3);
    assert_eq!(
        names,
        BTreeSet::from(["a".to_string(), "b".to_string(), "c".to_string()])
    );
    assert_eq!(flush.finish(), FlushOutcome::default());
    assert!(core.is_empty());
}

#[test]
fn an_abandoned_flush_removes_and_records_nothing() {
    let recorder = TestRecorder::default();
    let _local = metrics::set_default_local_recorder(&recorder);
    let mut core = AggregateCore::new(
        AggregateParams {
            window: BUCKET_WIDTH_SECS,
            context_limit: 2,
            counter_expiry: Some(NonZeroU64::new(300).unwrap()),
            flush_open_windows: false,
            histograms: HistogramConfiguration::default(),
        },
        AggregateTelemetry::new(&MetricsBuilder::default()),
    );
    core.admit(
        1_700_000_001,
        [
            Metric::gauge("a", 1.0),
            Metric::gauge("b", 1.0),
            Metric::gauge("c", 1.0),
        ],
    );
    assert!(core.context_limit_breached());

    // The host delivers the first context's metric, then fails to deliver the second's.
    let mut flush = core.flush(1_700_000_010, false).expect("the core holds contexts");
    assert!(flush.emit_next(|_| {}));
    assert!(flush.emit_next(|_| {}));
    flush.abandon();

    // Nothing was removed and the breach stands. The first context was queued for removal and counted as removed;
    // the second, whose delivery failed, was not.
    assert_eq!(core.len(), 2);
    assert!(core.context_limit_breached());
    assert_eq!(core.held_points(), (0, 0));
    assert_eq!(recorder.gauge("aggregate_active_contexts"), Some(1.0));
    assert_eq!(recorder.counter("aggregate_flushes_total"), Some(1));

    // The next flush emits nothing, since both values were split off, and removes both contexts, counting each visit
    // as a removal again, as native does.
    assert_eq!(flush_names(&mut core, 1_700_000_020, false), Some(vec![]));
    assert!(core.is_empty());
    assert!(!core.context_limit_breached());
    assert_eq!(recorder.gauge("aggregate_active_contexts"), Some(-1.0));
}

// Differential tests against the frozen pre-extraction state machine.

/// A metric's description, exact enough to compare emitted metrics as a multiset: its context's parts with sorted
/// tags, its metadata, its kind, and every point, with scalars by their bits and set members sorted.
fn describe(metric: &Metric) -> String {
    let context = metric.context();
    let mut tags = context.tags().into_iter().map(|tag| tag.as_str()).collect::<Vec<_>>();
    tags.sort();
    let mut origin_tags = context
        .origin_tags()
        .into_iter()
        .map(|tag| tag.as_str())
        .collect::<Vec<_>>();
    origin_tags.sort();
    let points = match metric.values() {
        MetricValues::Counter(points) | MetricValues::Gauge(points) | MetricValues::Rate(points, _) => points
            .into_iter()
            .map(|(ts, value)| format!("{:?}={:#x}", ts, value.to_bits()))
            .collect::<Vec<_>>(),
        MetricValues::Set(points) => points
            .members()
            .map(|(ts, members)| {
                let mut members = members.iter().collect::<Vec<_>>();
                members.sort();
                format!("{:?}={:?}", ts, members)
            })
            .collect(),
        MetricValues::Distribution(points) => points
            .into_iter()
            .map(|(ts, sketch)| format!("{:?}={:?}", ts, sketch))
            .collect(),
        MetricValues::Histogram(points) => points
            .into_iter()
            .map(|(ts, histogram)| format!("{:?}={:?}", ts, histogram))
            .collect(),
    };
    let kind = match metric.values() {
        MetricValues::Rate(_, interval) => format!("rate({:?})", interval),
        values => values.as_str().to_string(),
    };
    format!(
        "{} host={:?} tags={:?} origin_tags={:?} {:?} {} {:?}",
        context.name(),
        context.host(),
        tags,
        origin_tags,
        metric.metadata(),
        kind,
        points
    )
}

pub(super) fn multiset(metrics: &[Metric]) -> Vec<String> {
    let mut described = metrics.iter().map(describe).collect::<Vec<_>>();
    described.sort();
    described
}

pub(super) fn snapshot_set(entries: Vec<AggregateContextSnapshotEntry>) -> Vec<String> {
    let mut described = entries
        .into_iter()
        .map(|entry| {
            format!(
                "{} {:?} {:?} {:?}",
                entry.context(),
                entry.context().host(),
                entry.metric_type(),
                entry.unit()
            )
        })
        .collect::<Vec<_>>();
    described.sort();
    described
}

/// One step of a generated run: a batch arriving, or a flush tick. Times are offsets from the previous step's time.
#[derive(Clone, Debug)]
pub(super) enum Step {
    Admit { after: i64, metrics: Vec<GeneratedMetric> },
    Tick { after: i64 },
}

/// A generated metric, built relative to the time its batch arrives.
#[derive(Clone, Debug)]
pub(super) struct GeneratedMetric {
    name: &'static str,
    tags: Vec<&'static str>,
    unit: &'static str,
    kind: u8,
    interval: u64,
    sample_rate: Option<f64>,
    /// Pieces of the metric's values: an optional timestamp offset from arrival, and the values at it.
    pieces: Vec<(Option<i64>, Vec<i8>)>,
}

impl GeneratedMetric {
    pub(super) fn build(&self, now: u64) -> Metric {
        let mut values: Option<MetricValues> = None;
        for (offset, raw) in &self.pieces {
            let raw = raw.iter().map(|value| f64::from(*value)).collect::<Vec<_>>();
            let rate = self
                .sample_rate
                .map(|rate| SampleRate::try_from(rate).expect("valid sample rate"));
            let oks = || raw.iter().copied().map(Ok::<f64, ()>);
            let mut piece = match self.kind {
                0 => MetricValues::counter_sampled_fallible(oks(), rate).unwrap(),
                1 => MetricValues::rate(&raw[..], Duration::from_secs(self.interval)),
                2 => MetricValues::gauge(&raw[..]),
                3 => {
                    let members = raw
                        .iter()
                        .map(|value| format!("m{}", value.rem_euclid(4.0)))
                        .collect::<Vec<_>>();
                    let mut set = MetricValues::set(members[0].as_str());
                    for member in &members[1..] {
                        set.merge(MetricValues::set(member.as_str()));
                    }
                    set
                }
                4 => MetricValues::histogram_sampled_fallible(oks(), rate).unwrap(),
                _ => MetricValues::distribution_sampled_fallible(oks(), rate).unwrap(),
            };
            if let Some(offset) = offset {
                piece.set_timestamp(now.saturating_add_signed(*offset));
            }
            match &mut values {
                Some(values) => values.merge(piece),
                None => values = Some(piece),
            }
        }
        let tags = self.tags.iter().map(|tag| Tag::from(*tag)).collect::<TagSet>();
        let context = Context::from_parts(self.name, tags);
        let metadata = MetricMetadata::default().with_unit(MetaString::from(self.unit));
        Metric::from_parts(context, values.expect("at least one piece"), metadata)
    }
}

fn arb_generated_metric() -> impl Strategy<Value = GeneratedMetric> {
    let piece = (
        prop_oneof![3 => Just(None), 1 => (-40i64..40).prop_map(Some)],
        vec(-3i8..10, 1..4),
    );
    (
        proptest::sample::select(&["a", "b", "c", "d"][..]),
        proptest::sample::subsequence(&["t:1", "t:2"][..], 0..=2),
        proptest::sample::select(&["", "millisecond"][..]),
        0u8..6,
        proptest::sample::select(&[0u64, 10, 20][..]),
        proptest::option::of(proptest::sample::select(&[0.5, 0.4, 0.25][..])),
        vec(piece, 1..3),
    )
        .prop_map(
            |(name, tags, unit, kind, interval, sample_rate, pieces)| GeneratedMetric {
                name,
                tags,
                unit,
                kind,
                interval,
                sample_rate,
                pieces,
            },
        )
}

pub(super) fn arb_steps() -> impl Strategy<Value = Vec<Step>> {
    let after = prop_oneof![
        8 => 0i64..20,
        2 => 20i64..120,
        1 => 120i64..1200,
        1 => -30i64..0,
    ];
    let step = prop_oneof![
        3 => (after.clone(), vec(arb_generated_metric(), 0..8))
            .prop_map(|(after, metrics)| Step::Admit { after, metrics }),
        2 => after.prop_map(|after| Step::Tick { after }),
    ];
    vec(step, 1..40)
}

/// Generated aggregation parameters, as configuration spells them.
#[derive(Clone, Debug)]
pub(super) struct GeneratedParams {
    window: u64,
    pub(super) context_limit: usize,
    counter_expiry: Option<u64>,
    flush_open_windows: bool,
    aggregates: Vec<String>,
    percentiles: Vec<String>,
    copy_to_distribution: bool,
    copy_to_distribution_prefix: String,
}

impl GeneratedParams {
    /// Returns these parameters with the context limit lowered to `capacity`, if that is lower: the limit native
    /// aggregation applies that matches a core with this limit drawing on a global budget of `capacity` alone.
    pub(super) fn limited_to(&self, capacity: Option<u64>) -> Self {
        let mut params = self.clone();
        if let Some(capacity) = capacity {
            params.context_limit = params
                .context_limit
                .min(usize::try_from(capacity).unwrap_or(usize::MAX));
        }
        params
    }
}

pub(super) fn arb_params() -> impl Strategy<Value = GeneratedParams> {
    (
        proptest::sample::select(&[1u64, 7, 10, 15, 60][..]),
        prop_oneof![1 => Just(0usize), 4 => 1usize..6, 2 => Just(1000usize)],
        proptest::option::of(proptest::sample::select(&[0u64, 10, 20, 300][..])),
        any::<bool>(),
        proptest::sample::subsequence(&["count", "sum", "min", "max", "avg", "median"][..], 0..=6),
        proptest::sample::subsequence(&["0.5", "0.95", "0.955", "0", "1"][..], 0..=3),
        any::<bool>(),
        proptest::sample::select(&["", "dist."][..]),
    )
        .prop_map(
            |(window, context_limit, counter_expiry, flush_open_windows, aggregates, percentiles, copy, prefix)| {
                GeneratedParams {
                    window,
                    context_limit,
                    counter_expiry,
                    flush_open_windows,
                    aggregates: aggregates.into_iter().map(str::to_string).collect(),
                    percentiles: percentiles.into_iter().map(str::to_string).collect(),
                    copy_to_distribution: copy,
                    copy_to_distribution_prefix: prefix.to_string(),
                }
            },
        )
}

/// The frozen state machine with its run-loop decisions, and the recorder its telemetry reports to.
pub(super) fn reference_run_loop(params: &GeneratedParams, recorder: &TestRecorder) -> reference::RunLoop {
    let _local = metrics::set_default_local_recorder(recorder);
    let telemetry = reference::telemetry::Telemetry::new(&MetricsBuilder::default());
    let hist_config = reference::config::HistogramConfiguration::try_new(
        &params.aggregates,
        &params.percentiles,
        params.copy_to_distribution,
        params.copy_to_distribution_prefix.clone(),
    )
    .expect("generated statistics are valid");
    // As the original transform built it, where `None` and `Some(0)` both disable expiry.
    let state = reference::AggregationState::new(
        NonZeroU64::new(params.window).unwrap(),
        params.context_limit,
        params.counter_expiry.filter(|s| *s != 0).map(Duration::from_secs),
        hist_config,
        telemetry.clone(),
    );
    reference::RunLoop {
        state,
        telemetry,
        flush_open_windows: params.flush_open_windows,
    }
}

/// The parameters of native aggregation that generated parameters spell.
pub(super) fn aggregate_params(params: &GeneratedParams) -> AggregateParams {
    let histograms = HistogramConfiguration::try_new(
        &params.aggregates,
        &params.percentiles,
        params.copy_to_distribution,
        params.copy_to_distribution_prefix.clone(),
    )
    .expect("generated statistics are valid");
    AggregateParams {
        window: NonZeroU64::new(params.window).unwrap(),
        context_limit: params.context_limit,
        counter_expiry: params.counter_expiry.and_then(NonZeroU64::new),
        flush_open_windows: params.flush_open_windows,
        histograms,
    }
}

pub(super) fn kernel_core(params: &GeneratedParams, recorder: &TestRecorder) -> AggregateCore {
    let _local = metrics::set_default_local_recorder(recorder);
    AggregateCore::new(
        aggregate_params(params),
        AggregateTelemetry::new(&MetricsBuilder::default()),
    )
}

/// Runs the frozen flush at `now` through a real buffered dispatcher and returns what it emitted.
pub(super) async fn reference_tick(
    run_loop: &mut reference::RunLoop, now: u64, last: bool,
) -> Option<(Result<bool, String>, Vec<Metric>)> {
    let mut dispatcher = Dispatcher::<EventsBuffer>::new(ComponentContext::test_transform("reference"));
    let (sender, mut receiver) = mpsc::channel(4096);
    dispatcher.add_output(OutputName::Default).unwrap();
    dispatcher
        .attach_sender_to_output(&OutputName::Default, sender)
        .unwrap();
    let mut buffered = dispatcher.buffered().expect("the default output exists");

    let outcome = run_loop.tick(now, last, &mut buffered).await?;
    buffered.flush().await.expect("the receiver is open");
    drop(dispatcher);

    let mut emitted = Vec::new();
    while let Ok(buffer) = receiver.try_recv() {
        emitted.extend(
            buffer
                .into_iter()
                .map(|event| event.try_into_metric().expect("only metrics")),
        );
    }
    Some((outcome.map_err(|error| error.to_string()), emitted))
}

/// The telemetry values to compare, in a fixed order.
pub(super) fn telemetry_values(recorder: &TestRecorder) -> Vec<(String, Option<f64>)> {
    let mut values = vec![
        (
            "aggregate_active_contexts".to_string(),
            recorder.gauge("aggregate_active_contexts"),
        ),
        (
            "component_events_dropped_total".to_string(),
            recorder
                .counter(("component_events_dropped_total", &[("intentional", "true")]))
                .map(|value| value as f64),
        ),
        (
            "aggregate_flushes_total".to_string(),
            recorder.counter("aggregate_flushes_total").map(|value| value as f64),
        ),
    ];
    for data_type in ["series", "sketches"] {
        values.push((
            format!("aggregate_flushed_total{{{}}}", data_type),
            recorder
                .counter(("aggregate_flushed_total", &[("data_type", data_type)]))
                .map(|value| value as f64),
        ));
    }
    for metric_type in ["counter", "gauge", "rate", "set", "histogram", "distribution"] {
        for name in [
            "aggregate_active_contexts_by_type",
            "aggregate_active_contexts_bytes_by_type",
        ] {
            values.push((
                format!("{}{{{}}}", name, metric_type),
                recorder.gauge((name, &[("metric_type", metric_type)])),
            ));
        }
    }
    values
}

/// Drives the frozen state machine and a core through one run, as the native loop would: each batch arrives, and each
/// tick flushes, at one time for both, and the run ends with the final tick, as the native loop's does when its input
/// closes. Compares every admission decision, every flush, and the state after each step, and then runs `after_step`
/// on the core.
pub(super) fn run_against_reference(
    expected: &mut reference::RunLoop, actual: &mut AggregateCore, start: u64, steps: &[Step],
    mut after_step: impl FnMut(&AggregateCore) -> Result<(), TestCaseError>,
) -> Result<(), TestCaseError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("the test runtime builds");

    let mut now = 1_700_000_000 + start;
    let last_step = steps.len();
    for (position, step) in steps.iter().chain([Step::Tick { after: 1 }].iter()).enumerate() {
        match step {
            Step::Admit { after, metrics } => {
                now = now.saturating_add_signed(*after);
                let metrics = metrics.iter().map(|metric| metric.build(now)).collect::<Vec<_>>();
                let (inserted, warned) = expected.events(now, metrics.iter().cloned().map(Event::Metric));
                let outcome = actual.admit(now, metrics);
                prop_assert_eq!(outcome.accepted, inserted.iter().filter(|inserted| **inserted).count());
                prop_assert_eq!(outcome.dropped, inserted.iter().filter(|inserted| !**inserted).count());
                prop_assert_eq!(outcome.limit_reached, warned);
            }
            Step::Tick { after } => {
                now = now.saturating_add_signed(*after);
                let last = position == last_step;
                let reference_flush = runtime.block_on(reference_tick(expected, now, last));
                let mut emitted = Vec::new();
                let outcome = actual.flush_into(now, last, |metric| emitted.push(metric));
                match (reference_flush, outcome) {
                    (None, None) => {}
                    (Some((Ok(recovered), expected_emitted)), Some(outcome)) => {
                        prop_assert_eq!(outcome.recovered, recovered);
                        prop_assert_eq!(multiset(&expected_emitted), multiset(&emitted), "flush at {}", now);
                    }
                    (expected, actual) => {
                        return Err(TestCaseError::fail(format!(
                            "flush at {}: the frozen state machine flushed: {:?}, the core flushed: {:?}",
                            now,
                            expected.map(|(outcome, _)| outcome),
                            actual
                        )));
                    }
                }
            }
        }
        prop_assert_eq!(expected.state.context_limit_breached(), actual.context_limit_breached());
        prop_assert_eq!(expected.state.is_empty(), actual.is_empty());
        prop_assert_eq!(
            snapshot_set(expected.state.snapshot_contexts()),
            snapshot_set(actual.snapshot_contexts())
        );
        after_step(actual)?;
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn property_test_core_matches_the_pre_extraction_state_machine(
        params in arb_params(),
        start in 0u64..3600,
        steps in arb_steps(),
    ) {
        let reference_recorder = TestRecorder::default();
        let kernel_recorder = TestRecorder::default();
        let mut expected = reference_run_loop(&params, &reference_recorder);
        let mut actual = kernel_core(&params, &kernel_recorder);
        run_against_reference(&mut expected, &mut actual, start, &steps, |_| Ok(()))?;
        prop_assert_eq!(telemetry_values(&reference_recorder), telemetry_values(&kernel_recorder));
    }
}

#[test]
fn an_abandoned_flush_leaves_the_state_the_failed_native_flush_left() {
    // Natively, a flush stops at the first delivery that fails: the buffered dispatcher sends a full buffer of 1,024
    // metrics when the 1,025th is pushed, and that send fails once the receiver is gone. A host of the core abandons
    // the flush at the same delivery. Both then leave 1,024 contexts queued for removal, the 1,025th split but not
    // queued, and the rest unvisited, which the next flush shows: it emits the unvisited contexts' gauges, and counts
    // every context it visits as removed again.
    //
    // The two visit contexts in different orders, so the names have one length, for every context to have one size.
    const CONTEXTS: usize = 1100;
    let gauges = || {
        (0..CONTEXTS)
            .map(|i| Metric::gauge(Context::from_parts(format!("g{:04}", i), TagSet::default()), 1.0))
            .collect::<Vec<_>>()
    };
    let params = GeneratedParams {
        window: 10,
        context_limit: 10_000,
        counter_expiry: Some(300),
        flush_open_windows: false,
        aggregates: Vec::new(),
        percentiles: Vec::new(),
        copy_to_distribution: false,
        copy_to_distribution_prefix: String::new(),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let reference_recorder = TestRecorder::default();
    let mut expected = reference_run_loop(&params, &reference_recorder);
    expected.events(1_700_000_001, gauges().into_iter().map(Event::Metric));
    let failed = runtime.block_on(async {
        let mut dispatcher = Dispatcher::<EventsBuffer>::new(ComponentContext::test_transform("reference"));
        let (sender, receiver) = mpsc::channel(4096);
        dispatcher.add_output(OutputName::Default).unwrap();
        dispatcher
            .attach_sender_to_output(&OutputName::Default, sender)
            .unwrap();
        drop(receiver);
        let mut buffered = dispatcher.buffered().unwrap();
        expected.tick(1_700_000_010, false, &mut buffered).await
    });
    assert!(matches!(failed, Some(Err(_))));

    let kernel_recorder = TestRecorder::default();
    let mut actual = kernel_core(&params, &kernel_recorder);
    actual.admit(1_700_000_001, gauges());
    let mut flush = actual.flush(1_700_000_010, false).unwrap();
    let mut delivered = 0;
    let mut pending = Vec::new();
    'contexts: while flush.emit_next(|metric| pending.push(metric)) {
        for _ in pending.drain(..) {
            if delivered == 1024 {
                break 'contexts;
            }
            delivered += 1;
        }
    }
    flush.abandon();
    assert_eq!(
        telemetry_values(&reference_recorder),
        telemetry_values(&kernel_recorder)
    );
    assert_eq!(
        reference_recorder.gauge("aggregate_active_contexts"),
        Some((CONTEXTS - 1024) as f64)
    );

    let (recovered, expected_emitted) = runtime
        .block_on(reference_tick(&mut expected, 1_700_000_020, false))
        .unwrap();
    assert_eq!(recovered, Ok(false));
    let mut emitted = Vec::new();
    actual.flush_into(1_700_000_020, false, |metric| emitted.push(metric));
    assert_eq!(expected_emitted.len(), CONTEXTS - 1025);
    assert_eq!(emitted.len(), CONTEXTS - 1025);
    assert!(actual.is_empty());
    assert_eq!(
        telemetry_values(&reference_recorder),
        telemetry_values(&kernel_recorder)
    );
    assert_eq!(
        kernel_recorder.gauge("aggregate_active_contexts"),
        Some(CONTEXTS as f64 - 1024.0 - CONTEXTS as f64)
    );
}

mod budgets;

pub(super) mod emission;
