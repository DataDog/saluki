// Bin keys and quantiles compare exactly once both lanes share a grid, so these assert equality
// rather than a margin.
#![allow(clippy::float_cmp)]

use proptest::prelude::*;

use super::*;

fn sketch(count: i64, sum: f64, min: f64, max: f64, bins: &[(i32, u32)]) -> SketchValue {
    SketchValue {
        count,
        sum,
        min,
        max,
        bins: bins.to_vec(),
    }
}

proptest! {
    // Two lanes on the same grid must be comparable, whatever their bins hold. Both implementations trim
    // bins from the LEFT once they pass 4096, folding the lowest into the cutoff bin, so the lowest
    // populated key becomes the cutoff rather than `key(min)` while the `min` summary keeps the true
    // minimum. Inferring the grid from the lowest populated key therefore reads a collapse as a different
    // grid and reds with a misdiagnosis.
    #[test]
    fn property_test_a_collapsed_prefix_is_still_the_same_grid(
        cutoff in 40i32..300,
        top in 400i32..600,
        counts in prop::collection::vec(1u32..50, 2..8),
    ) {
        let low = cutoff - 20;
        let full: Vec<(i32, u32)> = (0..counts.len())
            .map(|i| (low + i32::try_from(i).unwrap_or(0), counts[i]))
            .chain(std::iter::once((top, 1)))
            .collect();
        // The same data after a left trim: everything below the cutoff folded into it.
        let folded: u32 = full.iter().filter(|(k, _)| *k < cutoff).map(|(_, n)| *n).sum();
        let collapsed: Vec<(i32, u32)> = std::iter::once((cutoff, folded.max(1)))
            .chain(full.iter().copied().filter(|(k, _)| *k >= cutoff))
            .collect();

        // Same grid, same summary extremes. Only the bin layout differs.
        let min = key_to_value(low).expect("grid key");
        let max = key_to_value(top).expect("grid key");
        let a = sketch(10, 100.0, min, max, &full);
        let b = sketch(10, 100.0, min, max, &collapsed);

        prop_assert!(
            same_quantization(&a, &b),
            "a collapsed prefix read as a different grid: {:?} vs {:?}",
            a.bins,
            b.bins
        );
    }

    // Invariant 20. Merge is associative and commutative, so the order a bucket's sketches arrive in
    // cannot change the merged bins.
    #[test]
    fn property_test_merge_ignores_order(
        mut parts in prop::collection::vec(
            (1i64..100, 1.0f64..1000.0, prop::collection::vec((1i32..40, 1u32..20), 1..6)),
            1..5,
        ),
    ) {
        let build = |p: &(i64, f64, Vec<(i32, u32)>)| {
            let mut bins = p.2.clone();
            bins.sort_unstable_by_key(|(k, _)| *k);
            bins.dedup_by_key(|(k, _)| *k);
            sketch(p.0, p.1, 1.0, 100.0, &bins)
        };
        let forward = merge(&parts.iter().map(build).collect::<Vec<_>>()).expect("non-empty");
        parts.reverse();
        let reversed = merge(&parts.iter().map(build).collect::<Vec<_>>()).expect("non-empty");

        // Bins are integer counts, so they agree exactly. `sum` is float addition, which is not
        // associative, so it agrees only to rounding. That drift is many orders below
        // equivalence_threshold and cannot move a verdict.
        prop_assert_eq!(&forward.bins, &reversed.bins);
        prop_assert_eq!(forward.count, reversed.count);
        prop_assert!((forward.sum - reversed.sum).abs() <= forward.sum.abs() * 1e-12);
    }

    // Invariant 21. Merge preserves the summaries. Counts and sums add, min and max take extremes, so
    // a merged bucket reads as one sketch over the same points would.
    #[test]
    fn property_test_merge_preserves_the_summaries(
        parts in prop::collection::vec((1i64..100, 1.0f64..500.0, 1.0f64..50.0, 60.0f64..500.0), 1..6),
    ) {
        let values: Vec<SketchValue> = parts
            .iter()
            .map(|&(c, s, lo, hi)| sketch(c, s, lo, hi, &[(10, 1)]))
            .collect();
        let merged = merge(&values).expect("non-empty");

        prop_assert_eq!(merged.count, parts.iter().map(|p| p.0).sum::<i64>());
        prop_assert_eq!(merged.sum, parts.iter().map(|p| p.1).sum::<f64>());
        prop_assert_eq!(merged.min, parts.iter().map(|p| p.2).fold(f64::INFINITY, f64::min));
        prop_assert_eq!(merged.max, parts.iter().map(|p| p.3).fold(f64::NEG_INFINITY, f64::max));
    }

    // Invariant 23. Quantiles are monotone, so a wider quantile never reads below a narrower one.
    #[test]
    fn property_test_quantiles_are_monotone(bins in prop::collection::vec((1i32..200, 1u32..50), 1..12)) {
        let mut bins = bins;
        bins.sort_unstable_by_key(|(k, _)| *k);
        bins.dedup_by_key(|(k, _)| *k);
        let s = sketch(100, 500.0, 1.0, 1000.0, &bins);

        let (p75, p95, p99) = (
            quantile(&s, 0.75).expect("bins"),
            quantile(&s, 0.95).expect("bins"),
            quantile(&s, 0.99).expect("bins"),
        );

        prop_assert!(p75 <= p95 && p95 <= p99, "{p75} {p95} {p99}");
    }

    // Invariant 24. On one grid, equal bins give equal quantiles. This is what the quantization
    // requirement buys: the comparison is exact rather than bounded by the sketch's relative accuracy.
    #[test]
    fn property_test_equal_bins_give_equal_quantiles(bins in prop::collection::vec((1i32..200, 1u32..50), 1..12)) {
        let mut bins = bins;
        bins.sort_unstable_by_key(|(k, _)| *k);
        bins.dedup_by_key(|(k, _)| *k);
        let a = sketch(10, 50.0, 1.0, 100.0, &bins);
        let b = sketch(10, 50.0, 1.0, 100.0, &bins);

        prop_assert_eq!(project(&a), project(&b));
    }

    // A key stands for the values in `[gamma^(k-bias-0.5), gamma^(k-bias+0.5))`. The representative value
    // must sit inside its own bin, so a value we map from a key keys back to it.
    #[test]
    fn property_test_a_key_maps_inside_its_own_bin(k in 1i32..500) {
        let value = key_to_value(k).expect("a key inside the grid maps to a value");
        let exponent = f64::from(k - bias());
        let lower = GAMMA.powf(exponent - 0.5);
        let upper = GAMMA.powf(exponent + 0.5);

        prop_assert!(value >= lower && value < upper, "{value} outside [{lower}, {upper}) for key {k}");
    }
}

