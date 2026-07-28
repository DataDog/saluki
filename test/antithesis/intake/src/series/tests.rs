// Exactness is the point in several of these. `d(x,x)` is zero by definition, not within a margin.
#![allow(clippy::float_cmp)]

use std::collections::BTreeMap;

use harness::Phase;
use proptest::prelude::*;

use super::*;
use crate::capture::MetricKind;

/// Finite values wide enough to exercise sign changes and magnitude gaps without reaching infinity.
fn value() -> impl Strategy<Value = f64> {
    -1e6f64..1e6f64
}

/// A pair of equal-length series, which is what truncation hands the comparison.
fn series_pair() -> impl Strategy<Value = (Vec<f64>, Vec<f64>)> {
    (1usize..12).prop_flat_map(|n| (prop::collection::vec(value(), n), prop::collection::vec(value(), n)))
}

proptest! {
    // Invariant 1. A bucket compared with itself is zero distance, including at zero where the
    // denominator would otherwise be zero.
    #[test]
    fn property_test_d_of_a_value_with_itself_is_zero(x in value()) {
        prop_assert_eq!(distance(x, x), 0.0);
    }

    // Invariant 2. Neither lane is privileged, so d reads the same in either order.
    #[test]
    fn property_test_d_is_symmetric(a in value(), b in value()) {
        prop_assert_eq!(distance(a, b), distance(b, a));
    }

    // Invariant 3. d never leaves [0, 2]. The upper end is reached only when the two values have equal
    // magnitude and opposite signs.
    #[test]
    fn property_test_d_is_bounded(a in value(), b in value()) {
        let d = distance(a, b);
        prop_assert!((0.0..=2.0).contains(&d), "d = {d}");
    }

    // Invariant 4. d compares shape, not scale, so rescaling both lanes leaves it unchanged.
    #[test]
    fn property_test_d_is_scale_invariant(a in value(), b in value(), k in 0.001f64..1000.0) {
        let scaled = distance(a * k, b * k);
        prop_assert!((scaled - distance(a, b)).abs() < 1e-9, "{scaled} vs {}", distance(a, b));
    }

    // Invariant 5. Two lanes carrying the same curve are equivalent at any leash.
    #[test]
    fn property_test_identical_series_score_zero(a in prop::collection::vec(value(), 1..12), w in 0usize..4) {
        prop_assert_eq!(frechet(&a, &a, w), Some(0.0));
    }

    // Invariant 6. Neither lane is privileged in F either.
    #[test]
    fn property_test_frechet_is_symmetric((a, b) in series_pair(), w in 0usize..4) {
        prop_assert_eq!(frechet(&a, &b, w), frechet(&b, &a, w));
    }

    // Invariant 7. A wider leash admits every walk a narrower one did, so it can only lower the result.
    #[test]
    fn property_test_frechet_is_monotone_non_increasing_in_leash((a, b) in series_pair(), w in 0usize..3) {
        let narrow = frechet(&a, &b, w).expect("non-empty");
        let wide = frechet(&a, &b, w + 1).expect("non-empty");
        prop_assert!(wide <= narrow + 1e-9, "w={w} narrow={narrow} wide={wide}");
    }

    // Invariant 8. F is a max along one walk, so it cannot exceed the worst admissible pair.
    #[test]
    fn property_test_frechet_is_bounded_by_the_worst_admissible_pair((a, b) in series_pair(), w in 0usize..4) {
        let worst = a
            .iter()
            .enumerate()
            .flat_map(|(i, &x)| {
                b.iter()
                    .enumerate()
                    .filter(move |(j, _)| i.abs_diff(*j) <= w)
                    .map(move |(_, &y)| distance(x, y))
            })
            .fold(0.0f64, f64::max);
        let f = frechet(&a, &b, w).expect("non-empty");
        prop_assert!(f <= worst + 1e-9, "f={f} worst={worst}");
    }

    // Invariant 10. Same inputs, same output, so a replay reproduces a verdict exactly.
    #[test]
    fn property_test_frechet_is_deterministic((a, b) in series_pair(), w in 0usize..4) {
        prop_assert_eq!(frechet(&a, &b, w), frechet(&a, &b, w));
    }
}

