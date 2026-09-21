//! Shared [`Store`] tests ported from the reference `sketches-go` implementation.
//!
//! These cases are ports of the store suite in
//! [`ddsketch/store/store_test.go`](https://github.com/DataDog/sketches-go/blob/v1.4.7/ddsketch/store/store_test.go)
//! at tag `v1.4.7`, which is the tag our vendored DDSketch protobuf definitions track. They encode upstream
//! contracts we must not silently diverge from, so treat a failure here as a divergence from the reference rather
//! than as a test that needs relaxing.
//!
//! The centerpiece is [`Collapse`], the port of the reference suite's `collapsingLowest`/`collapsingHighest` bin
//! transforms. Those transforms specify collapsing behavior *declaratively*: given every `(index, count)` pair that
//! was ever added, each count survives in full, at `max(index, max_index - max_num_bins + 1)` for a lowest-collapsing
//! store and `min(index, min_index + max_num_bins - 1)` for a highest-collapsing one. That makes the expected result
//! independent of insertion order, which is precisely what the store implementations have to get right.
//!
//! Deliberate deviations from the reference, all of which make the tests stricter or cheaper rather than weaker:
//!
//! - Counts are `u64` here rather than `f64`, so every count comparison is exact instead of epsilon-based.
//! - The reference's `randomIndex` helper computes `random.Intn(to-from) - from`, which for `from = -1000` yields
//!   indices in `[1000, 3000)` and so never exercises negative indices. We use `[-1000, 1000)` as intended.
//! - Fuzz iteration counts and value counts are scaled down, since our `test` target runs in debug mode. The
//!   randomized breadth the reference gets from sheer volume is covered here by the `property_test_*` cases.
//! - Rank queries are checked against exact bin boundaries, which the reference can only approximate with an
//!   epsilon fudge factor because its counts are floating point.

use std::collections::BTreeMap;

use proptest::prelude::*;

use super::{CollapsingHighestDenseStore, CollapsingLowestDenseStore, DenseStore, SparseStore, Store};

/// A store's `index -> count` distribution, with empty bins removed.
type Bins = BTreeMap<i32, u64>;

/// The bin capacities the reference suite exercises (`testMaxNumBins`).
const MAX_NUM_BINS: [usize; 3] = [8, 128, 1024];

/// Fixed seed, carried over from the reference suite so failures reproduce.
const SEED: u64 = 5388928120325255124;

/// Number of randomized trials per fuzz case (the reference's `numTests`, scaled for debug-mode runs).
const NUM_TRIALS: usize = 10;

/// How a store redistributes bins once its capacity is exceeded.
#[derive(Clone, Copy, Debug)]
enum Collapse {
    /// The store grows without bound, so every index keeps its own bin.
    None,

    /// Bins below `max_index - max_num_bins + 1` fold into that index.
    Lowest(usize),

    /// Bins above `min_index + max_num_bins - 1` fold into that index.
    Highest(usize),
}

impl Collapse {
    /// Computes the bin distribution a store must hold after the given `(index, count)` pairs are added.
    ///
    /// This is the port of the reference suite's `normalize` composed with its `collapsingLowest`/`collapsingHighest`
    /// transforms: drop empty bins, clamp each index into the window the store can represent, then merge duplicates.
    fn expected(self, adds: &[(i32, u64)]) -> Bins {
        let occupied = || adds.iter().filter(|(_, count)| *count > 0).map(|(index, _)| *index);

        // `saturating_*` stands in for the reference's explicit guards against the clamp point overflowing when the
        // occupied range sits at the extreme end of the index space.
        let floor = match self {
            Self::Lowest(max_num_bins) => occupied().max().map(|max| max.saturating_sub(max_num_bins as i32 - 1)),
            _ => None,
        };
        let ceiling = match self {
            Self::Highest(max_num_bins) => occupied().min().map(|min| min.saturating_add(max_num_bins as i32 - 1)),
            _ => None,
        };

        let mut expected = Bins::new();
        for &(index, count) in adds {
            if count == 0 {
                continue;
            }

            let mut index = index;
            if let Some(floor) = floor {
                index = index.max(floor);
            }
            if let Some(ceiling) = ceiling {
                index = index.min(ceiling);
            }

            *expected.entry(index).or_default() += count;
        }

        expected
    }

    /// Returns the store's bin capacity, if it has one.
    fn max_num_bins(self) -> Option<usize> {
        match self {
            Self::None => None,
            Self::Lowest(max_num_bins) | Self::Highest(max_num_bins) => Some(max_num_bins),
        }
    }
}

