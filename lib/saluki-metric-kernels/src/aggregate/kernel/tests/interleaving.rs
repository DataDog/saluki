//! Tests of admission while a flush is pending: an interleaved flush, with batches admitted between its steps, against
//! the same flush completed before the batches.
//!
//! Each case runs two hosts of the same kernel over the same batches. The one-shot host completes every flush in one
//! call and then admits the batches that follow it; the interleaved host starts the flush one context a step, admits
//! the batches while it is pending, and completes it afterwards. Their output multisets, losses, held contexts and
//! points, snapshots, breach flags and log lines, last flushes, budget use, and suppressed counts must agree once both
//! flushes have completed; only the order of records may differ.

use std::{collections::HashMap, sync::Arc};

use proptest::{collection::vec, prelude::*};
use saluki_core::data_model::event::metric::MetricValues;
use stringtheory::MetaString;

use super::*;
use crate::aggregate::{
    kernel::LimitLogs,
    lanes::LaneShared,
    tests::{arb_generated_metric_named, GeneratedMetric},
    PassProgress, RoomToAdmit,
};

/// One context a call.
fn one_context() -> Budget {
    Budget::unlimited().with_limit(WorkKind::StateWork, 1)
}

impl Host {
    /// Runs one call of a flush tick at `watermark` that visits at most one context.
    fn step(&mut self, watermark: AdmissionTime) -> Progress {
        self.call(Some(watermark), one_context(), None, |kernel, turn| {
            kernel.advance_time(turn)
        })
    }

    /// Runs the pending flush tick, if any, to completion, one context a call.
    fn complete(&mut self, watermark: AdmissionTime) {
        let mut calls = 0;
        while self.kernel.has_pending_work() {
            self.step(watermark);
            calls += 1;
            assert!(calls < 10_000, "a pending flush makes progress");
        }
    }

    /// Asks the kernel whether `contexts` new contexts fit, with the turn's parameters.
    fn room(&mut self, contexts: usize) -> bool {
        let mut room = RoomToAdmit { contexts, fits: false };
        let progress = self.call(None, Budget::unlimited(), None, |kernel, turn| {
            kernel.control(turn, &mut room)
        });
        assert_eq!(progress, Progress::Done, "a read-only question never yields");
        room.fits
    }

    /// Asks the kernel how far its pending flush has come.
    fn progress(&mut self) -> PassProgress {
        let mut progress = PassProgress {
            visited: usize::MAX,
            total: usize::MAX,
        };
        self.call(None, Budget::unlimited(), None, |kernel, turn| {
            kernel.control(turn, &mut progress)
        });
        progress
    }

    /// Admits a batch with a unit on every metric.
    fn admit_with_unit(&mut self, admitted_at: AdmissionTime, metrics: Vec<Metric>, unit: &'static str) {
        let metrics = metrics
            .into_iter()
            .map(|metric| {
                let (context, values, metadata) = metric.into_parts();
                Metric::from_parts(context, values, metadata.with_unit(MetaString::from(unit)))
            })
            .collect();
        self.admit(admitted_at, metrics);
    }
}

/// The one-shot and the interleaved host of one case, and what each emitted since the last comparison.
struct Pair {
    oneshot: Host,
    interleaved: Host,
    emitted: (Vec<Metric>, Vec<Metric>),
}

impl Pair {
    fn new(params: AggregateParams) -> Self {
        Self {
            oneshot: Host::new(params.clone()),
            interleaved: Host::new(params),
            emitted: (Vec::new(), Vec::new()),
        }
    }

    fn collect(&mut self) {
        self.emitted.0.extend(self.oneshot.emitted());
        self.emitted.1.extend(self.interleaved.emitted());
    }

    /// Flushes at `watermark`: the one-shot host completes the flush, and the interleaved host runs its first step,
    /// which visits one context, and returns what that step emitted.
    fn flush(&mut self, watermark: AdmissionTime) -> Vec<Metric> {
        self.oneshot
            .whole(Some(watermark), |kernel, turn| kernel.advance_time(turn));
        self.emitted.0.extend(self.oneshot.emitted());
        assert_eq!(
            self.interleaved.step(watermark),
            Progress::Yielded,
            "the flush visits more than one context"
        );
        let emitted = self.interleaved.emitted();
        self.emitted.1.extend(emitted.clone());
        emitted
    }

    /// Admits a batch to both hosts, and returns what the interleaved host's kernel wrote while admitting it.
    fn admit(&mut self, admitted_at: AdmissionTime, metrics: Vec<Metric>) -> Vec<Metric> {
        self.oneshot.admit(admitted_at, metrics.clone());
        self.interleaved.admit(admitted_at, metrics);
        self.emitted.0.extend(self.oneshot.emitted());
        let written = self.interleaved.emitted();
        self.emitted.1.extend(written.clone());
        written
    }

    /// Completes the interleaved host's pending flush, and returns what completing it emitted.
    fn complete(&mut self, watermark: AdmissionTime) -> Vec<Metric> {
        self.interleaved.complete(watermark);
        let emitted = self.interleaved.emitted();
        self.emitted.1.extend(emitted.clone());
        emitted
    }

    /// Ticks both hosts at `watermark`, completing each flush in one call.
    fn tick(&mut self, watermark: AdmissionTime) {
        for host in [&mut self.oneshot, &mut self.interleaved] {
            host.whole(Some(watermark), |kernel, turn| kernel.advance_time(turn));
        }
        self.collect();
    }

    /// Checks that the hosts agree, with no flush pending in either, and forgets what they emitted.
    fn assert_same(&mut self) {
        self.collect();
        let (oneshot, interleaved) = (&mut self.oneshot, &mut self.interleaved);
        assert!(!oneshot.kernel.has_pending_work() && !interleaved.kernel.has_pending_work());
        assert_eq!(multiset(&self.emitted.0), multiset(&self.emitted.1), "output");
        self.emitted.0.clear();
        self.emitted.1.clear();
        assert_eq!(oneshot.counters, interleaved.counters, "losses");
        assert_eq!(oneshot.kernel.held(), interleaved.kernel.held(), "held");
        assert_eq!(
            snapshot_set(oneshot.snapshot().entries),
            snapshot_set(interleaved.snapshot().entries),
            "snapshot"
        );
        let (expected, actual) = (oneshot.kernel.core(), interleaved.kernel.core());
        assert_eq!(
            expected.context_limit_breached(),
            actual.context_limit_breached(),
            "breach"
        );
        assert_eq!(expected.last_flush(), actual.last_flush(), "last flush");
        assert_eq!(
            expected.budget().global.used(),
            actual.budget().global.used(),
            "global budget"
        );
        assert_eq!(
            expected.budget().node.used(),
            actual.budget().node.used(),
            "node budget"
        );
        assert_eq!(
            oneshot.kernel.limit_logs(),
            interleaved.kernel.limit_logs(),
            "limit log lines"
        );
        assert_eq!(
            oneshot.kernel.take_suppressed(),
            interleaved.kernel.take_suppressed(),
            "suppressed"
        );
    }
}