/// Owns the columns a `ScalarView` borrows, so a test can hand points to the comparison core.
#[derive(Default)]
struct Columns {
    timestamps: Vec<i64>,
    seqs: Vec<u32>,
    intervals: Vec<u32>,
    values: Vec<f64>,
}

impl Columns {
    fn of(samples: &[Sample]) -> Self {
        let mut c = Self::default();
        for s in samples {
            c.timestamps.push(s.timestamp);
            c.seqs.push(s.seq);
            c.intervals.push(s.interval);
            c.values.push(s.value);
        }
        c
    }

    fn view(&self) -> ScalarView<'_> {
        ScalarView {
            timestamps: &self.timestamps,
            seqs: &self.seqs,
            intervals: &self.intervals,
            values: &self.values,
        }
    }
}

fn pt(ts: i64, seq: u32, interval: u32, value: f64) -> Sample {
    Sample {
        timestamp: ts,
        seq,
        interval,
        value,
    }
}

proptest! {
    // Invariant 11. Once the resubmit rule has run a bucket holds at most one point per timestamp, so
    // the fold sees a set and the order it arrived in cannot change the answer.
    #[test]
    fn property_test_folds_ignore_input_order(
        kind in prop::sample::select(vec![MetricKind::Count, MetricKind::Rate, MetricKind::Gauge]),
        mut points in prop::collection::vec((0i64..40, 1u32..30, value()), 1..10),
    ) {
        let samples: Vec<Sample> = points
            .iter()
            .enumerate()
            .map(|(i, &(ts, interval, v))| pt(ts, u32::try_from(i).unwrap_or(0), interval, v))
            .collect();
        let cols_samples = Columns::of(&samples);
        let forward = fold_buckets(kind, cols_samples.view(), 10, Resubmit::KeepLast);
        points.reverse();
        let reversed: Vec<Sample> = points
            .iter()
            .enumerate()
            .map(|(i, &(ts, interval, v))| pt(ts, u32::try_from(points.len() - 1 - i).unwrap_or(0), interval, v))
            .collect();

        let cols_reversed = Columns::of(&reversed);
        prop_assert_eq!(forward, fold_buckets(kind, cols_reversed.view(), 10, Resubmit::KeepLast));
    }

    // Invariant 12. A weighted mean cannot escape the values it averages.
    #[test]
    fn property_test_the_rate_fold_stays_within_its_bucket(
        points in prop::collection::vec((1u32..30, value()), 1..8),
    ) {
        let samples: Vec<Sample> = points
            .iter()
            .enumerate()
            .map(|(i, &(interval, v))| pt(i64::try_from(i).unwrap_or(0), u32::try_from(i).unwrap_or(0), interval, v))
            .collect();
        let lo = points.iter().map(|(_, v)| *v).fold(f64::INFINITY, f64::min);
        let hi = points.iter().map(|(_, v)| *v).fold(f64::NEG_INFINITY, f64::max);

        let cols_samples = Columns::of(&samples);
        let folded = fold_buckets(MetricKind::Rate, cols_samples.view(), 100, Resubmit::KeepLast);
        let value = folded.values().next().copied().expect("one bucket");

        prop_assert!(value >= lo - 1e-9 && value <= hi + 1e-9, "{value} outside [{lo}, {hi}]");
    }

    // Invariant 16. Bucketing partitions the points. Every collapsed point lands in exactly one bucket,
    // so the populations sum back to the collapsed count and nothing is quietly dropped.
    #[test]
    fn property_test_bucketing_partitions_the_points(
        points in prop::collection::vec((0i64..200, value()), 1..20),
        w in 1i64..30,
    ) {
        let samples: Vec<Sample> = points
            .iter()
            .enumerate()
            .map(|(i, &(ts, v))| pt(ts, u32::try_from(i).unwrap_or(0), 10, v))
            .collect();
        let collapsed = collapse(samples.iter().copied(), Resubmit::KeepLast);
        let mut populations = BTreeMap::new();
        for s in &collapsed {
            *populations.entry(s.timestamp.div_euclid(w)).or_insert(0usize) += 1;
        }

        prop_assert_eq!(populations.values().sum::<usize>(), collapsed.len());
    }

    // Invariant 19. Each resubmit rule is a fixpoint. Applying it to points already collapsed changes
    // nothing, so a query that collapses twice reads the same curve.
    #[test]
    fn property_test_resubmit_rules_are_idempotent(
        points in prop::collection::vec((0i64..15, value()), 1..12),
        rule in prop::sample::select(vec![Resubmit::KeepLast, Resubmit::KeepFirst, Resubmit::Sum]),
    ) {
        let samples: Vec<Sample> = points
            .iter()
            .enumerate()
            .map(|(i, &(ts, v))| pt(ts, u32::try_from(i).unwrap_or(0), 10, v))
            .collect();
        let once = collapse(samples.iter().copied(), rule);
        let twice = collapse(once.iter().copied(), rule);

        prop_assert_eq!(once, twice);
    }
}