/// Reads a store's bin distribution out of its protobuf encoding, dropping empty bins.
///
/// `to_proto` is the only view of the raw bins the [`Store`] trait offers, and it's also what the wire format carries,
/// so a disagreement between this and `total_count` is exactly what a downstream consumer would observe. The original
/// report in <https://github.com/DataDog/saluki/issues/2596> was found this way.
fn stored_bins<S: Store>(store: &S) -> Bins {
    let proto = store.to_proto();
    let mut bins = Bins::new();

    for (&index, &count) in &proto.binCounts {
        if count > 0.0 {
            *bins.entry(index).or_default() += count as u64;
        }
    }

    for (offset, &count) in proto.contiguousBinCounts.iter().enumerate() {
        if count > 0.0 {
            *bins.entry(proto.contiguousBinIndexOffset + offset as i32).or_default() += count as u64;
        }
    }

    bins
}

/// Asserts that `store` holds exactly `expected`, across every observable in the [`Store`] contract.
///
/// This is the port of the reference suite's `assertEncodeBins`, plus the rank assertions our trait makes possible.
fn assert_store_matches<S: Store>(store: &S, expected: &Bins, case: &str) {
    let expected_total: u64 = expected.values().sum();

    assert_eq!(store.total_count(), expected_total, "{case}: total count");
    assert_eq!(store.is_empty(), expected_total == 0, "{case}: is_empty");
    assert_eq!(&stored_bins(store), expected, "{case}: bin distribution");

    if expected_total == 0 {
        assert_eq!(store.min_index(), None, "{case}: min index of empty store");
        assert_eq!(store.max_index(), None, "{case}: max index of empty store");
        assert_eq!(store.key_at_rank(0), None, "{case}: rank 0 of empty store");
        return;
    }

    assert_eq!(store.min_index(), expected.keys().next().copied(), "{case}: min index");
    assert_eq!(
        store.max_index(),
        expected.keys().next_back().copied(),
        "{case}: max index"
    );

    // Every rank in `[0, total_count)` must resolve to the bin covering it. The reference samples at most ten bins
    // here to keep its fuzz cases affordable; do the same, but check both ends of each sampled bin's rank range
    // rather than nudging a float rank by an epsilon.
    let stride = (expected.len() / 10).max(1);
    let mut cumulative = 0u64;
    for (position, (&index, &count)) in expected.iter().enumerate() {
        if position % stride == 0 {
            let first = store.key_at_rank(cumulative);
            let last = store.key_at_rank(cumulative + count - 1);
            assert_eq!(
                first,
                Some(index),
                "{case}: rank {cumulative} should fall in bin {index}"
            );
            assert_eq!(
                last,
                Some(index),
                "{case}: rank {} should fall in bin {index}",
                cumulative + count - 1
            );
        }
        cumulative += count;
    }
    assert_eq!(
        store.key_at_rank(expected_total),
        None,
        "{case}: rank past the end must not resolve"
    );
}

/// Asserts that the store's occupied range never exceeds its configured bin capacity.
fn assert_within_capacity<S: Store>(store: &S, collapse: Collapse, case: &str) {
    let (Some(max_num_bins), Some(min), Some(max)) = (collapse.max_num_bins(), store.min_index(), store.max_index())
    else {
        return;
    };

    let span = (max as i64 - min as i64 + 1) as usize;
    assert!(
        span <= max_num_bins,
        "{case}: occupied span {span} exceeds capacity {max_num_bins}"
    );

    // The dense encodings emit their whole allocated window, so this also bounds the allocation itself. The `<=` keeps
    // it honest if the encoding ever starts trimming empty leading/trailing bins.
    let encoded = store.to_proto().contiguousBinCounts.len();
    assert!(
        encoded <= max_num_bins,
        "{case}: encoded {encoded} bins, capacity is {max_num_bins}"
    );
}