// Invariant 22. A quantile read out of a sketch sits inside the range the sketch claims, so a bad bin
// cannot report a value the data never held.
#[test]
fn a_quantile_lies_within_the_recorded_range() {
    let s = sketch(
        6,
        60.0,
        key_to_value(600).expect("grid key"),
        key_to_value(700).expect("grid key"),
        &[(600, 3), (650, 2), (700, 1)],
    );

    let p95 = quantile(&s, 0.95).expect("bins");

    assert!(p95 >= s.min && p95 <= s.max, "{p95} outside [{}, {}]", s.min, s.max);
}

// Invariant 26. A sketch with no bins yields no quantile series. Reading zero there would report a
// dropped distribution as one centred on zero.
#[test]
fn a_sketch_without_bins_yields_no_quantiles() {
    let s = sketch(0, 0.0, 0.0, 0.0, &[]);

    let projected = project(&s);

    assert_eq!(quantile(&s, 0.5), None);
    assert_eq!((projected.p75, projected.p95, projected.p99), (None, None, None));
}

// Invariant 25. Lanes reporting the same extremes but a different highest populated key are on different
// grids, since a different gamma moves where `max` lands. Every quantile comparison between them is
// meaningless, so this is caught rather than compared. The lowest key is not part of the test, because bin
// collapse moves it on the identical grid.
#[test]
fn differing_grids_under_equal_extremes_are_detected() {
    let a = sketch(10, 50.0, 1.0, 100.0, &[(100, 5), (200, 5)]);
    let b = sketch(10, 50.0, 1.0, 100.0, &[(100, 5), (201, 5)]);

    assert!(!same_quantization(&a, &b));
}