// Invariant 18. Collapse leaves one point per timestamp, and the rule decides which. `KeepLast` takes
// the greatest seq, `KeepFirst` the least, and `Sum` adds the values while keeping the least seq.
#[test]
fn collapse_applies_the_rule_to_a_timestamp_tie() {
    let samples = [pt(100, 0, 10, 1.0), pt(100, 1, 10, 2.0), pt(110, 2, 10, 5.0)];

    let last = collapse(samples.iter().copied(), Resubmit::KeepLast);
    let first = collapse(samples.iter().copied(), Resubmit::KeepFirst);
    let sum = collapse(samples.iter().copied(), Resubmit::Sum);

    assert_eq!(last.iter().map(|s| s.value).collect::<Vec<_>>(), vec![2.0, 5.0]);
    assert_eq!(first.iter().map(|s| s.value).collect::<Vec<_>>(), vec![1.0, 5.0]);
    assert_eq!(sum.iter().map(|s| s.value).collect::<Vec<_>>(), vec![3.0, 5.0]);
}

// Invariants 13 and 15. Gauge folds to the latest timestamp in the bucket, and an empty gauge bucket
// carries the previous value forward rather than reading zero.
#[test]
fn gauge_folds_last_and_carries_forward() {
    let samples = [pt(0, 0, 10, 7.0), pt(5, 1, 10, 9.0), pt(40, 2, 10, 3.0)];

    let cols_samples = Columns::of(&samples);
    let folded = fold_buckets(MetricKind::Gauge, cols_samples.view(), 10, Resubmit::KeepLast);
    let filled = fill(MetricKind::Gauge, &folded, 0, 4);

    assert_eq!(filled, vec![9.0, 9.0, 9.0, 9.0, 3.0]);
}

// Invariants 14 and 15. Count and rate read zero where no point landed, and the filled series spans
// k_start through k_end with no gap.
#[test]
fn count_fills_empty_buckets_with_zero() {
    let samples = [pt(0, 0, 10, 4.0), pt(30, 1, 10, 6.0)];

    let cols_samples = Columns::of(&samples);
    let folded = fold_buckets(MetricKind::Count, cols_samples.view(), 10, Resubmit::KeepLast);
    let filled = fill(MetricKind::Count, &folded, 0, 3);

    assert_eq!(filled, vec![4.0, 0.0, 0.0, 6.0]);
}

// The count fold sums a bucket, so two points in one bucket add rather than replace.
#[test]
fn count_sums_within_a_bucket() {
    let samples = [pt(0, 0, 10, 4.0), pt(3, 1, 10, 6.0)];

    let cols_samples = Columns::of(&samples);
    let folded = fold_buckets(MetricKind::Count, cols_samples.view(), 10, Resubmit::KeepLast);

    assert_eq!(folded.get(&0), Some(&10.0));
}

// The rate fold weights by interval, so a long-interval point pulls the average toward itself.
#[test]
fn rate_weights_by_interval() {
    let samples = [pt(0, 0, 10, 0.0), pt(1, 1, 30, 4.0)];

    let cols_samples = Columns::of(&samples);
    let folded = fold_buckets(MetricKind::Rate, cols_samples.view(), 10, Resubmit::KeepLast);

    assert_eq!(folded.get(&0), Some(&3.0));
}

// Invariant 36. A range with no buckets is not comparable, which is distinct from comparing as equal.
#[test]
fn an_inverted_range_is_not_comparable() {
    let folded = BTreeMap::new();

    assert!(fill(MetricKind::Count, &folded, 5, 4).is_empty());
}

