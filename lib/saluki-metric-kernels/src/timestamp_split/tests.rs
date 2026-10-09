use std::{num::NonZeroU64, time::Duration};

use metrics::set_default_local_recorder;
use proptest::{collection::vec, prelude::*};
use saluki_core::{
    data_model::event::{
        metric::{context::Context, Metric, ScalarPoints},
        service_check::{CheckStatus, ServiceCheck},
        Event,
    },
    execution::{BatchHeader, LossClass, LossCount},
    topology::EventsBuffer,
};
use saluki_metrics::{test::TestRecorder, MetricsBuilder};

use super::{
    has_timestamped_metrics, reference, split_metric, split_timestamped, to_passthrough, TimestampSplit,
    TimestampSplitTelemetry,
};
use crate::{
    test_support::{buffer_of, consume, describe, metrics_of},
    testing::arb_metric,
};

const WINDOW_SECS: NonZeroU64 = NonZeroU64::new(10).expect("not zero");
const WINDOW: Duration = Duration::from_secs(WINDOW_SECS.get());

fn mixed_counter(name: &'static str) -> Metric {
    let mixed_values = ScalarPoints::from_iter([(None, 2.0), (NonZeroU64::new(123), 3.0)]);
    Metric::counter(Context::from_static_name(name), mixed_values)
}

#[test]
fn has_timestamped_metrics_detects_any_timestamped_value() {
    let mut buffer = EventsBuffer::default();
    assert!(!has_timestamped_metrics(&buffer));

    assert!(buffer.try_push(Event::Metric(Metric::gauge("metric1", 1.0))).is_none());
    assert!(!has_timestamped_metrics(&buffer));

    // A metric where only _some_ values are timestamped still has to take the splitting path.
    assert!(buffer.try_push(Event::Metric(mixed_counter("metric2"))).is_none());
    assert!(has_timestamped_metrics(&buffer));
}

#[test]
fn split_all_timestamped_values_go_to_passthrough() {
    let metric = Metric::gauge("metric1", (123, 1.0));
    let (timestamped, remainder) = split_timestamped(metric.clone());
    assert_eq!(timestamped, Some(metric));
    assert_eq!(remainder, None);
}

#[test]
fn split_non_timestamped_values_go_to_remainder() {
    let metric = Metric::gauge("metric1", 1.0);
    let (timestamped, remainder) = split_timestamped(metric.clone());
    assert_eq!(timestamped, None);
    assert_eq!(remainder, Some(metric));
}

#[test]
fn split_mixed_timestamped_values_go_to_both_outputs() {
    let context = Context::from_static_name("metric1");
    let metric = mixed_counter("metric1");

    let (timestamped, remainder) = split_timestamped(metric);

    let timestamped = timestamped.expect("mixed metric should yield a timestamped half");
    assert_eq!(timestamped.context(), &context);
    assert!(timestamped.values().all_timestamped());

    let remainder = remainder.expect("mixed metric should yield a non-timestamped remainder");
    assert_eq!(remainder.context(), &context);
    assert!(!remainder.values().any_timestamped());
}