// The converse, spelled out for one concrete collapse. A lane that folded its low bins into a cutoff
// reports a higher lowest key on the same grid, and must stay comparable.
#[test]
fn a_collapsed_low_bin_is_not_a_grid_mismatch() {
    let full = sketch(10, 50.0, 1.0, 100.0, &[(100, 2), (150, 3), (200, 5)]);
    let collapsed = sketch(10, 50.0, 1.0, 100.0, &[(150, 5), (200, 5)]);

    assert!(same_quantization(&full, &collapsed));
}

// Different data legitimately lands on different keys, so the check only fires where the raw extremes
// agree. Otherwise every honest difference would read as a grid mismatch.
#[test]
fn differing_extremes_are_not_a_grid_mismatch() {
    let a = sketch(10, 50.0, 1.0, 100.0, &[(100, 5)]);
    let b = sketch(10, 50.0, 1.0, 900.0, &[(400, 5)]);

    assert!(same_quantization(&a, &b));
}

// Merging adds the counts of a shared key rather than keeping one, and leaves the bins key-sorted for
// the cumulative walk the quantile read depends on.
#[test]
fn merge_adds_shared_keys_and_keeps_order() {
    let a = sketch(3, 30.0, 1.0, 100.0, &[(10, 2), (30, 1)]);
    let b = sketch(2, 20.0, 1.0, 100.0, &[(10, 1), (20, 1)]);

    let merged = merge(&[a, b]).expect("non-empty");

    assert_eq!(merged.bins, vec![(10, 3), (20, 1), (30, 1)]);
}

// The Agent reserves the outermost keys for everything its bin grid cannot represent. Mapping a terminal
// key as though it were an ordinary key lands 1.5% from the last finite bin, inside the equivalence
// threshold, so a lane reporting overflow would read as agreeing with a lane reporting a large value.
#[test]
fn a_terminal_key_folds_to_the_sketch_extreme() {
    // The key itself is an infinity, as the Agent maps it.
    assert_eq!(key_to_value(32_767), Some(f64::INFINITY));
    assert_eq!(key_to_value(-32_767), Some(f64::NEG_INFINITY));

    // The projection clamps to the sketch's own extremes, so the infinity never reaches a series.
    // Weighted so the p99 rank lands in the terminal bin rather than the first.
    let overflowed = sketch(4, 40.0, 2.0, 900.0, &[(600, 1), (32_767, 3)]);
    let p99 = quantile(&overflowed, 0.99).expect("bins");
    assert_eq!(p99, 900.0, "a terminal bin escaped the clamp");
}

// No lane can put a key outside the Agent's domain on the wire, since both key through an i16. A key
// that arrives anyway is a producer fault, and the projection must report it rather than negate past
// `i32::MIN`, which aborts under the release profile's overflow checks and poisons the capture lock.
#[test]
fn an_out_of_domain_key_has_no_value() {
    assert_eq!(key_to_value(32_768), None);
    assert_eq!(key_to_value(-32_768), None);
    assert_eq!(key_to_value(i32::MIN), None);
}