// Two lanes carrying the same points compare as equivalent end to end.
#[test]
fn matching_lanes_compare_at_zero() {
    let points: Vec<Sample> = (0..6)
        .map(|i| pt(i * 10, u32::try_from(i).unwrap_or(0), 10, 100.0))
        .collect();

    let cols_a = Columns::of(&points);
    let cols_b = Columns::of(&points);
    let verdict = compare(
        MetricKind::Count,
        cols_a.view(),
        cols_b.view(),
        10,
        1,
        Resubmit::KeepLast,
        Phase::Eventually,
    );

    assert_eq!(verdict, Verdict::Distance(0.0));
}

// A lane that halves one bucket is caught rather than absorbed by the leash.
#[test]
fn a_halved_bucket_is_a_distance() {
    let agent: Vec<Sample> = (0..6)
        .map(|i| pt(i * 10, u32::try_from(i).unwrap_or(0), 10, 200.0))
        .collect();
    let mut adp = agent.clone();
    adp[2].value = 100.0;

    let cols_a = Columns::of(&agent);
    let cols_b = Columns::of(&adp);
    let Verdict::Distance(d) = compare(
        MetricKind::Count,
        cols_a.view(),
        cols_b.view(),
        10,
        1,
        Resubmit::KeepLast,
        Phase::Eventually,
    ) else {
        panic!("expected a distance");
    };

    assert!(d > 0.02, "d = {d}");
}

// A lane with no points is not comparable, never equivalent. An empty lane must not read as agreement.
#[test]
fn an_empty_lane_is_not_comparable() {
    let points = [pt(0, 0, 10, 1.0)];

    let cols_a = Columns::of(&points);
    let cols_b = Columns::default();
    assert_eq!(
        compare(
            MetricKind::Count,
            cols_a.view(),
            cols_b.view(),
            10,
            1,
            Resubmit::KeepLast,
            Phase::Eventually
        ),
        Verdict::Skipped(Skip::NoOverlap)
    );
}

// A kind the fold table drops is skipped under its own reason, not folded to zero and compared.
#[test]
fn an_other_kind_is_skipped_by_reason() {
    let points = [pt(0, 0, 10, 1.0)];

    let cols_a = Columns::of(&points);
    let cols_b = Columns::of(&points);
    assert_eq!(
        compare(
            MetricKind::Other,
            cols_a.view(),
            cols_b.view(),
            10,
            1,
            Resubmit::KeepLast,
            Phase::Eventually
        ),
        Verdict::Skipped(Skip::KindOther)
    );
}

// Truncation drops the trailing bucket, so a lane with only one bucket of data has nothing whole to
// compare and is skipped rather than compared against a still-filling bucket.
#[test]
fn a_single_bucket_is_too_short_to_compare() {
    let points = [pt(0, 0, 10, 1.0), pt(5, 1, 10, 2.0)];

    let cols_a = Columns::of(&points);
    let cols_b = Columns::of(&points);
    assert_eq!(
        compare(
            MetricKind::Count,
            cols_a.view(),
            cols_b.view(),
            10,
            1,
            Resubmit::KeepLast,
            Phase::Eventually
        ),
        Verdict::Skipped(Skip::ShortSeries)
    );
}

// One lane reporting an infinity where the other reports a finite value is a divergence, not something to
// set aside. Skipping it meant the context never counted as failed, so both assertions stayed green on a
// real mismatch as long as some other context satisfied coverage.
#[test]
fn an_infinity_against_a_finite_value_diverges() {
    let agent: Vec<Sample> = (0..6)
        .map(|i| pt(i * 10, u32::try_from(i).unwrap_or(0), 10, 1.0))
        .collect();
    let mut adp = agent.clone();
    adp[2].value = f64::INFINITY;

    let cols_a = Columns::of(&agent);
    let cols_b = Columns::of(&adp);
    assert_eq!(
        compare(
            MetricKind::Count,
            cols_a.view(),
            cols_b.view(),
            10,
            1,
            Resubmit::KeepLast,
            Phase::Eventually
        ),
        Verdict::Distance(MAX_DISTANCE)
    );
}

