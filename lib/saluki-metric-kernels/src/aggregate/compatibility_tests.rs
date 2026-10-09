//! Native compatibility-matrix fixtures for aggregation that only a direct drive of the core can express.
//!
//! The reference interpreter's traces cover most aggregation rows of the specification's native compatibility matrix
//! (see the plan crate's aggregation adapter fixtures). These cover what a trace cannot: a wall clock that jumps,
//! a final flush at a time no tick ran at, and the breach flag itself. The expected results are written by hand from
//! the native rules and the recorded probes of native aggregation. Times are written relative to `B`, a realistic Unix
//! time that is a multiple of 10.

use std::num::NonZeroU64;

use saluki_core::data_model::event::metric::{Metric, MetricValues, ScalarPoints};

use super::{AggregateCore, AggregateParams, AggregateTelemetry, HistogramConfiguration};

const B: u64 = 1_700_000_000;

fn core(context_limit: usize, counter_expiry: u64, flush_open_windows: bool) -> AggregateCore {
    AggregateCore::new(
        AggregateParams {
            window: NonZeroU64::new(10).unwrap(),
            context_limit,
            counter_expiry: NonZeroU64::new(counter_expiry),
            flush_open_windows,
            histograms: HistogramConfiguration::default(),
        },
        AggregateTelemetry::noop(),
    )
}

/// Describes a scalar record as `name kind [timestamp:value, ...]`, with timestamps relative to `B` and points sorted.
fn describe(metric: &Metric) -> String {
    let (kind, points) = match metric.values() {
        MetricValues::Rate(points, interval) => (format!("rate/{}s", interval.as_secs()), points),
        MetricValues::Gauge(points) => ("gauge".to_string(), points),
        other => panic!("only rates and gauges are described, not {}", other.as_str()),
    };
    let mut points = points
        .into_iter()
        .map(|(ts, value)| format!("{}:{}", ts.map_or(-1, |ts| ts.get() as i64 - B as i64), value))
        .collect::<Vec<_>>();
    points.sort();
    format!("{} {} [{}]", metric.context().name(), kind, points.join(", "))
}

/// Flushes at `B + relative` and returns the records emitted, described and sorted, or `None` if the core was empty.
fn flush(core: &mut AggregateCore, relative: u64, last: bool) -> Option<Vec<String>> {
    let mut records = Vec::new();
    core.flush_into(B + relative, last, |metric| records.push(describe(&metric)))?;
    records.sort();
    Some(records)
}

fn records(records: &[&str]) -> Option<Vec<String>> {
    Some(records.iter().map(|record| record.to_string()).collect())
}

#[test]
fn bucketing_follows_the_wall_clock_backward() {
    // Matrix: bucketing follows the wall clock, including backward. A flush after the clock jumped back from 115 to 95
    // finds no zero-value buckets, since the range from the last flush is empty, and records 95 as the last flush. A
    // point arriving at 96 lands in [90, 100), which was flushed long ago, and the next flush computes zeros from 90
    // again, so it emits a second, conflicting point for bucket 100, which already went out as 1.
    let mut core = core(10, 300, false);
    core.admit(B + 105, [Metric::counter("c", 1.0)]);
    assert_eq!(flush(&mut core, 115, false), records(&["c rate/10s [100:1]"]));
    assert_eq!(flush(&mut core, 95, false), records(&[]));
    core.admit(B + 96, [Metric::counter("c", 1.0)]);
    assert_eq!(
        flush(&mut core, 130, false),
        records(&["c rate/10s [100:0, 110:0, 120:0, 90:1]"])
    );
}

#[test]
fn a_forward_jump_fills_zeros_up_to_the_expiry_and_expires_the_counter() {
    // Matrix: after a forward jump, a flush computes a zero-value bucket for every bucket across the jump, and the
    // clock passes every counter's expiry. A counter last seen at 1 with an expiry of 300 gains zeros at 10 to 300,
    // the buckets before 301, and is removed.
    let mut core = core(10, 300, false);
    core.admit(B + 1, [Metric::counter("c", 1.0)]);
    assert_eq!(flush(&mut core, 15, false), records(&["c rate/10s [0:1]"]));

    let mut zeros = (10..=300)
        .step_by(10)
        .map(|bucket| format!("{}:0", bucket))
        .collect::<Vec<_>>();
    zeros.sort();
    assert_eq!(
        flush(&mut core, 1015, false),
        Some(vec![format!("c rate/10s [{}]", zeros.join(", "))])
    );
    assert!(core.is_empty());
}