// A sketch with no points carries no extremes, and the normative merge skips its summary,
// `lib/ddsketch/src/agent/sketch.rs:601-618`. Folding its zero min into the bucket would report a range
// the points never had.
#[test]
fn a_zero_count_part_does_not_drag_the_merged_extremes() {
    let empty = sketch(0, 0.0, 0.0, 0.0, &[(5, 1)]);
    let real = sketch(4, 40.0, 7.0, 11.0, &[(600, 4)]);
    let merged = merge(&[empty, real]).expect("merge");
    assert_eq!(merged.min, 7.0);
    assert_eq!(merged.max, 11.0);
    assert_eq!(merged.count, 4);
    // Bins merge regardless of count, so the empty part's bin survives.
    assert_eq!(merged.bins, vec![(5, 1), (600, 4)]);
}

// Two parts can carry the same key with counts that do not fit a `u32`. Saturating would discard the
// excess and shrink the distribution, so the remainder rides in a second entry for the same key, which
// `quantile_key` reads back in full because it accumulates in `u64` over key-ordered bins.
#[test]
fn an_overflowing_bin_count_keeps_its_remainder() {
    let a = sketch(1, 1.0, 1.0, 1.0, &[(600, u32::MAX)]);
    let b = sketch(1, 1.0, 1.0, 1.0, &[(600, 10)]);
    let merged = merge(&[a, b]).expect("merge");
    assert_eq!(merged.bins, vec![(600, u32::MAX), (600, 10)]);
    let total: u64 = merged.bins.iter().map(|(_, n)| u64::from(*n)).sum();
    assert_eq!(total, u64::from(u32::MAX) + 10);
}

// The rank is the backend's, `trunc(q * (total - 1) + 1)` over the sum of the bin counts. The old
// `ceil(total * p / 100)` sat one rank higher for
// most totals, so for any bucket holding under a hundred samples p99 selected the top populated bin and the
// p99 series became a copy of the max series, inheriting full outlier sensitivity.
#[test]
fn the_quantile_rank_matches_the_backend() {
    // Ten singleton bins. Backend rank for p75 is trunc(0.75 * 9 + 1) = 7, so the seventh bin.
    let bins: Vec<(i32, u32)> = (600..610).map(|k| (k, 1)).collect();
    let s = sketch(10, 100.0, 0.0, f64::MAX, &bins);

    let expect = |q: f64, key: i32| {
        assert_eq!(quantile(&s, q), key_to_value(key), "q = {q} did not select key {key}");
    };
    expect(0.75, 606);
    expect(0.95, 608);
    expect(0.99, 608);
    // The point of the rank fix: p99 is not the top populated bin, even at ten samples. Under
    // `ceil(total * p / 100)` it was, for every total under a hundred.
    assert_ne!(quantile(&s, 0.99), key_to_value(609), "p99 read as the maximum bin");
}

// A zero-count sketch carries no samples, so its sum describes nothing. Adding it fabricates samples in
// the projected sum series and one lane emitting a spare empty sketch would red the oracle on its own.
#[test]
fn a_zero_count_sum_is_not_folded_in() {
    let stale = sketch(0, 500.0, 0.0, 0.0, &[(5, 1)]);
    let real = sketch(4, 40.0, 7.0, 11.0, &[(600, 4)]);

    let merged = merge(&[stale, real]).expect("merge");

    assert_eq!(merged.sum, 40.0, "a zero-count sum was folded in");
    assert_eq!(merged.count, 4);
    assert_eq!((merged.min, merged.max), (7.0, 11.0));
}

// Every part being zero-count is the one case where the summary is adopted rather than skipped, since
// there is nothing else to take.
#[test]
fn an_all_zero_count_merge_adopts_a_summary() {
    let first = sketch(0, 3.0, 1.0, 2.0, &[(5, 1)]);
    let second = sketch(0, 9.0, 0.0, 0.0, &[(6, 1)]);

    let merged = merge(&[first, second]).expect("merge");

    assert_eq!(merged.count, 0);
    assert_eq!(merged.bins, vec![(5, 1), (6, 1)]);
}