// Two lanes carrying the same non-finite value for the same input agree. `NaN` never equals itself, so
// this is the case a plain comparison would report as the loudest possible divergence.
#[test]
fn matching_non_finite_values_agree() {
    for value in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
        let points: Vec<Sample> = (0..6)
            .map(|i| pt(i * 10, u32::try_from(i).unwrap_or(0), 10, value))
            .collect();
        let cols_a = Columns::of(&points);
        let cols_b = Columns::of(&points);
        assert_eq!(
            compare(
                MetricKind::Count,
                cols_a.view(),
                cols_b.view(),
                10,
                1,
                Resubmit::KeepLast,
                Phase::Eventually
            ),
            Verdict::Distance(0.0),
            "{value} disagreed with itself"
        );
    }
}

// Weighting by `value * interval` overflows a large finite value to `+inf`, which the division cannot
// undo, and `distance` reads two matching infinities as agreement. Lanes a factor of eight apart then
// compared equal. The boundary pool samples `f64::MAX`, so this is load the run generates.
#[test]
fn a_huge_rate_stays_distinguishable_from_a_smaller_one() {
    let lane = |value: f64| -> Vec<Sample> {
        (0..6)
            .map(|i| pt(i * 10, u32::try_from(i).unwrap_or(0), 10, value))
            .collect()
    };
    let agent = lane(f64::MAX);
    let adp = lane(f64::MAX / 8.0);
    let cols_a = Columns::of(&agent);
    let cols_b = Columns::of(&adp);

    let verdict = compare(
        MetricKind::Rate,
        cols_a.view(),
        cols_b.view(),
        10,
        1,
        Resubmit::KeepLast,
        Phase::Eventually,
    );

    let Verdict::Distance(d) = verdict else {
        panic!("expected a distance, got {verdict:?}");
    };
    assert!(d > 0.02, "d = {d}");
}

// A zero-interval point carries no weight, so it moves the mean nowhere and a bucket holding only such
// points folds to nothing rather than dividing by zero.
#[test]
fn zero_interval_points_carry_no_weight() {
    let weighted = [pt(0, 0, 0, 500.0), pt(1, 1, 10, 3.0)];
    let cols = Columns::of(&weighted);
    let folded = fold_buckets(MetricKind::Rate, cols.view(), 100, Resubmit::KeepLast);
    assert_eq!(folded.values().next().copied(), Some(3.0));

    let weightless = [pt(0, 0, 0, 500.0)];
    let cols = Columns::of(&weightless);
    assert!(fold_buckets(MetricKind::Rate, cols.view(), 100, Resubmit::KeepLast).is_empty());
}

// Opposite infinities are as far apart as the scale goes.
#[test]
fn opposite_infinities_diverge() {
    assert_eq!(distance(f64::INFINITY, f64::NEG_INFINITY), MAX_DISTANCE);
    assert_eq!(distance(f64::INFINITY, 1.0), MAX_DISTANCE);
    assert_eq!(distance(f64::NAN, 1.0), MAX_DISTANCE);
}

/// A sketch point whose extremes agree with its bins, as a real one's must. The lowest populated bin
/// holds `min` and the highest holds `max`, so a fixture that violates that reads as a grid mismatch.
fn sk(ts: i64, seq: u32, bins: &[(i32, u32)], count: i64, sum: f64) -> SketchSample {
    let lo = bins.first().map_or(0, |(k, _)| *k);
    let hi = bins.last().map_or(0, |(k, _)| *k);
    SketchSample {
        timestamp: ts,
        seq,
        value: SketchValue {
            count,
            sum,
            min: crate::sketch::key_to_value(lo).expect("grid key"),
            max: crate::sketch::key_to_value(hi).expect("grid key"),
            bins: bins.to_vec(),
        },
    }
}

// Two lanes holding identical sketches agree across all seven projected statistics.
#[test]
fn matching_sketch_lanes_compare_at_zero() {
    let points: Vec<SketchSample> = (0..6)
        .map(|i| sk(i * 10, 0, &[(100, 5), (200, 5)], 10, 500.0))
        .collect();

    assert_eq!(
        compare_sketch(&points, &points, 10, 1, Phase::Eventually),
        Verdict::Distance(0.0)
    );
}