/// Re-decodes a store's protobuf encoding into every store type and checks each against its own expectations.
///
/// This is the port of the reference suite's `testEncodingDecoding`: a store's encoding has to be readable by any
/// other store, with that store's own collapsing rules then applied on top.
fn assert_proto_round_trips<S: Store>(store: &S, expected: &Bins, case: &str) {
    let proto = store.to_proto();
    let flattened: Vec<(i32, u64)> = expected.iter().map(|(&index, &count)| (index, count)).collect();

    macro_rules! decode_into {
        ($target:expr, $collapse:expr) => {{
            let collapse = $collapse;
            let mut decoded = $target;
            decoded
                .merge_from_proto(&proto)
                .expect("a store's own encoding must always decode");

            let case = format!("{case} -> {collapse:?}");
            assert_store_matches(&decoded, &collapse.expected(&flattened), &case);
            assert_within_capacity(&decoded, collapse, &case);
        }};
    }

    decode_into!(DenseStore::new(), Collapse::None);
    decode_into!(SparseStore::new(), Collapse::None);
    for max_num_bins in MAX_NUM_BINS {
        decode_into!(
            CollapsingLowestDenseStore::new(max_num_bins),
            Collapse::Lowest(max_num_bins)
        );
        decode_into!(
            CollapsingHighestDenseStore::new(max_num_bins),
            Collapse::Highest(max_num_bins)
        );
    }
}

/// Adds every pair to a fresh store and asserts the full contract against the reference expectation.
fn check_adds<S: Store>(make: impl Fn() -> S, collapse: Collapse, adds: &[(i32, u64)], case: &str) -> (S, Bins) {
    let mut store = make();
    for &(index, count) in adds {
        store.add(index, count);
    }

    let expected = collapse.expected(adds);
    assert_store_matches(&store, &expected, case);
    assert_within_capacity(&store, collapse, case);

    (store, expected)
}

/// A small deterministic generator, standing in for the reference's seeded `math/rand` source.
struct Rng(u64);

impl Rng {
    fn new() -> Self {
        Self(SEED)
    }