fn names(metrics: &[Metric]) -> Vec<String> {
    let mut names = metrics
        .iter()
        .map(|metric| metric.context().name().to_string())
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn a_kernel_off_lanes_admits_while_advancing_and_one_on_lanes_does_not() {
    let host = Host::new(params(10, None));
    assert!(host.kernel.admits_while_advancing());

    let lanes = Arc::new(LaneShared::new(2));
    let mut on_lanes = Host::new(params(10, None));
    on_lanes.kernel = AggregateKernel::new(
        params(10, None),
        Duration::from_secs(15),
        ContextBudget::exclusive(),
        AggregateTelemetry::noop(),
    )
    .with_lanes(lanes);
    assert!(!on_lanes.kernel.admits_while_advancing());
    on_lanes.admit(at(1, 0), vec![Metric::gauge("a", 1.0)]);
    assert!(!on_lanes.room(1), "nothing fits on lanes");
}

#[test]
fn a_counter_admitted_during_its_own_buckets_flush_is_flushed_first_on_touch() {
    // Three counters in [B, B+10), flushed at B+10.5 one context a step. After the first step, a batch adds to all
    // three in [B+10, B+20): each context the flush has not reached is flushed on touch, by the batch, and its point
    // for [B, B+10) leaves with the batch's call, before the new point lands in the open bucket.
    let mut pair = Pair::new(params(10, Some(300)));
    pair.admit(
        at(1, 0),
        vec![
            Metric::counter("a", 1.0),
            Metric::counter("b", 1.0),
            Metric::counter("c", 1.0),
        ],
    );
    let first = pair.flush(at(10, 500));
    assert_eq!(first.len(), 1, "the first step visits one context");
    assert_eq!(pair.interleaved.progress(), PassProgress { visited: 1, total: 3 });

    let written = pair.admit(
        at(11, 0),
        vec![
            Metric::counter("a", 2.0),
            Metric::counter("b", 2.0),
            Metric::counter("c", 2.0),
        ],
    );
    assert_eq!(
        written.len(),
        2,
        "the two contexts the flush had not reached are flushed on touch"
    );
    let mut touched = names(&first);
    touched.extend(names(&written));
    touched.sort();
    assert_eq!(touched, vec!["a", "b", "c"]);
    assert_eq!(pair.interleaved.progress(), PassProgress { visited: 3, total: 3 });
    assert!(
        pair.complete(at(10, 500)).is_empty(),
        "nothing is left for the flush's steps"
    );
    assert_eq!(
        multiset(&pair.emitted.1),
        multiset(&[rate("a", &[(0, 1.0)]), rate("b", &[(0, 1.0)]), rate("c", &[(0, 1.0)])])
    );
    pair.assert_same();

    // The new points were not part of the flush: the next one emits them.
    pair.tick(at(20, 0));
    assert_eq!(
        multiset(&pair.emitted.1),
        multiset(&[
            rate("a", &[(10, 2.0)]),
            rate("b", &[(10, 2.0)]),
            rate("c", &[(10, 2.0)])
        ])
    );
    pair.assert_same();
}

#[test]
fn idle_counter_zeros_are_taken_at_each_visit_with_the_last_update_the_context_had_before() {
    // Counters last updated at B+1, with an expiry of 20 s, flushed at B+10 and then idle until the flush at B+40: it
    // adds zeros at B+10 and B+20, the buckets before their expiry at B+21, and expires them. A batch at B+41 during
    // that flush must not move their expiry before their visit: a context it reaches first takes the zeros of its old
    // last update, is removed, and the batch creates it again.
    let mut pair = Pair::new(params(10, Some(20)));
    let names_ = ["a", "b", "c", "d"];
    pair.admit(
        at(1, 0),
        names_.iter().map(|name| Metric::counter(*name, 1.0)).collect(),
    );
    pair.tick(at(10, 0));
    pair.assert_same();

    pair.flush(at(40, 0));
    pair.admit(
        at(41, 0),
        names_.iter().map(|name| Metric::counter(*name, 5.0)).collect(),
    );
    pair.complete(at(40, 0));
    let zeros = names_
        .iter()
        .map(|name| rate(name, &[(10, 0.0), (20, 0.0)]))
        .collect::<Vec<_>>();
    assert_eq!(multiset(&pair.emitted.1), multiset(&zeros));
    pair.assert_same();

    // The batch created the counters again, with no zeros before their new points.
    pair.tick(at(50, 0));
    let next = names_.iter().map(|name| rate(name, &[(40, 5.0)])).collect::<Vec<_>>();
    assert_eq!(multiset(&pair.emitted.1), multiset(&next));
    pair.assert_same();
}

#[test]
fn an_expired_counter_reached_before_its_visit_is_removed_and_created_again_with_its_new_metadata() {
    let mut pair = Pair::new(params(10, Some(10)));
    for host in [&mut pair.oneshot, &mut pair.interleaved] {
        host.admit_with_unit(
            at(1, 0),
            vec![Metric::counter("a", 1.0), Metric::counter("b", 1.0)],
            "old",
        );
    }
    pair.tick(at(10, 0));
    pair.assert_same();

    // At B+30 both counters are idle past their expiry, empty after their zeros, and removed.
    pair.flush(at(30, 0));
    for host in [&mut pair.oneshot, &mut pair.interleaved] {
        host.admit_with_unit(
            at(31, 0),
            vec![Metric::counter("a", 1.0), Metric::counter("b", 1.0)],
            "new",
        );
    }
    pair.complete(at(30, 0));
    let units = pair
        .interleaved
        .snapshot()
        .entries
        .iter()
        .map(|entry| entry.unit().map(str::to_string))
        .collect::<Vec<_>>();
    assert_eq!(
        units,
        vec![Some("new".to_string()); 2],
        "each counter is created again by the batch"
    );
    pair.assert_same();
}

#[test]
fn gauges_and_sets_whose_buckets_all_closed_are_removed_at_the_visit_and_created_again() {
    let mut pair = Pair::new(params(10, None));
    let set = |member: &'static str| Metric::set("s", member);
    for host in [&mut pair.oneshot, &mut pair.interleaved] {
        host.admit_with_unit(
            at(1, 0),
            vec![Metric::gauge("g", 1.0), set("x"), Metric::gauge("h", 3.0)],
            "old",
        );
    }
    pair.flush(at(10, 0));
    for host in [&mut pair.oneshot, &mut pair.interleaved] {
        host.admit_with_unit(
            at(12, 0),
            vec![Metric::gauge("g", 2.0), set("y"), Metric::gauge("h", 4.0)],
            "new",
        );
    }
    pair.complete(at(10, 0));
    assert_eq!(pair.interleaved.snapshot().entries.len(), 3);
    assert!(
        pair.interleaved
            .snapshot()
            .entries
            .iter()
            .all(|entry| entry.unit() == Some("new")),
        "each context was created again by the batch"
    );
    pair.assert_same();

    pair.tick(at(20, 0));
    pair.assert_same();
    assert!(pair.interleaved.kernel.core().is_empty());
}

#[test]
fn histograms_and_distributions_emit_their_closed_buckets_at_the_visit_and_merge_later_samples_afterward() {
    // Samples whose sums depend on the order they merge in: the open bucket's samples merge after the visit, in arrival
    // order, so every statistic, and every sketch, is the one-shot host's.
    let mut pair = Pair::new(params(10, None));
    let histogram = |name: &'static str, samples: &[f64]| Metric::histogram(name, samples);
    let distribution = |name: &'static str, samples: &[f64]| Metric::distribution(name, samples);
    pair.admit(
        at(1, 0),
        vec![
            histogram("h", &[0.1, 0.2]),
            distribution("d", &[0.3, 1e9]),
            histogram("k", &[7.0]),
        ],
    );
    pair.admit(at(12, 0), vec![histogram("h", &[0.7])]);
    pair.flush(at(10, 0));
    let mut late = histogram("h", &[0.3, 1e-9]);
    let mut closed = MetricValues::histogram([5.0]);
    closed.set_timestamp(B + 4);
    late.values_mut().merge(closed);
    pair.admit(
        at(13, 0),
        vec![late, distribution("d", &[0.5]), histogram("k", &[1.0, 2.0])],
    );
    pair.complete(at(10, 0));
    pair.assert_same();
    // The late sample in the closed bucket [B, B+10) waited for the next flush, as after a completed flush.
    pair.tick(at(20, 0));
    assert!(pair.emitted.1.iter().any(|metric| {
        metric.context().name() == "h.max"
            && matches!(metric.values(), MetricValues::Gauge(points) if points.into_iter().any(|(ts, value)| ts.map(|ts| ts.get()) == Some(B + 4) && value == 5.0))
    }));
    pair.assert_same();
}

#[test]
fn a_kind_conflict_during_a_flush_emits_the_closed_points_first_and_replaces_only_the_open_ones() {
    // `x` holds a gauge point in [B, B+10), closed by the flush at B+10.5, and one at B+15, open. A counter for `x`
    // during the flush replaces only the open point, after the closed one leaves.
    let mut pair = Pair::new(params(10, None));
    let metrics = || {
        vec![
            Metric::gauge("x", 1.0),
            gauge("x", &[(15, 2.0)]),
            Metric::gauge("y", 1.0),
            gauge("y", &[(15, 2.0)]),
        ]
    };
    pair.admit(at(1, 0), metrics());
    pair.flush(at(10, 500));
    pair.admit(at(11, 0), vec![Metric::counter("x", 3.0), Metric::counter("y", 3.0)]);
    pair.complete(at(10, 500));
    assert_eq!(
        multiset(&pair.emitted.1),
        multiset(&[gauge("x", &[(0, 1.0)]), gauge("y", &[(0, 1.0)])])
    );
    assert_eq!(
        pair.interleaved.counters.loss(LossClass::Policy),
        LossCount { events: 2, points: 2 },
        "only the open points are replaced"
    );
    pair.assert_same();
    pair.tick(at(20, 0));
    pair.assert_same();
}

#[test]
fn timestamped_values_in_closed_buckets_admitted_during_a_flush_wait_for_the_next_one() {
    let mut pair = Pair::new(params(10, None));
    pair.admit(
        at(1, 0),
        vec![
            Metric::gauge("a", 1.0),
            Metric::gauge("b", 1.0),
            gauge("c", &[(12, 1.0)]),
        ],
    );
    pair.flush(at(10, 0));
    // Points at B+3, closed by the flush, for held contexts and a new one.
    pair.admit(
        at(11, 0),
        vec![
            gauge("a", &[(3, 5.0)]),
            gauge("b", &[(3, 5.0)]),
            gauge("c", &[(3, 5.0)]),
            gauge("n", &[(3, 5.0)]),
        ],
    );
    pair.complete(at(10, 0));
    assert_eq!(
        names(&pair.emitted.1),
        vec!["a", "b"],
        "the flush emits what it held when it started"
    );
    pair.assert_same();
    pair.tick(at(20, 0));
    assert_eq!(
        multiset(&pair.emitted.1),
        multiset(&[
            gauge("a", &[(3, 5.0)]),
            gauge("b", &[(3, 5.0)]),
            gauge("c", &[(3, 5.0), (12, 1.0)]),
            gauge("n", &[(3, 5.0)]),
        ])
    );
    pair.assert_same();
}

#[test]
fn a_context_created_during_a_flush_is_never_visited_by_it() {
    // Five gauges in a table of eight buckets, which has room for two more without growing.
    let mut pair = Pair::new(params(10, None));
    let held = ["a", "b", "c", "d", "e"];
    pair.admit(at(1, 0), held.iter().map(|name| Metric::gauge(*name, 1.0)).collect());
    pair.flush(at(10, 0));
    let before = pair.interleaved.progress();
    assert_eq!(before, PassProgress { visited: 1, total: 5 });
    // New contexts, with points the flush would close, are not part of it.
    for (n, name) in [(0, "n0"), (1, "n1")] {
        assert!(pair.interleaved.room(1));
        let written = pair.admit(at(11, 0), vec![gauge(name, &[(n, 1.0)]), gauge(name, &[(n + 5, 2.0)])]);
        assert!(written.is_empty(), "admitting new contexts visits nothing");
        assert!(pair.interleaved.kernel.has_pass_in_progress());
    }
    assert_eq!(pair.interleaved.progress(), before);
    pair.complete(at(10, 0));
    assert_eq!(names(&pair.emitted.1), held);
    assert_eq!(
        pair.interleaved.kernel.take_last_completed(),
        Some(PassProgress { visited: 5, total: 5 })
    );
    pair.assert_same();
    pair.tick(at(20, 0));
    assert_eq!(
        names(&pair.emitted.1),
        vec!["n0", "n1"],
        "the next flush emits the new contexts' points"
    );
    pair.assert_same();
}

#[test]
fn no_admission_is_refused_while_a_flush_is_pending_and_its_end_clears_the_breach_as_native_does() {
    // At a limit of two, `a` (a gauge the flush removes) and `k` (a counter kept forever) fill the state, and `d` is
    // refused, which sets the breach flag. During the flush, `e` is admitted once the flush has removed `a`. The flush
    // ends holding `k` and `e`, two contexts, but would have ended holding only `k` had it completed first: it judges
    // the breach without `e`, and clears it, as the one-shot host does.
    let mut pair = Pair::new(params(2, None));
    pair.admit(
        at(1, 0),
        vec![
            Metric::gauge("a", 1.0),
            Metric::counter("k", 1.0),
            Metric::gauge("d", 1.0),
        ],
    );
    assert!(pair.interleaved.kernel.core().context_limit_breached());
    pair.flush(at(10, 0));
    if pair.interleaved.kernel.core().len() == 2 {
        // The first step visited `k`: `a` is removed on touch, by its own metric, which creates it again.
        pair.admit(at(11, 0), vec![Metric::gauge("a", 2.0)]);
        assert!(pair.interleaved.kernel.has_pass_in_progress());
    } else {
        pair.admit(at(11, 0), vec![Metric::gauge("e", 2.0)]);
        assert!(
            pair.interleaved.kernel.has_pass_in_progress(),
            "the new context took the unit `a` released"
        );
    }
    assert!(
        pair.interleaved.kernel.core().context_limit_breached(),
        "only the end of the flush clears the breach"
    );
    pair.complete(at(10, 0));
    assert!(!pair.interleaved.kernel.core().context_limit_breached());
    assert_eq!(
        pair.interleaved.kernel.limit_logs(),
        LimitLogs {
            reached: 1,
            recovered: 1
        }
    );
    pair.assert_same();
}

#[test]
fn a_new_context_the_limit_would_refuse_during_a_flush_completes_the_flush_and_is_admitted_as_after_it() {
    // At a limit of two, `a` and `k` fill the state and `d` is refused. During the flush, a batch for `a`, which the
    // flush removes and the batch creates again, and for `e`, which the limit would refuse while the flush is pending.
    // Admission completes the flush, whose end clears the breach since the state, without the `a` the batch created,
    // is below the limit, and then refuses `e`, which starts a new breach: both log lines, as the one-shot host logs
    // them.
    let mut pair = Pair::new(params(2, None));
    pair.admit(
        at(1, 0),
        vec![
            Metric::gauge("a", 1.0),
            Metric::counter("k", 1.0),
            Metric::gauge("d", 1.0),
        ],
    );
    let first = pair.flush(at(10, 0));
    let written = pair.admit(at(11, 0), vec![Metric::gauge("a", 2.0), Metric::gauge("e", 2.0)]);
    let mut all = first;
    all.extend(written);
    assert_eq!(names(&all), vec!["a", "k"], "the batch completed the flush");
    assert!(!pair.interleaved.kernel.has_pass_in_progress());
    assert!(
        pair.interleaved.kernel.has_pending_work(),
        "the flush tick waits for its host to resume it"
    );
    assert_eq!(pair.interleaved.progress(), PassProgress { visited: 2, total: 2 });
    assert_eq!(
        pair.interleaved.kernel.take_last_completed(),
        Some(PassProgress { visited: 2, total: 2 })
    );

    // Resuming the flush tick returns at once, without flushing again.
    assert_eq!(pair.interleaved.step(at(10, 0)), Progress::Done);
    assert!(pair.interleaved.emitted().is_empty());
    assert_eq!(pair.interleaved.progress(), PassProgress::default());
    assert_eq!(
        pair.interleaved.counters.loss(LossClass::Capacity),
        LossCount { events: 2, points: 2 },
        "`d` before the flush, and `e` after it"
    );
    assert_eq!(
        pair.interleaved.kernel.limit_logs(),
        LimitLogs {
            reached: 2,
            recovered: 1
        }
    );
    pair.assert_same();
    assert_eq!(pair.interleaved.kernel.core().last_flush(), B + 10);
}

#[test]
fn a_new_context_that_needs_the_table_to_grow_during_a_flush_completes_the_flush_first() {
    // Three counters kept forever fill a table of four buckets, which holds three entries without growing. A new
    // context during the flush would grow the table, moving every entry the flush has yet to visit: admission completes
    // the flush first, and only then inserts it.
    let mut pair = Pair::new(params(1_000, None));
    pair.admit(
        at(1, 0),
        vec![
            Metric::counter("a", 1.0),
            Metric::counter("b", 1.0),
            Metric::counter("c", 1.0),
        ],
    );
    let (buckets, capacity) = pair.interleaved.kernel.core().table_geometry();
    assert_eq!((buckets, capacity), (4, 3));
    pair.flush(at(10, 0));
    assert!(!pair.interleaved.room(1), "a new context needs the table to grow");
    pair.admit(at(11, 0), vec![Metric::counter("a", 1.0)]);
    assert!(
        pair.interleaved.kernel.has_pass_in_progress(),
        "a held context needs no room"
    );
    assert_eq!(pair.interleaved.kernel.core().table_geometry(), (buckets, capacity));
    pair.admit(at(11, 0), vec![Metric::counter("n", 1.0)]);
    assert!(
        !pair.interleaved.kernel.has_pass_in_progress(),
        "the new context completed the flush"
    );
    assert!(pair.interleaved.kernel.core().table_geometry().1 > capacity);
    assert_eq!(
        pair.interleaved.kernel.take_last_completed(),
        Some(PassProgress { visited: 3, total: 3 })
    );
    pair.complete(at(10, 0));
    pair.assert_same();
}

#[test]
fn room_to_admit_fits_only_without_growing_the_table_and_strictly_below_every_limit() {
    // A table that holds five contexts has eight buckets and room for seven.
    let five = || {
        ["a", "b", "c", "d", "e"]
            .map(|name| Metric::counter(name, 1.0))
            .to_vec()
    };
    let mut host = Host::new(params(1_000, None));
    host.admit(at(1, 0), five());
    assert_eq!(host.kernel.core().table_geometry(), (8, 7));
    assert!(host.room(0) && host.room(2));
    assert!(!host.room(3), "the table would grow");

    // Strictly below the context limit of the turn's parameters.
    let mut limited = Host::new(params(7, None));
    limited.admit(at(1, 0), five());
    assert!(limited.room(1));
    assert!(!limited.room(2), "seven contexts are not below a limit of seven");
    let mut step = limited.kernel.defaults().clone();
    step.admit = AdmitParams { context_limit: 6 };
    let mut room = RoomToAdmit {
        contexts: 1,
        fits: true,
    };
    limited.call(None, Budget::unlimited(), Some(&step), |kernel, turn| {
        kernel.control(turn, &mut room)
    });
    assert!(!room.fits, "the turn's limit applies");

    // Strictly below the shared global budget's capacity, which other kernels use too.
    let global = StateBudget::new(8);
    let budget = || ContextBudget {
        global: global.clone(),
        node: StateBudget::unlimited(),
    };
    let mut shared = Host::with(params(1_000, None), budget(), AggregateTelemetry::noop());
    shared.admit(at(1, 0), five());
    assert!(shared.room(2));
    let mut other = Host::with(params(1_000, None), budget(), AggregateTelemetry::noop());
    other.admit(at(1, 0), vec![Metric::gauge("z", 1.0)]);
    assert!(shared.room(1));
    assert!(!shared.room(2), "eight units are not below a capacity of eight");

    // While a flush is pending, a removal at the visit makes room in the budget, never in the table.
    let mut pending = Host::new(params(6, None));
    pending.admit(
        at(1, 0),
        five()
            .into_iter()
            .map(|metric| Metric::gauge(metric.context().clone(), 1.0))
            .collect(),
    );
    assert!(!pending.room(1));
    assert_eq!(pending.step(at(10, 0)), Progress::Yielded);
    assert!(pending.room(1), "the visited gauge was removed and gave back its unit");
    assert!(!pending.room(2));
    assert!(
        pending.kernel.has_pass_in_progress(),
        "asking leaves the flush in place"
    );
}

#[test]
fn pass_progress_counts_visits_by_the_flushs_steps_and_on_touch() {
    let mut host = Host::new(params(1_000, Some(300)));
    assert_eq!(host.progress(), PassProgress::default(), "no flush is pending");
    host.admit(
        at(1, 0),
        ["a", "b", "c", "d", "e"]
            .map(|name| Metric::counter(name, 1.0))
            .to_vec(),
    );
    assert_eq!(host.step(at(10, 0)), Progress::Yielded);
    assert_eq!(host.progress(), PassProgress { visited: 1, total: 5 });

    // Touching every context visits those not visited yet, once; touching them again, or creating new ones, does not.
    for _ in 0..2 {
        host.admit(
            at(11, 0),
            ["a", "b", "c", "d", "e", "f"]
                .map(|name| Metric::counter(name, 1.0))
                .to_vec(),
        );
        assert_eq!(host.progress(), PassProgress { visited: 5, total: 5 });
    }
    assert_eq!(
        host.step(at(10, 0)),
        Progress::Done,
        "the flush finds nothing left to visit"
    );
    assert_eq!(host.progress(), PassProgress::default());
    assert_eq!(
        host.kernel.take_last_completed(),
        Some(PassProgress { visited: 5, total: 5 })
    );
    assert_eq!(host.emitted().len(), 5);
}

#[test]
fn touch_visits_apply_the_flushs_parameters_and_count_what_they_skip_when_the_batch_returns() {
    // The flush starts with a filter that drops `a` and `b` and histograms expanded to their count only; the batch
    // during it carries parameters without either. Visits on touch apply the flush's.
    let mut pair = Pair::new(params(10, None));
    let mut flush_step = pair.oneshot.kernel.defaults().clone();
    flush_step.flush.filter = Some(Arc::new(TestFilter::names(&["a", "b"])));
    flush_step.flush.histograms =
        HistogramConfiguration::try_new(&["count".to_string()], &[], false, String::new()).unwrap();
    let batch_step = pair.oneshot.kernel.defaults().clone();
    let metrics = || {
        vec![
            Metric::gauge("a", 1.0),
            Metric::gauge("b", 1.0),
            Metric::gauge("c", 1.0),
            Metric::histogram("h", [1.0]),
        ]
    };
    for host in [&mut pair.oneshot, &mut pair.interleaved] {
        host.admit(at(1, 0), metrics());
    }
    pair.oneshot.call(
        Some(at(10, 0)),
        Budget::unlimited(),
        Some(&flush_step),
        |kernel, turn| assert_eq!(kernel.advance_time(turn), Progress::Done),
    );
    let progress = pair
        .interleaved
        .call(Some(at(10, 0)), one_context(), Some(&flush_step), |kernel, turn| {
            kernel.advance_time(turn)
        });
    assert_eq!(progress, Progress::Yielded);
    let taken_after_step = pair.interleaved.kernel.take_suppressed();
    for host in [&mut pair.oneshot, &mut pair.interleaved] {
        host.consume(
            Some(at(11, 0)),
            metrics().into_iter().map(Event::Metric).collect(),
            Some(&batch_step),
        );
    }
    let taken_after_batch = pair.interleaved.kernel.take_suppressed();
    let suppressed = taken_after_step
        .iter()
        .chain(taken_after_batch.iter())
        .map(|(_, suppressed)| suppressed.records)
        .sum::<u64>();
    assert_eq!(suppressed, 2, "`a` and `b` were skipped, whichever visit reached them");
    assert!(pair.interleaved.kernel.take_suppressed().is_empty());
    pair.interleaved.call(
        Some(at(10, 0)),
        Budget::unlimited(),
        Some(&flush_step),
        |kernel, turn| assert_eq!(kernel.advance_time(turn), Progress::Done),
    );
    pair.collect();
    assert_eq!(
        names(&pair.emitted.1),
        vec!["c", "h.count"],
        "the flush's histogram statistics"
    );
    // Both hosts skipped the same records; the counts were taken from the interleaved one above.
    let oneshot_suppressed = pair.oneshot.kernel.take_suppressed();
    assert_eq!(
        oneshot_suppressed
            .iter()
            .map(|(_, suppressed)| suppressed.records)
            .sum::<u64>(),
        2
    );
    pair.assert_same();
}

#[test]
fn the_table_never_rehashes_while_a_flush_is_pending() {
    // Six gauges in a table of eight buckets, flushed one a step. Each step removes a gauge in place; new contexts
    // admitted between steps take the room the table has without growing, and every gauge the flush has yet to visit
    // stays where it was when the flush started.
    let mut host = Host::new(params(1_000, None));
    host.admit(
        at(1, 0),
        (0..6)
            .map(|n| Metric::gauge(["a", "b", "c", "d", "e", "f"][n], 1.0))
            .collect(),
    );
    let geometry = host.kernel.core().table_geometry();
    assert_eq!(geometry, (8, 7));
    let started = host
        .kernel
        .core()
        .positions()
        .into_iter()
        .map(|(position, context, _)| (context, position))
        .collect::<HashMap<_, _>>();
    assert_eq!(host.step(at(10, 0)), Progress::Yielded);
    let mut created = 0;
    for name in ["n0", "n1", "n2", "n3"] {
        if host.room(1) {
            host.admit(at(11, 0), vec![Metric::gauge(name, 1.0)]);
            created += 1;
            assert!(host.kernel.has_pass_in_progress());
            assert_eq!(host.kernel.core().table_geometry().0, geometry.0, "no rehash");
            for (position, context, visited) in host.kernel.core().positions() {
                assert!(visited || started.get(&context) == Some(&position), "{} moved", context);
            }
        }
        if host.step(at(10, 0)) == Progress::Done {
            break;
        }
    }
    assert!(created >= 2);
    host.complete(at(10, 0));
    assert_eq!(names(&host.emitted()), vec!["a", "b", "c", "d", "e", "f"]);
}

#[test]
fn passes_are_numbered_from_one_again_when_their_numbers_run_out() {
    let mut pair = Pair::new(params(10, None));
    for host in [&mut pair.oneshot, &mut pair.interleaved] {
        host.kernel.core_mut().set_passes(u32::MAX - 1);
    }
    for round in 0..4u64 {
        let base = round * 10;
        pair.admit(
            at(base + 1, 0),
            vec![
                Metric::gauge("a", 1.0),
                Metric::counter("k", 1.0),
                Metric::gauge("b", 1.0),
            ],
        );
        pair.flush(at(base + 10, 0));
        pair.admit(
            at(base + 11, 0),
            vec![
                Metric::gauge("a", 2.0),
                Metric::counter("k", 2.0),
                Metric::gauge("n", 1.0),
            ],
        );
        pair.complete(at(base + 10, 0));
        pair.assert_same();
    }
}

// A generated run, with flushes that interleave with the batches that follow them.

/// The names of generated metrics: with their tags, up to 32 contexts.
const NAMES: &[&str] = &["a", "b", "c", "d", "e", "f", "g", "h"];

/// One step of a generated run: a batch, a flush tick, or a clock reset, each some seconds after the step before it.
/// After a reset, the clock continues `after` seconds away, earlier or later.
#[derive(Clone, Debug)]
enum RunStep {
    Admit { after: i64, metrics: Vec<GeneratedMetric> },
    Tick { after: i64 },
    Reset { after: i64 },
}

/// A run of mostly batches, so that several arrive during each flush, over up to 32 contexts of every kind.
fn arb_run() -> impl Strategy<Value = Vec<RunStep>> {
    let after = prop_oneof![
        8 => 0i64..20,
        2 => 20i64..120,
        1 => 120i64..1200,
        1 => -30i64..0,
    ];
    let reset_after = prop_oneof![
        2 => -1200i64..-300,
        1 => Just(0i64),
        2 => 1i64..1200,
    ];
    let step = prop_oneof![
        24 => (after.clone(), vec(arb_generated_metric_named(NAMES), 0..12))
            .prop_map(|(after, metrics)| RunStep::Admit { after, metrics }),
        6 => after.prop_map(|after| RunStep::Tick { after }),
        1 => reset_after.prop_map(|after| RunStep::Reset { after }),
    ];
    vec(step, 1..80)
}

/// A context limit for a generated run: none admitted, a few, about as many as a run creates, or plenty.
fn arb_context_limit() -> impl Strategy<Value = usize> {
    prop_oneof![1 => Just(0usize), 2 => 1usize..8, 4 => 8usize..40, 3 => Just(1000usize)]
}

/// How the interleaved host proceeds while a flush is pending, before the next batch: how many steps it runs first,
/// and each step's budget.
#[derive(Clone, Debug)]
struct Pace {
    steps_before: u8,
    state_work: u64,
    events: u64,
}

fn arb_pace() -> impl Strategy<Value = Vec<Pace>> {
    let pace = (
        prop_oneof![6 => Just(0u8), 3 => Just(1u8), 1 => 2u8..5],
        prop_oneof![4 => Just(1u64), 1 => 2u64..6],
        1u64..24,
    )
        .prop_map(|(steps_before, state_work, events)| Pace {
            steps_before,
            state_work,
            events,
        });
    vec(pace, 1..12)
}

/// Generated per-call parameters: the step parameters of two revisions of the program that share the kernel's state.
fn step_params(
    generated: &crate::aggregate::tests::GeneratedParams, filter: Option<TestFilter>,
) -> AggregateStepParams {
    let (_, admit, mut flush) = aggregate_params(generated).split();
    flush.filter = filter.map(|filter| Arc::new(filter) as _);
    AggregateStepParams {
        admit,
        flush,
        flush_interval: Duration::from_secs(15),
    }
}

/// The one-shot and the interleaved host of a generated run, with the recorders of their telemetry.
struct Run {
    oneshot: Host,
    interleaved: Host,
    oneshot_budget: ContextBudget,
    interleaved_budget: ContextBudget,
    recorders: (TestRecorder, TestRecorder),
    emitted: (Vec<Metric>, Vec<Metric>),
    /// The steps' budgets, cycled.
    paces: Vec<Pace>,
    next_pace: usize,
    /// Whether the interleaved host asks [`RoomToAdmit`] before it delivers a batch while a flush is pending, and holds
    /// the batch until the flush completes when it does not fit, as a region driver does.
    room_check: bool,
    /// The flush pending on the interleaved host: its watermark and parameters, the contexts the core held when it
    /// started, the table's geometry then, and the visits its last progress reported.
    pending: Option<PendingFlush>,
}

#[derive(Clone)]
struct PendingFlush {
    watermark: AdmissionTime,
    params: AggregateStepParams,
    total: usize,
    /// The number of buckets of the table when the flush started, and the position of every context it held.
    buckets: usize,
    positions: HashMap<Context, usize>,
    visited: usize,
}

impl Run {
    fn pace(&mut self) -> Pace {
        let pace = self.paces[self.next_pace % self.paces.len()].clone();
        self.next_pace += 1;
        pace
    }

    /// Runs one step of the interleaved host's pending flush, with the next pace's budget.
    fn step(&mut self) -> Result<(), TestCaseError> {
        let pending = self.pending.clone().expect("a flush is pending");
        let pace = self.pace();
        let budget = Budget::unlimited()
            .with_limit(WorkKind::StateWork, pace.state_work)
            .with_limit(WorkKind::Events, pace.events);
        let progress = self.interleaved.call(
            Some(pending.watermark),
            budget,
            Some(&pending.params),
            |kernel, turn| kernel.advance_time(turn),
        );
        self.emitted.1.extend(self.interleaved.emitted());
        if progress == Progress::Done {
            self.end_pending()?;
        } else {
            self.check_pending()?;
        }
        Ok(())
    }

    /// Checks the invariants of the pending flush: its progress, and a table that never rehashes.
    fn check_pending(&mut self) -> Result<(), TestCaseError> {
        let progress = self.interleaved.progress();
        let pending = self.pending.as_mut().expect("a flush is pending");
        prop_assert!(self.interleaved.kernel.has_pending_work());
        prop_assert_eq!(
            progress.total,
            pending.total,
            "the flush visits the contexts it started with"
        );
        prop_assert!(progress.visited >= pending.visited && progress.visited <= progress.total);
        pending.visited = progress.visited;
        if self.interleaved.kernel.has_pass_in_progress() {
            // Every context the flush has yet to visit is where it was when the flush started, so its steps find it,
            // and their number is what the flush's progress leaves.
            let core = self.interleaved.kernel.core();
            prop_assert_eq!(
                core.table_geometry().0,
                pending.buckets,
                "the table never grows during a flush"
            );
            let mut unvisited = 0;
            for (position, context, visited) in core.positions() {
                if !visited {
                    unvisited += 1;
                    prop_assert_eq!(pending.positions.get(&context), Some(&position), "{} moved", context);
                }
            }
            prop_assert_eq!(unvisited, progress.total - progress.visited);
        } else {
            prop_assert_eq!(
                progress.visited,
                progress.total,
                "a flush completed by a batch visited every context"
            );
        }
        prop_assert_eq!(
            self.interleaved_budget.global.used(),
            self.interleaved.kernel.core().len() as u64
        );
        Ok(())
    }

    /// Checks that a flush that just completed visited every context it started with, once.
    fn end_pending(&mut self) -> Result<(), TestCaseError> {
        let pending = self.pending.take().expect("a flush was pending");
        prop_assert!(!self.interleaved.kernel.has_pending_work());
        prop_assert_eq!(
            self.interleaved.kernel.take_last_completed(),
            Some(PassProgress {
                visited: pending.total,
                total: pending.total
            })
        );
        prop_assert_eq!(self.interleaved.progress(), PassProgress::default());
        Ok(())
    }

    /// Completes the interleaved host's pending flush, if any.
    fn complete(&mut self) -> Result<(), TestCaseError> {
        for _ in 0..100_000 {
            if self.pending.is_none() {
                return Ok(());
            }
            self.step()?;
        }
        Err(TestCaseError::fail("a pending flush made no progress"))
    }

    fn admit(
        &mut self, admitted_at: AdmissionTime, metrics: Vec<Metric>, params: &AggregateStepParams,
    ) -> Result<(), TestCaseError> {
        let events = || metrics.iter().cloned().map(Event::Metric).collect::<Vec<_>>();
        self.oneshot.consume(Some(admitted_at), events(), Some(params));
        self.emitted.0.extend(self.oneshot.emitted());

        if self.pending.is_some() {
            for _ in 0..self.pace().steps_before {
                if self.pending.is_some() {
                    self.step()?;
                }
            }
        }
        let mut fits = false;
        if self.pending.is_some() && self.room_check {
            let mut room = RoomToAdmit {
                contexts: metrics.len(),
                fits: false,
            };
            self.interleaved
                .call(None, Budget::unlimited(), Some(params), |kernel, turn| {
                    kernel.control(turn, &mut room)
                });
            fits = room.fits;
            if !fits {
                // The batch waits for the flush.
                self.complete()?;
            }
        }
        self.interleaved.consume(Some(admitted_at), events(), Some(params));
        self.emitted.1.extend(self.interleaved.emitted());
        if self.pending.is_some() {
            if fits {
                prop_assert!(
                    self.interleaved.kernel.has_pass_in_progress(),
                    "a batch that fits never completes the flush"
                );
            }
            self.check_pending()?;
        }
        Ok(())
    }

    /// Starts a flush tick at `watermark` on both hosts: the one-shot host completes it, and the interleaved host runs
    /// its first step.
    fn tick(&mut self, watermark: AdmissionTime, params: &AggregateStepParams) -> Result<(), TestCaseError> {
        self.oneshot
            .call(Some(watermark), Budget::unlimited(), Some(params), |kernel, turn| {
                assert_eq!(kernel.advance_time(turn), Progress::Done)
            });
        self.emitted.0.extend(self.oneshot.emitted());
        // What a reset's passes recorded is not this flush's.
        self.interleaved.kernel.take_last_completed();
        let core = self.interleaved.kernel.core();
        self.pending = Some(PendingFlush {
            watermark,
            params: params.clone(),
            total: core.len(),
            buckets: core.table_geometry().0,
            positions: core
                .positions()
                .into_iter()
                .map(|(position, context, _)| (context, position))
                .collect(),
            visited: 0,
        });
        if core.is_empty() {
            // An empty core does not flush.
            self.interleaved
                .call(Some(watermark), Budget::unlimited(), Some(params), |kernel, turn| {
                    assert_eq!(kernel.advance_time(turn), Progress::Done)
                });
            self.pending = None;
            prop_assert_eq!(self.interleaved.kernel.take_last_completed(), None);
            return Ok(());
        }
        self.step()
    }

    /// Checks that the hosts agree, with no flush pending.
    fn check(&mut self) -> Result<(), TestCaseError> {
        prop_assert!(self.pending.is_none());
        let (oneshot, interleaved) = (&mut self.oneshot, &mut self.interleaved);
        prop_assert!(!oneshot.kernel.has_pending_work() && !interleaved.kernel.has_pending_work());
        prop_assert_eq!(multiset(&self.emitted.0), multiset(&self.emitted.1), "output");
        self.emitted.0.clear();
        self.emitted.1.clear();
        prop_assert_eq!(oneshot.counters, interleaved.counters, "losses by class");
        prop_assert_eq!(oneshot.kernel.held(), interleaved.kernel.held(), "held");
        prop_assert_eq!(
            snapshot_set(oneshot.snapshot().entries),
            snapshot_set(interleaved.snapshot().entries),
            "snapshot"
        );
        let (expected, actual) = (oneshot.kernel.core(), interleaved.kernel.core());
        prop_assert_eq!(expected.len(), actual.len(), "contexts held");
        prop_assert_eq!(
            expected.context_limit_breached(),
            actual.context_limit_breached(),
            "breach flag"
        );
        prop_assert_eq!(expected.last_flush(), actual.last_flush(), "last flush");
        prop_assert_eq!(
            self.oneshot_budget.global.used(),
            self.interleaved_budget.global.used(),
            "global budget"
        );
        prop_assert_eq!(
            self.oneshot_budget.node.used(),
            self.interleaved_budget.node.used(),
            "node budget"
        );
        prop_assert_eq!(self.interleaved_budget.global.used(), actual.len() as u64);
        prop_assert_eq!(
            oneshot.kernel.limit_logs(),
            interleaved.kernel.limit_logs(),
            "limit log lines"
        );
        prop_assert_eq!(
            oneshot.kernel.take_suppressed(),
            interleaved.kernel.take_suppressed(),
            "suppressed"
        );
        prop_assert_eq!(
            telemetry_values(&self.recorders.0),
            telemetry_values(&self.recorders.1),
            "telemetry"
        );
        Ok(())
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn property_test_an_interleaved_flush_is_a_flush_before_its_batches(
        mut generated in arb_params(),
        mut other in arb_params(),
        limits in (arb_context_limit(), arb_context_limit()),
        start in 0u64..3600,
        steps in arb_run(),
        revisions in prop_oneof![1 => Just(vec![false]), 2 => vec(any::<bool>(), 1..40)],
        paces in arb_pace(),
        capacity in proptest::option::weighted(0.4, 0u64..48),
        filters in (
            proptest::option::weighted(0.3, crate::aggregate::tests::emission::arb_filter()),
            proptest::option::weighted(0.3, crate::aggregate::tests::emission::arb_filter()),
        ),
        room_check in any::<bool>(),
        retire in any::<bool>(),
    ) {
        // Two hosts see the same run, each with a global budget of its own of the same capacity, near the limit or
        // unlimited. Calls alternate at random between the parameters of two revisions of the program, context limits
        // and filters included, so that a flush and the batches admitted during it can carry different parameters:
        // what a flush emits on touch follows the flush's.
        generated.context_limit = limits.0;
        other.context_limit = limits.1;
        let params = aggregate_params(&generated);
        let revisions_params = [step_params(&generated, filters.0.clone()), step_params(&other, filters.1.clone())];
        let budget = || ContextBudget {
            global: capacity.map_or_else(StateBudget::unlimited, StateBudget::new),
            node: StateBudget::unlimited(),
        };
        let (oneshot_budget, interleaved_budget) = (budget(), budget());
        let recorders = (TestRecorder::default(), TestRecorder::default());
        let host = |budget: &ContextBudget, recorder: &TestRecorder| {
            let _local = metrics::set_default_local_recorder(recorder);
            Host::with(params.clone(), budget.clone(), AggregateTelemetry::new(&MetricsBuilder::default()))
        };
        let mut run = Run {
            oneshot: host(&oneshot_budget, &recorders.0),
            interleaved: host(&interleaved_budget, &recorders.1),
            oneshot_budget,
            interleaved_budget,
            recorders,
            emitted: (Vec::new(), Vec::new()),
            paces,
            next_pace: 0,
            room_check,
            pending: None,
        };

        // Times move as the steps say, earlier as well as later, and batches are admitted at any time, before a flush's
        // watermark included: the kernel's guarantee holds whatever the times.
        let time = |secs: u64, millis: u64| AdmissionTime::from_unix_nanos(secs * 1_000_000_000 + millis * 1_000_000);
        let mut now = 1_700_000_000 + start;
        for (position, step) in steps.iter().enumerate() {
            let params = &revisions_params[usize::from(revisions[position % revisions.len()])];
            match step {
                RunStep::Admit { after, metrics } => {
                    now = now.saturating_add_signed(*after);
                    let metrics = metrics.iter().map(|metric| metric.build(now)).collect::<Vec<_>>();
                    run.admit(time(now, 750), metrics, params)?;
                }
                RunStep::Tick { after } => {
                    // A new flush never starts while one is pending: the previous one completes first.
                    run.complete()?;
                    run.check()?;
                    now = now.saturating_add_signed(*after);
                    run.tick(time(now, 250), params)?;
                }
                RunStep::Reset { after } => {
                    // A reset needs the pending flush completed first.
                    run.complete()?;
                    run.check()?;
                    run.oneshot.call(Some(time(now, 0)), Budget::unlimited(), Some(params), |kernel, turn| {
                        assert_eq!(kernel.control(turn, &mut ClockReset), Progress::Done)
                    });
                    run.interleaved.call(Some(time(now, 0)), Budget::unlimited(), Some(params), |kernel, turn| {
                        assert_eq!(kernel.control(turn, &mut ClockReset), Progress::Done)
                    });
                    run.emitted.0.extend(run.oneshot.emitted());
                    run.emitted.1.extend(run.interleaved.emitted());
                    run.check()?;
                    now = now.saturating_add_signed(*after);
                }
            }
        }

        // The run ends by retiring, which emits everything, or by shutting down, which discards the open buckets.
        run.complete()?;
        run.check()?;
        let policy = if retire { DrainPolicy::Retire } else { DrainPolicy::Shutdown };
        let params = &revisions_params[0];
        for host in [&mut run.oneshot, &mut run.interleaved] {
            host.call(Some(time(now, 0)), Budget::unlimited(), Some(params), |kernel, turn| {
                assert_eq!(kernel.finish(turn, policy), Progress::Done)
            });
        }
        run.emitted.0.extend(run.oneshot.emitted());
        run.emitted.1.extend(run.interleaved.emitted());
        run.check()?;
        prop_assert!(run.interleaved.kernel.core().is_empty());
        prop_assert_eq!(run.interleaved_budget.global.used(), 0);
    }
}