// A lane whose distribution shifts into higher bins diverges on the quantile series even though its
// count matches. This is the coverage the four summary statistics alone would miss.
#[test]
fn a_shifted_distribution_is_caught_at_the_quantiles() {
    let agent: Vec<SketchSample> = (0..6)
        .map(|i| sk(i * 10, 0, &[(100, 9), (110, 1)], 10, 500.0))
        .collect();
    let adp: Vec<SketchSample> = (0..6)
        .map(|i| sk(i * 10, 0, &[(100, 9), (400, 1)], 10, 500.0))
        .collect();

    let Verdict::Distance(d) = compare_sketch(&agent, &adp, 10, 1, Phase::Eventually) else {
        panic!("expected a distance");
    };

    assert!(d > 0.02, "d = {d}");
}

/// A sketch whose extremes are stated independently of its bins. Equal extremes over different keys
/// cannot happen on one grid, which is the signature the mismatch check reads.
fn off_grid(ts: i64, bins: &[(i32, u32)], min: f64, max: f64) -> SketchSample {
    SketchSample {
        timestamp: ts,
        seq: 0,
        value: SketchValue {
            count: 10,
            sum: 500.0,
            min,
            max,
            bins: bins.to_vec(),
        },
    }
}

// Lanes on different grids are reported as a mismatch, not as a distance. A number computed across
// two grids would describe nothing, and calling it a skip would hide a real config-parity defect.
#[test]
fn differing_grids_report_a_mismatch() {
    let agent: Vec<SketchSample> = (0..6)
        .map(|i| off_grid(i * 10, &[(100, 5), (200, 5)], 1.0, 100.0))
        .collect();
    let adp: Vec<SketchSample> = (0..6)
        .map(|i| off_grid(i * 10, &[(100, 5), (201, 5)], 1.0, 100.0))
        .collect();

    assert_eq!(
        compare_sketch(&agent, &adp, 10, 1, Phase::Eventually),
        Verdict::QuantizationMismatch
    );
}

// The grid check runs before any comparison, so a mismatch is reported even where the series is too
// short to have yielded a distance.
#[test]
fn the_grid_check_precedes_truncation() {
    let agent = [off_grid(0, &[(100, 5)], 1.0, 100.0)];
    let adp = [off_grid(0, &[(101, 5)], 1.0, 100.0)];

    assert_eq!(
        compare_sketch(&agent, &adp, 10, 1, Phase::Eventually),
        Verdict::QuantizationMismatch
    );
}

// An empty sketch lane is not comparable, never equivalent.
#[test]
fn an_empty_sketch_lane_is_not_comparable() {
    let points = [sk(0, 0, &[(100, 5)], 5, 50.0)];

    assert_eq!(
        compare_sketch(&points, &[], 10, 1, Phase::Eventually),
        Verdict::Skipped(Skip::NoOverlap)
    );
}

// Invariant 9. A one-bucket lag on a falling edge is flush timing, not a divergence, and must score zero
// through the oracle. `frechet` pins its endpoints, so this holds because `compare` drops `leash` buckets
// from each end and leaves the lag interior, where the band routes around it. Freeing the endpoints
// instead bought the same zero here at the price of forgiving any divergence in an edge bucket.
#[test]
fn a_lag_on_a_falling_edge_scores_zero_through_the_oracle() {
    // A falls at bucket 2, B one bucket later.
    let agent: Vec<Sample> = (0..6)
        .map(|i| {
            pt(
                i * 10,
                u32::try_from(i).unwrap_or(0),
                10,
                if i >= 2 { 0.0 } else { 10.0 },
            )
        })
        .collect();
    let adp: Vec<Sample> = (0..6)
        .map(|i| {
            pt(
                i * 10,
                u32::try_from(i).unwrap_or(0),
                10,
                if i >= 3 { 0.0 } else { 10.0 },
            )
        })
        .collect();

    let cols_a = Columns::of(&agent);
    let cols_b = Columns::of(&adp);
    assert_eq!(
        compare(
            MetricKind::Count,
            cols_a.view(),
            cols_b.view(),
            10,
            1,
            Resubmit::KeepLast,
            Phase::Eventually
        ),
        Verdict::Distance(0.0)
    );
}