#[test]
fn flush_open_windows_has_no_observable_effect_on_the_final_flush() {
    // Matrix and amendment 11 (probe 3): the final flush at 37, which no tick preceded, treats [30, 40) as closed for
    // idle-counter zeros when `flush_open_windows` is set, but emits only buckets closed by 37, so its output is the
    // same either way: nothing. The open buckets are discarded; only what is held differs.
    for flush_open_windows in [false, true] {
        let mut core = core(100, 300, flush_open_windows);
        core.admit(
            B + 5,
            [
                Metric::counter("a", 1.0),
                Metric::counter("idle", 1.0),
                Metric::gauge("g", 1.0),
            ],
        );
        assert_eq!(
            flush(&mut core, 15, false),
            records(&["a rate/10s [0:1]", "g gauge [0:1]", "idle rate/10s [0:1]"])
        );
        assert_eq!(
            flush(&mut core, 30, false),
            records(&["a rate/10s [10:0, 20:0]", "idle rate/10s [10:0, 20:0]"])
        );
        core.admit(B + 32, [Metric::gauge("g", 2.0), Metric::counter("a", 3.0)]);

        assert_eq!(
            flush(&mut core, 37, true),
            records(&[]),
            "flush_open_windows={}",
            flush_open_windows
        );
        assert_eq!(core.len(), 3);
        // The zero at 30 merged into `a` adds nothing; `idle` holds it alone only when the flag is set.
        let held = if flush_open_windows { (3, 3) } else { (2, 2) };
        assert_eq!(core.held_points(), held, "flush_open_windows={}", flush_open_windows);
    }
}

#[test]
fn the_breach_flag_clears_only_after_a_flush_below_the_limit() {
    // Matrix: the breach flag is set by a drop and clears only at the end of a flush that leaves fewer contexts than
    // the limit. It does not gate admission: held contexts are admitted while it is set (probe 10).
    let mut core = core(2, 300, false);
    core.admit(B + 1, [Metric::counter("a", 1.0), Metric::gauge("g", 1.0)]);
    assert!(!core.context_limit_breached());
    assert_eq!(core.admit(B + 1, [Metric::gauge("h", 1.0)]).dropped, 1);
    assert!(core.context_limit_breached());
    assert_eq!(core.admit(B + 1, [Metric::gauge("g", 2.0)]).accepted, 1);
    assert!(core.context_limit_breached());

    // The flush removes the gauge, leaving the idle counter: one context, below the limit of two.
    let mut flush = core.flush(B + 15, false).unwrap();
    while flush.emit_next(|_| {}) {}
    assert!(flush.finish().recovered);
    assert!(!core.context_limit_breached());

    core.admit(B + 16, [Metric::gauge("h", 1.0)]);
    assert!(!core.context_limit_breached());
    let outcome = core.admit(B + 16, [Metric::gauge("i", 1.0)]);
    assert!(outcome.limit_reached);
    assert!(core.context_limit_breached());

    // A flush that leaves the core at its limit keeps the flag: the counter is idle and `h` has an open point.
    core.admit(B + 21, [Metric::gauge("h", 2.0)]);
    let mut flush = core.flush(B + 25, false).unwrap();
    while flush.emit_next(|_| {}) {}
    assert!(!flush.finish().recovered);
    assert_eq!(core.len(), 2);
    assert!(core.context_limit_breached());
}

#[test]
fn a_context_limit_of_zero_drops_everything_and_never_recovers() {
    // Matrix: with a limit of zero every metric is dropped, and since an empty core is never flushed, the breach flag
    // stays set.
    let mut core = core(0, 300, false);
    let outcome = core.admit(B + 1, [Metric::gauge("g", ScalarPoints::from([1.0, 2.0]))]);
    assert_eq!((outcome.accepted, outcome.dropped, outcome.dropped_points), (0, 1, 2));
    assert!(outcome.limit_reached);
    assert_eq!(flush(&mut core, 15, false), None);
    assert!(core.context_limit_breached());
    assert!(!core.admit(B + 16, [Metric::gauge("g", 1.0)]).limit_reached);
}

#[test]
fn a_future_point_holds_its_context_against_the_limit() {
    // Matrix: a future timestamped point stays held until the clock passes its bucket, and its context counts against
    // the limit meanwhile (probe 5d).
    let mut core = core(1, 300, false);
    core.admit(B + 18, [Metric::gauge("future", (B + 95, 4.0))]);
    assert_eq!(flush(&mut core, 20, false), records(&[]));
    assert_eq!(core.admit(B + 21, [Metric::gauge("other", 1.0)]).dropped, 1);
    assert_eq!(flush(&mut core, 100, false), records(&["future gauge [95:4]"]));
    assert!(core.is_empty());
    assert_eq!(core.admit(B + 101, [Metric::gauge("other", 1.0)]).accepted, 1);
}