#[test]
fn preaggregated_counters_become_rates_over_the_window() {
    // Counters become rates whose interval is the aggregation window; every other kind passes as it is, including a
    // rate with an interval of its own.
    let cases = [
        (
            Metric::counter("metric1", (123456, 42.0)),
            Metric::rate("metric1", (123456, 42.0), WINDOW),
        ),
        (
            Metric::rate("metric1", (123456, 42.0), Duration::from_secs(60)),
            Metric::rate("metric1", (123456, 42.0), Duration::from_secs(60)),
        ),
        (
            Metric::gauge("metric1", (123456, 42.0)),
            Metric::gauge("metric1", (123456, 42.0)),
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(to_passthrough(input, WINDOW_SECS), expected);
    }

    // Only the timestamped half of a mixed counter is converted.
    let (passthrough, default) = split_metric(mixed_counter("metric1"), WINDOW_SECS);
    assert_eq!(
        passthrough,
        Some(Metric::rate(
            Context::from_static_name("metric1"),
            ScalarPoints::from_iter([(NonZeroU64::new(123), 3.0)]),
            WINDOW
        ))
    );
    assert_eq!(default, Some(Metric::counter("metric1", 2.0)));
}

#[test]
fn the_kernel_routes_each_point_by_its_timestamp() {
    let mut kernel = TimestampSplit::new(WINDOW_SECS, TimestampSplitTelemetry::noop());
    let input = buffer_of([
        Metric::gauge("untimestamped", 1.0),
        Metric::counter("timestamped", (100, 2.0)),
        mixed_counter("mixed"),
        Metric::distribution("sketch", 4.0),
    ]);
    let consumed = consume(&mut kernel, &BatchHeader::default(), 2, input);

    assert_eq!(
        consumed.ports[0],
        [
            Metric::gauge("untimestamped", 1.0),
            Metric::counter("mixed", 2.0),
            Metric::distribution("sketch", 4.0),
        ]
    );
    assert_eq!(
        consumed.ports[1],
        [
            Metric::rate("timestamped", (100, 2.0), WINDOW),
            Metric::rate("mixed", (123, 3.0), WINDOW),
        ]
    );
    assert!(consumed.counters.is_empty());
}

#[test]
fn a_batch_without_timestamped_points_leaves_whole_on_the_default_port() {
    let mut kernel = TimestampSplit::new(WINDOW_SECS, TimestampSplitTelemetry::noop());
    let metrics = [Metric::gauge("a", 1.0), Metric::counter("b", 2.0)];
    let mut input = buffer_of(metrics.clone());
    // The fast path does not look at events other than metrics, which leave with the batch.
    assert!(input
        .try_push(Event::ServiceCheck(ServiceCheck::new("check", CheckStatus::Ok)))
        .is_none());
    let consumed = consume(&mut kernel, &BatchHeader::default(), 2, input);

    assert_eq!(consumed.buffers, [1, 0]);
    assert_eq!(consumed.ports[0], metrics);
    assert!(consumed.counters.is_empty());
}

#[test]
fn events_that_are_not_metrics_are_invalid_when_a_batch_is_split() {
    let mut kernel = TimestampSplit::new(WINDOW_SECS, TimestampSplitTelemetry::noop());
    let mut input = buffer_of([Metric::gauge("timestamped", (100, 1.0))]);
    assert!(input
        .try_push(Event::ServiceCheck(ServiceCheck::new("check", CheckStatus::Ok)))
        .is_none());
    let consumed = consume(&mut kernel, &BatchHeader::default(), 2, input);

    assert!(consumed.ports[0].is_empty());
    assert_eq!(consumed.ports[1], [Metric::gauge("timestamped", (100, 1.0))]);
    assert_eq!(
        consumed.counters.loss(LossClass::Invalid),
        LossCount { events: 1, points: 0 }
    );
}

#[test]
fn the_kernel_counts_passthrough_metrics_and_one_flush_per_batch() {
    let recorder = TestRecorder::default();
    let _local = set_default_local_recorder(&recorder);
    let mut kernel = TimestampSplit::new(WINDOW_SECS, TimestampSplitTelemetry::new(&MetricsBuilder::default()));

    let batches = [
        // Two passthrough metrics.
        vec![
            Metric::gauge("a", (100, 1.0)),
            mixed_counter("b"),
            Metric::gauge("c", 1.0),
        ],
        // None: no flush either.
        vec![Metric::gauge("c", 1.0)],
        // One more.
        vec![Metric::counter("d", (100, 1.0))],
    ];
    for batch in batches {
        consume(&mut kernel, &BatchHeader::default(), 2, buffer_of(batch));
    }
    assert_eq!(recorder.counter("aggregate_passthrough_metrics_total"), Some(3));
    assert_eq!(recorder.counter("aggregate_passthrough_flushes_total"), Some(2));
}

fn arb_window() -> impl Strategy<Value = NonZeroU64> {
    prop_oneof![Just(1u64), Just(10), Just(60)].prop_map(|secs| NonZeroU64::new(secs).expect("not zero"))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn property_test_splitting_matches_the_pre_extraction_transform(
        window in arb_window(),
        metrics in vec(arb_metric(), 0..24),
        with_service_check in any::<bool>(),
    ) {
        // One metric at a time.
        for metric in &metrics {
            let (timestamped, default) = reference::try_split_timestamped_values(metric.clone());
            let expected = (
                timestamped.map(|metric| describe(&reference::passthrough_conversion(metric, window))),
                default.map(|metric| describe(&metric)),
            );
            let (passthrough, default) = split_metric(metric.clone(), window);
            let actual = (passthrough.map(|metric| describe(&metric)), default.map(|metric| describe(&metric)));
            prop_assert_eq!(expected, actual);
        }

        // A whole batch through the kernel: the same metrics on each port, in the same order, and the whole buffer
        // on the default port exactly when the native fast path forwarded it. An event that is not a metric leaves
        // with the batch on the fast path, and is dropped, as invalid, when the batch is split.
        let batch = || {
            let mut buffer = buffer_of(metrics.iter().cloned());
            if with_service_check {
                let check = Event::ServiceCheck(ServiceCheck::new("check", CheckStatus::Ok));
                assert!(buffer.try_push(check).is_none());
            }
            buffer
        };
        let mut kernel = TimestampSplit::new(window, TimestampSplitTelemetry::noop());
        let consumed = consume(&mut kernel, &BatchHeader::default(), 2, batch());
        let describe_all = |metrics: &[Metric]| metrics.iter().map(describe).collect::<Vec<_>>();
        match reference::process_buffer(batch(), window) {
            reference::Routed::Whole(buffer) => {
                prop_assert!(!reference::has_timestamped_metrics(&batch()));
                let whole = !metrics.is_empty() || with_service_check;
                prop_assert_eq!(&consumed.buffers, &[usize::from(whole), 0]);
                prop_assert_eq!(describe_all(&metrics_of(buffer)), describe_all(&consumed.ports[0]));
                prop_assert!(consumed.counters.is_empty());
            }
            reference::Routed::Split { default, passthrough } => {
                prop_assert_eq!(describe_all(&default), describe_all(&consumed.ports[0]));
                prop_assert_eq!(describe_all(&passthrough), describe_all(&consumed.ports[1]));
                let dropped = u64::from(with_service_check);
                prop_assert_eq!(
                    consumed.counters.loss(LossClass::Invalid),
                    LossCount { events: dropped, points: 0 }
                );
            }
        }
    }
}