// Pinning costs monotonicity in the truncation point, which is worth pinning down rather than leaving as
// a claim. On a falling edge, cutting the trailing bucket raises the distance from nothing to everything,
// because the walk that routed around the lag no longer has a bucket to finish on. `compare` is what keeps
// this off the oracle: it ends the compared range `leash` buckets inside the data, so the range never ends
// on the edge this needs.
#[test]
fn pinning_is_not_monotone_in_the_truncation_point() {
    let a = [10.0, 0.0, 0.0];
    let b = [10.0, 10.0, 0.0];

    assert_eq!(frechet(&a, &b, 1), Some(0.0));
    assert_eq!(frechet(&a[..2], &b[..2], 1), Some(1.0));
}

// A lane that genuinely halves a bucket is caught.
#[test]
fn a_halved_bucket_is_caught() {
    let a = [200.0, 200.0, 200.0];
    let b = [200.0, 100.0, 200.0];

    assert_eq!(frechet(&a, &b, 1), Some(0.5));
}

// A zero leash pins the walk to the diagonal, so a shift is not forgiven.
#[test]
fn a_zero_leash_forgives_nothing() {
    let a = [10.0, 0.0, 0.0];
    let b = [10.0, 10.0, 0.0];

    assert_eq!(frechet(&a, &b, 0), Some(1.0));
}

// An empty series is not comparable. The caller must not read that as agreement, matching invariant 36
// at the `frechet` level.
#[test]
fn an_empty_series_is_not_comparable() {
    assert_eq!(frechet(&[], &[], 1), None);
    assert_eq!(frechet(&[1.0], &[], 1), None);
}
// A divergence anywhere in the compared range must count, including its first and last bucket. With
// free endpoints a walk could start or finish inside the range and step over an edge bucket, so a lane
// whose first bucket was a thousandfold off scored exact agreement.
#[test]
fn an_edge_bucket_divergence_is_not_forgiven() {
    let a = [1.0, 1.0, 1.0, 1.0, 1.0];
    for diverged in [
        [1000.0, 1.0, 1.0, 1.0, 1.0],
        [1.0, 1.0, 1.0, 1.0, 1000.0],
        [1.0, 1.0, 1000.0, 1.0, 1.0],
    ] {
        let distance = frechet(&a, &diverged, 1).expect("comparable");
        assert!(distance > 0.9, "divergence forgiven, scored {distance}: {diverged:?}");
    }
}

// A pure phase offset of one bucket is what the leash is for, so it stays cheap inside the band.
#[test]
fn a_phase_offset_within_the_leash_stays_cheap() {
    let a = [1.0, 2.0, 3.0, 4.0, 5.0];
    let shifted = [1.0, 1.0, 2.0, 3.0, 4.0];
    let with_leash = frechet(&a, &shifted, 1).expect("comparable");
    let without = frechet(&a, &shifted, 0).expect("comparable");
    assert!(with_leash < without, "leash bought nothing: {with_leash} vs {without}");
}

// The newest bucket is dropped while load runs because it is still filling. Once load has stopped nothing
// is filling, so the finally phase keeps it and compares one bucket further.
#[test]
fn the_finally_phase_keeps_the_last_completed_bucket() {
    assert_eq!(range(0, 100, 10, 0, Phase::Eventually), Some((0, 9)));
    assert_eq!(range(0, 100, 10, 0, Phase::Finally), Some((0, 10)));
    // The leash margin applies in both phases.
    assert_eq!(range(0, 100, 10, 1, Phase::Eventually), Some((1, 8)));
    assert_eq!(range(0, 100, 10, 1, Phase::Finally), Some((1, 9)));
}

// A span wider than the cap would size the filled vectors off how long the context lived divided by the
// bucket width, so it is skipped with its own reason rather than allocated.
#[test]
fn a_span_wider_than_the_cap_is_skipped() {
    let agent = vec![pt(0, 0, 1, 1.0), pt(MAX_BUCKET_SPAN + 10, 1, 1, 1.0)];
    let cols = Columns::of(&agent);
    assert_eq!(
        compare(
            MetricKind::Count,
            cols.view(),
            cols.view(),
            1,
            0,
            Resubmit::KeepLast,
            Phase::Eventually,
        ),
        Verdict::Skipped(Skip::SpanTooWide)
    );
}