    fn next(&mut self) -> u64 {
        // xorshift64*, chosen only for being short and reproducible.
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    /// The reference's `randomIndex`, with the intended `[-1000, 1000)` range.
    fn index(&mut self) -> i32 {
        self.below(2000) as i32 - 1000
    }

    /// The reference's `randomCount`, as a positive integer count.
    fn count(&mut self) -> u64 {
        self.below(10) + 1
    }
}

// ==================== Generic cases, run against every store ====================

/// Port of the reference suite's `TestEmpty`.
fn check_empty<S: Store>(make: impl Fn() -> S, collapse: Collapse) {
    let (store, expected) = check_adds(&make, collapse, &[], "empty");
    assert_proto_round_trips(&store, &expected, "empty");
}

/// Port of the reference suite's `TestAddIntDatasets`.
///
/// The reference also exercises a fractional count of `0.1`; our counts are integers, so that case is dropped rather
/// than rounded, and the remaining counts are checked exactly.
fn check_add_int_datasets<S: Store>(make: impl Fn() -> S, collapse: Collapse) {
    let datasets: [&[i32]; 10] = [
        &[-1000],
        &[-1],
        &[0],
        &[1],
        &[1000],
        &[1000, 1000],
        &[1000, -1000],
        &[-1000, 1000],
        &[-1000, -1000],
        &[0, 0, 0, 0],
    ];

    for dataset in datasets {
        for count in [1u64, 100] {
            let adds: Vec<(i32, u64)> = dataset.iter().map(|&index| (index, count)).collect();
            let case = format!("dataset {dataset:?} x {count}");
            let (store, expected) = check_adds(&make, collapse, &adds, &case);
            assert_proto_round_trips(&store, &expected, &case);
        }
    }
}

/// Port of the reference suite's `TestAddConstant`.
fn check_add_constant<S: Store>(make: impl Fn() -> S, collapse: Collapse) {
    for index in [-1000i32, -1, 0, 1, 1000] {
        for repeats in [0usize, 1, 2, 4, 5, 10, 20, 100, 1000, 10_000] {
            let adds = vec![(index, 1u64); repeats];
            let case = format!("index {index} added {repeats} times");
            let (store, expected) = check_adds(&make, collapse, &adds, &case);

            // Round-tripping every repeat count is pure duplication once the distribution is a single bin, so only
            // the boundary cases go through the encoding.
            if repeats <= 1 {
                assert_proto_round_trips(&store, &expected, &case);
            }
        }
    }
}

/// Port of the reference suite's `TestAddMonotonous`.
///
/// This is the case that pins down window expansion in both directions: a negative increment walks the index space
/// downwards from zero, so a lowest-collapsing store must widen its window down to the bin cap and settle its
/// collapsed floor at `max_index - max_num_bins + 1`, and a positive increment demands the mirror of that from a
/// highest-collapsing store. Getting this wrong leaves the collapsed counts pinned to whichever index arrived first.
fn check_add_monotonous<S: Store>(make: impl Fn() -> S, collapse: Collapse) {
    for increment in [2i32, 10, 100, -2, -10, -100] {
        for spread in [2i32, 10, 10_000] {
            let mut adds = Vec::new();
            let mut index = 0i32;
            while index.abs() <= spread {
                adds.push((index, 1u64));
                index += increment;
            }

            let case = format!("increment {increment} over spread {spread}");
            check_adds(&make, collapse, &adds, &case);
        }
    }
}

/// Port of the reference suite's `TestAddFuzzy` and `TestAddIntFuzzy`.
fn check_add_fuzzy<S: Store>(make: impl Fn() -> S, collapse: Collapse) {
    let mut rng = Rng::new();

    for trial in 0..NUM_TRIALS {
        let values = rng.below(1000) as usize;
        let unit_counts: Vec<(i32, u64)> = (0..values).map(|_| (rng.index(), 1)).collect();
        let varied_counts: Vec<(i32, u64)> = (0..values).map(|_| (rng.index(), rng.count())).collect();

        check_adds(
            &make,
            collapse,
            &unit_counts,
            &format!("fuzzy trial {trial}, unit counts"),
        );
        check_adds(
            &make,
            collapse,
            &varied_counts,
            &format!("fuzzy trial {trial}, varied counts"),
        );
    }
}

/// Port of the reference suite's `TestMergeFuzzy`.
fn check_merge_fuzzy<S: Store>(make: impl Fn() -> S, collapse: Collapse) {
    let mut rng = Rng::new();

    for trial in 0..NUM_TRIALS {
        let mut store = make();
        let mut adds = Vec::new();

        for _ in 0..3 {
            let mut other = make();
            for _ in 0..rng.below(500) {
                let (index, count) = (rng.index(), rng.count());
                adds.push((index, count));
                other.add(index, count);
            }
            store.merge(&other);
        }

        let case = format!("merge trial {trial}");
        assert_store_matches(&store, &collapse.expected(&adds), &case);
        assert_within_capacity(&store, collapse, &case);
    }
}

/// Port of the reference suite's `TestMergeAfterClear`.
fn check_merge_after_clear<S: Store>(make: impl Fn() -> S, collapse: Collapse) {
    let mut first = make();
    let mut second = make();

    for index in 0..1000 {
        second.add(index, 1);
    }
    second.clear();

    first.merge(&second);
    second.merge(&first);

    assert_store_matches(&first, &Bins::new(), "merge after clear, target");
    assert_store_matches(&second, &Bins::new(), "merge after clear, source");
    assert_within_capacity(&first, collapse, "merge after clear, target");
}

/// Instantiates the shared reference suite for one store configuration.
macro_rules! reference_suite {
    ($name:ident, $make:expr, $collapse:expr) => {
        mod $name {
            use super::*;

            #[test]
            fn empty() {
                check_empty($make, $collapse);
            }

            #[test]
            fn add_int_datasets() {
                check_add_int_datasets($make, $collapse);
            }

            #[test]
            fn add_constant() {
                check_add_constant($make, $collapse);
            }

            #[test]
            fn add_monotonous() {
                check_add_monotonous($make, $collapse);
            }

            #[test]
            fn add_fuzzy() {
                check_add_fuzzy($make, $collapse);
            }

            #[test]
            fn merge_fuzzy() {
                check_merge_fuzzy($make, $collapse);
            }

            #[test]
            fn merge_after_clear() {
                check_merge_after_clear($make, $collapse);
            }
        }
    };
}

reference_suite!(dense, DenseStore::new, Collapse::None);
reference_suite!(sparse, SparseStore::new, Collapse::None);
reference_suite!(
    collapsing_lowest_8,
    || CollapsingLowestDenseStore::new(8),
    Collapse::Lowest(8)
);
reference_suite!(
    collapsing_lowest_128,
    || CollapsingLowestDenseStore::new(128),
    Collapse::Lowest(128)
);
reference_suite!(
    collapsing_lowest_1024,
    || CollapsingLowestDenseStore::new(1024),
    Collapse::Lowest(1024)
);
reference_suite!(
    collapsing_highest_8,
    || CollapsingHighestDenseStore::new(8),
    Collapse::Highest(8)
);
reference_suite!(
    collapsing_highest_128,
    || CollapsingHighestDenseStore::new(128),
    Collapse::Highest(128)
);
reference_suite!(
    collapsing_highest_1024,
    || CollapsingHighestDenseStore::new(1024),
    Collapse::Highest(1024)
);

// ==================== Store-specific structural cases ====================

/// Port of the reference suite's `TestCollapsingLowest`.
///
/// Filling twice the capacity must leave the window pinned to the top of the index range. The reference asserts on
/// the private bin array; the observable equivalent through the `Store` trait is the occupied range, which
/// [`assert_within_capacity`] also bounds.
#[test]
fn collapsing_lowest_window_tracks_the_highest_indices() {
    for max_num_bins in MAX_NUM_BINS {
        let mut store = CollapsingLowestDenseStore::new(max_num_bins);
        for index in 0..(2 * max_num_bins) as i32 {
            store.add(index, 1);
        }

        let case = format!("collapsing lowest, capacity {max_num_bins}");
        assert_eq!(store.min_index(), Some(max_num_bins as i32), "{case}: min index");
        assert_eq!(
            store.max_index(),
            Some(2 * max_num_bins as i32 - 1),
            "{case}: max index"
        );
        assert_eq!(store.total_count(), 2 * max_num_bins as u64, "{case}: total count");
        assert!(store.is_collapsed(), "{case}: should have collapsed");
        assert_within_capacity(&store, Collapse::Lowest(max_num_bins), &case);
    }
}

/// Port of the reference suite's `TestCollapsingHighest`.
#[test]
fn collapsing_highest_window_tracks_the_lowest_indices() {
    for max_num_bins in MAX_NUM_BINS {
        let mut store = CollapsingHighestDenseStore::new(max_num_bins);
        for index in 0..(2 * max_num_bins) as i32 {
            store.add(index, 1);
        }

        let case = format!("collapsing highest, capacity {max_num_bins}");
        assert_eq!(store.min_index(), Some(0), "{case}: min index");
        assert_eq!(store.max_index(), Some(max_num_bins as i32 - 1), "{case}: max index");
        assert_eq!(store.total_count(), 2 * max_num_bins as u64, "{case}: total count");
        assert!(store.is_collapsed(), "{case}: should have collapsed");
        assert_within_capacity(&store, Collapse::Highest(max_num_bins), &case);
    }
}

// ==================== Regression cases ====================

/// Regression test for <https://github.com/DataDog/saluki/issues/2596>.
///
/// `grow` used to hand `collapse_lowest` a shift distance measured against `max_num_bins`, which then clamped it to
/// `bins.len() - 1`. When the new index sat at least `max_num_bins` positions above the top of the window, the clamp
/// left the offset short, the index stayed outside the window, and `add` tallied the count in `self.count` without
/// ever writing it to a bin. `total_count` then over-reported, and `key_at_rank` could not resolve the top rank.
///
/// The jump only needs to clear `max_num_bins` index positions, so a store with a small capacity reproduces it just
/// as well as the 2^30-sized value jump in the original report.
#[test]
fn large_upward_jump_keeps_every_count() {
    let mut store = CollapsingLowestDenseStore::new(5);
    store.add(0, 7);
    store.add(10, 3);

    assert_eq!(store.total_count(), 10, "cached count");
    assert_eq!(stored_bins(&store), [(6, 7), (10, 3)].into_iter().collect(), "bins");
    assert_eq!(store.key_at_rank(9), Some(10), "the top rank must resolve");
    assert_eq!(
        store.min_index(),
        Some(6),
        "collapsed floor sits at max_index - max_num_bins + 1"
    );
}

/// Regression test for the downward-expansion half of
/// <https://github.com/DataDog/saluki/issues/2596>.
///
/// A prepend that overflowed the capacity used to return without touching the window, so the collapsed counts stayed
/// at whatever index happened to be lowest at the time. The window now widens down to the capacity first, putting the
/// floor at `max_index - max_num_bins + 1` the way the reference implementation does. Counts were conserved either
/// way; what was wrong was the index they were reported at, and hence every low quantile.
#[test]
fn large_downward_jump_widens_the_window() {
    let mut store = CollapsingLowestDenseStore::new(5);
    store.add(10, 3);
    store.add(0, 7);

    assert_eq!(store.total_count(), 10, "cached count");
    assert_eq!(stored_bins(&store), [(6, 7), (10, 3)].into_iter().collect(), "bins");
    assert_eq!(
        store.min_index(),
        Some(6),
        "collapsed floor sits at max_index - max_num_bins + 1"
    );

    // Insertion order must not change the outcome.
    let mut ascending = CollapsingLowestDenseStore::new(5);
    ascending.add(0, 7);
    ascending.add(10, 3);
    assert_eq!(stored_bins(&store), stored_bins(&ascending), "order independence");
}

/// Mirror of [`large_downward_jump_widens_the_window`] for the highest-collapsing store.
#[test]
fn large_upward_jump_widens_the_highest_window() {
    let mut store = CollapsingHighestDenseStore::new(5);
    store.add(0, 7);
    store.add(10, 3);

    assert_eq!(store.total_count(), 10, "cached count");
    assert_eq!(stored_bins(&store), [(0, 7), (4, 3)].into_iter().collect(), "bins");
    assert_eq!(
        store.max_index(),
        Some(4),
        "collapsed ceiling sits at min_index + max_num_bins - 1"
    );

    let mut descending = CollapsingHighestDenseStore::new(5);
    descending.add(10, 3);
    descending.add(0, 7);
    assert_eq!(stored_bins(&store), stored_bins(&descending), "order independence");
}

// ==================== Properties ====================

proptest! {
    /// Any sequence of adds, in any order, must leave a lowest-collapsing store matching the reference oracle.
    ///
    /// The capacities and index range here are deliberately small relative to each other so that most generated
    /// cases collapse repeatedly and in both directions, which is where <https://github.com/DataDog/saluki/issues/2596>
    /// lived. Order independence is the real content of the property: the oracle is computed from the multiset of
    /// adds alone.
    #[test]
    fn property_test_collapsing_lowest_matches_reference_oracle(
        adds in prop::collection::vec((-2_000i32..2_000, 1u64..10), 0..200),
        max_num_bins in 1usize..64,
    ) {
        let mut store = CollapsingLowestDenseStore::new(max_num_bins);
        for &(index, count) in &adds {
            store.add(index, count);
        }

        let expected = Collapse::Lowest(max_num_bins).expected(&adds);
        let expected_total: u64 = expected.values().sum();

        prop_assert_eq!(stored_bins(&store), expected.clone());
        prop_assert_eq!(store.total_count(), expected_total);
        prop_assert_eq!(store.min_index(), expected.keys().next().copied());
        prop_assert_eq!(store.max_index(), expected.keys().next_back().copied());

        for rank in 0..expected_total {
            prop_assert!(store.key_at_rank(rank).is_some(), "rank {} did not resolve", rank);
        }
        prop_assert_eq!(store.key_at_rank(expected_total), None);
    }

    /// Mirror of the above for the highest-collapsing store.
    #[test]
    fn property_test_collapsing_highest_matches_reference_oracle(
        adds in prop::collection::vec((-2_000i32..2_000, 1u64..10), 0..200),
        max_num_bins in 1usize..64,
    ) {
        let mut store = CollapsingHighestDenseStore::new(max_num_bins);
        for &(index, count) in &adds {
            store.add(index, count);
        }

        let expected = Collapse::Highest(max_num_bins).expected(&adds);
        let expected_total: u64 = expected.values().sum();

        prop_assert_eq!(stored_bins(&store), expected.clone());
        prop_assert_eq!(store.total_count(), expected_total);
        prop_assert_eq!(store.min_index(), expected.keys().next().copied());
        prop_assert_eq!(store.max_index(), expected.keys().next_back().copied());

        for rank in 0..expected_total {
            prop_assert!(store.key_at_rank(rank).is_some(), "rank {} did not resolve", rank);
        }
        prop_assert_eq!(store.key_at_rank(expected_total), None);
    }

    /// Merging must agree with adding every observation to a single store.
    #[test]
    fn property_test_collapsing_merge_agrees_with_direct_adds(
        left in prop::collection::vec((-2_000i32..2_000, 1u64..10), 0..100),
        right in prop::collection::vec((-2_000i32..2_000, 1u64..10), 0..100),
        max_num_bins in 1usize..64,
    ) {
        let mut merged = CollapsingLowestDenseStore::new(max_num_bins);
        let mut other = CollapsingLowestDenseStore::new(max_num_bins);
        for &(index, count) in &left {
            merged.add(index, count);
        }
        for &(index, count) in &right {
            other.add(index, count);
        }
        merged.merge(&other);

        let combined: Vec<(i32, u64)> = left.iter().chain(right.iter()).copied().collect();
        prop_assert_eq!(stored_bins(&merged), Collapse::Lowest(max_num_bins).expected(&combined));
    }
}
