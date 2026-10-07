//! Shared numerics for detectors, ported from the Agent's `metrics_detector_util.go`.
//!
//! This is the **single** statistics module that every detector reuses. The Go source file it ports also
//! contains storage plumbing (series-listing/status helpers and their optimization interfaces); those are
//! deliberately **not** ported here because they depend on the Go `StorageReader` interface and belong to
//! the store/detector cards that own those traits, not to the numerics layer. The ported helpers are:
//!
//! * [`median`] / [`mad`] — the free `detectorMedian` / `detectorMAD` helpers.
//! * [`median_point_interval`], [`median_timestamp_interval`] — median gap between timestamps.
//! * [`rank_biserial_correlation`], [`normal_cdf_upper`] — effect size and tail probability used by the
//!   Mann-Whitney and Welch scan detectors.
//! * [`append_point_window`] — bounded tail retention for a point buffer.
//! * [`parse_aggregate_suffix`] / [`parse_aggregate_config`] — aggregate-name parsing.
//! * [`ScanDetectorWorkspace`] — reusable per-detector scratch space with the Go workspace's
//!   `valuesFromPoints`, `assignRanks`, `median`, `mad`, and `medianPointInterval` methods.
//! * [`SCAN_MAX_POINTS`] — the Go `scanMaxPoints` constant.
//!
//! # Numeric fidelity
//!
//! Arithmetic-only helpers replicate the Go operation order exactly, so their results are bit-identical to
//! the Go reference and are asserted with `assert_eq!` against values produced by the Go functions.
//! [`normal_cdf_upper`] uses `exp`/`sqrt`, whose last-ulp result can differ between Go's and Rust's math
//! libraries; its fixtures are therefore asserted with an explicit `1e-12` tolerance.
//!
//! Non-finite values are rejected upstream by storage, so the sorts here order with
//! `partial_cmp(..).expect(..)` rather than Go's NaN-tolerant comparator: a NaN panics loudly instead of
//! producing the unspecified ordering Go's `sort.Slice` would.

use std::f64::consts::PI;

use crate::identity::Aggregate;
use crate::model::Point;

/// Maximum number of points a scan-based detector keeps per series (Go `scanMaxPoints`).
pub const SCAN_MAX_POINTS: usize = 120;

/// Returns the median of `values`, without modifying the input.
///
/// Returns `0.0` for an empty slice, matching Go's `detectorMedian`. For an even-length slice the two
/// central values are averaged.
pub fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sort_ascending(&mut sorted);
    median_of_sorted(&sorted)
}

/// Returns the median absolute deviation of `values` from `center`.
///
/// `mad = median(|x_i - center|)`. When `scale_to_sigma` is `true` the result is multiplied by `1.4826` to
/// estimate the standard deviation of normally distributed data. Use `true` when comparing against
/// sigma-based thresholds and `false` when using raw MAD as a relative-change denominator (for example the
/// scan detectors' pre-change MAD checks). Returns `0.0` for an empty slice.
pub fn mad(values: &[f64], center: f64, scale_to_sigma: bool) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut abs_devs: Vec<f64> = values.iter().map(|value| (value - center).abs()).collect();
    sort_ascending(&mut abs_devs);
    let result = median_of_sorted(&abs_devs);
    if scale_to_sigma {
        result * 1.4826
    } else {
        result
    }
}

/// Returns the median gap between consecutive point timestamps, in seconds.
///
/// Returns `0` when there are fewer than two points. The intervals are sorted, so this is `O(n log n)`; `n`
/// is typically 30–100, where the sort is negligible.
pub fn median_point_interval(points: &[Point]) -> i64 {
    if points.len() < 2 {
        return 0;
    }
    let mut intervals: Vec<i64> = point_intervals(points).collect();
    intervals.sort_unstable();
    median_of_sorted_i64(&intervals)
}

/// Returns the median gap between consecutive timestamps, in seconds.
///
/// This is the timestamp-array equivalent of [`median_point_interval`], for detectors that keep a compact
/// timestamp ring instead of retaining [`Point`]s. Returns `0` when there are fewer than two timestamps.
pub fn median_timestamp_interval(timestamps: &[i64]) -> i64 {
    if timestamps.len() < 2 {
        return 0;
    }
    let mut intervals: Vec<i64> = timestamp_intervals(timestamps).collect();
    intervals.sort_unstable();
    median_of_sorted_i64(&intervals)
}

/// Computes the rank-biserial correlation from a Mann-Whitney `u` statistic.
///
/// The result ranges from `-1` to `1`. Returns `0.0` when either sample is empty, matching Go's
/// `rankBiserialCorrelation`.
pub fn rank_biserial_correlation(u: f64, n1: usize, n2: usize) -> f64 {
    let fn1 = n1 as f64;
    let fn2 = n2 as f64;
    let product = fn1 * fn2;
    if product == 0.0 {
        return 0.0;
    }
    1.0 - 2.0 * u / product
}

/// Computes `P(Z > z)` for a standard normal variable using the Abramowitz & Stegun 26.2.17 rational
/// approximation.
///
/// For negative `z` the symmetry `P(Z > z) = 1 - P(Z > -z)` is applied first. This is the port of the Go
/// `normalCDFUpper` used by the scan detectors for p-values.
pub fn normal_cdf_upper(z: f64) -> f64 {
    if z < 0.0 {
        return 1.0 - normal_cdf_upper(-z);
    }
    // Rational approximation (Abramowitz & Stegun 26.2.17).
    const P: f64 = 0.2316419;
    const B1: f64 = 0.319381530;
    const B2: f64 = -0.356563782;
    const B3: f64 = 1.781477937;
    const B4: f64 = -1.821255978;
    const B5: f64 = 1.330274429;
    let t = 1.0 / (1.0 + P * z);
    let t2 = t * t;
    let t3 = t2 * t;
    let t4 = t3 * t;
    let t5 = t4 * t;
    let phi = (-z * z / 2.0).exp() / (2.0 * PI).sqrt();
    phi * (B1 * t + B2 * t2 + B3 * t3 + B4 * t4 + B5 * t5)
}

/// Appends `point` to `buf`, retaining only the newest `max_points` points.
///
/// Once `buf` is full, the oldest point is dropped and the rest shift down, so the buffer always holds a
/// bounded tail. `max_points` must be non-zero; the Go original assumes the same.
pub fn append_point_window(buf: &mut Vec<Point>, max_points: usize, point: Point) {
    if buf.len() < max_points {
        buf.push(point);
        return;
    }
    if buf.is_empty() {
        return;
    }
    buf.copy_within(1.., 0);
    let last = buf.len() - 1;
    buf[last] = point;
}

/// Parses a single aggregate suffix (`avg`, `sum`, `count`) into an [`Aggregate`].
///
/// Returns `None` for any other string, matching the Go `parseAggregateSuffix`.
pub fn parse_aggregate_suffix(s: &str) -> Option<Aggregate> {
    match s {
        "avg" => Some(Aggregate::Average),
        "sum" => Some(Aggregate::Sum),
        "count" => Some(Aggregate::Count),
        _ => None,
    }
}

/// Parses a list of aggregate suffixes, dropping any that are not recognized.
///
/// Returns an empty vector when `names` is empty, matching the Go `parseAggregateConfig` (which returns a
/// nil slice).
pub fn parse_aggregate_config(names: &[String]) -> Vec<Aggregate> {
    names.iter().filter_map(|name| parse_aggregate_suffix(name)).collect()
}

/// Reusable, bounded scratch space for a scan detector's per-series statistics.
///
/// Detectors are single-writer, so one workspace can be reused across series and aggregations instead of
/// allocating buffers proportional to every scan. This is the Rust counterpart of the Go
/// `scanDetectorWorkspace`; the `reuse*` capacity helpers from the Go file are subsumed by the interior
/// [`Vec`]s, which keep their capacity across calls.
///
/// Because a returned slice borrows the workspace, callers that need the values *and* a later mutable call
/// should copy the values out first, for example `workspace.values_from_points(points).to_vec()`.
#[derive(Debug, Default)]
pub struct ScanDetectorWorkspace {
    values: Vec<f64>,
    ranks: Vec<f64>,
    indexed: Vec<IndexedValue>,
    sort_scratch: Vec<f64>,
    intervals: Vec<i64>,
}

/// A value paired with its position, used to rank values while remembering where each came from.
#[derive(Clone, Copy, Debug)]
struct IndexedValue {
    value: f64,
    index: usize,
}

impl ScanDetectorWorkspace {
    /// Creates an empty workspace.
    pub fn new() -> Self {
        Self::default()
    }

    /// Refills the internal values buffer with the point values and returns it.
    pub fn values_from_points(&mut self, points: &[Point]) -> &[f64] {
        self.values.clear();
        self.values.extend(points.iter().map(|point| point.value));
        &self.values
    }

    /// Assigns 1-based average ranks to `values`, returning the ranks in input order and the tie
    /// correction `sum(t^3 - t)` over tie groups of size `t`.
    ///
    /// Ties share their average rank, matching the Go `assignRanks` used for the Mann-Whitney and Welch
    /// variance corrections.
    pub fn assign_ranks(&mut self, values: &[f64]) -> (&[f64], f64) {
        let n = values.len();
        self.indexed.clear();
        self.indexed.extend(
            values
                .iter()
                .enumerate()
                .map(|(index, &value)| IndexedValue { value, index }),
        );
        self.indexed
            .sort_by(|a, b| a.value.partial_cmp(&b.value).expect("rank values must not be NaN"));

        self.ranks.clear();
        self.ranks.resize(n, 0.0);
        let mut tie_correction = 0.0;
        let mut i = 0;
        while i < n {
            let mut j = i;
            while j < n && self.indexed[j].value == self.indexed[i].value {
                j += 1;
            }
            let avg_rank = (i + 1 + j) as f64 / 2.0;
            let tie_size = (j - i) as f64;
            for k in i..j {
                self.ranks[self.indexed[k].index] = avg_rank;
            }
            tie_correction += tie_size * tie_size * tie_size - tie_size;
            i = j;
        }
        (&self.ranks, tie_correction)
    }

    /// Returns the median of `values`, reusing the internal scratch buffer.
    pub fn median(&mut self, values: &[f64]) -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        self.sort_scratch.clear();
        self.sort_scratch.extend_from_slice(values);
        sort_ascending(&mut self.sort_scratch);
        median_of_sorted(&self.sort_scratch)
    }

    /// Returns the unscaled median absolute deviation of `values` from `center`, reusing the scratch buffer.
    ///
    /// This matches the Go workspace `mad`, which has no sigma-scaling flag; use the free [`mad`] when the
    /// `1.4826` scale is wanted.
    pub fn mad(&mut self, values: &[f64], center: f64) -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        self.sort_scratch.clear();
        self.sort_scratch
            .extend(values.iter().map(|value| (value - center).abs()));
        sort_ascending(&mut self.sort_scratch);
        median_of_sorted(&self.sort_scratch)
    }

    /// Returns the median gap between consecutive point timestamps, reusing the intervals buffer.
    pub fn median_point_interval(&mut self, points: &[Point]) -> i64 {
        if points.len() < 2 {
            return 0;
        }
        self.intervals.clear();
        self.intervals.extend(point_intervals(points));
        self.intervals.sort_unstable();
        median_of_sorted_i64(&self.intervals)
    }
}

/// Sorts in ascending order, panicking on NaN so non-finite input is caught rather than silently misordered.
fn sort_ascending(values: &mut [f64]) {
    values.sort_by(|a, b| a.partial_cmp(b).expect("sorted values must not be NaN"));
}

/// Returns the median of an already-sorted slice; `0.0` when empty.
fn median_of_sorted(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n == 0 {
        return 0.0;
    }
    if n.is_multiple_of(2) {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    } else {
        sorted[n / 2]
    }
}

/// Returns the median of an already-sorted slice of integers.
fn median_of_sorted_i64(sorted: &[i64]) -> i64 {
    sorted[sorted.len() / 2]
}

/// Lazily yields the gap between each pair of consecutive point timestamps.
fn point_intervals(points: &[Point]) -> impl Iterator<Item = i64> + '_ {
    points.windows(2).map(|window| window[1].second - window[0].second)
}

/// Lazily yields the gap between each pair of consecutive timestamps.
fn timestamp_intervals(timestamps: &[i64]) -> impl Iterator<Item = i64> + '_ {
    timestamps.windows(2).map(|window| window[1] - window[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference values below are produced by running the verbatim Go helper bodies from
    /// `metrics_detector_util.go` and printing `%.17g`; arithmetic-only helpers are asserted bit-exact.
    fn points(values: &[i64]) -> Vec<Point> {
        values.iter().map(|&second| Point { second, value: 0.0 }).collect()
    }

    #[test]
    fn scan_max_points_matches_go() {
        assert_eq!(SCAN_MAX_POINTS, 120);
    }

    #[test]
    fn median_matches_go_fixtures() {
        assert_eq!(median(&[]), 0.0);
        assert_eq!(median(&[5.0]), 5.0);
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
        assert_eq!(median(&[7.0, 1.0, 3.0, 2.0, 9.0, 8.0]), 5.0);
        assert_eq!(median(&[-5.0, -1.0, -3.0]), -3.0);
    }

    #[test]
    fn mad_matches_go_fixtures_and_does_not_modify_input() {
        let input = [1.0, 2.0, 3.0, 4.0];
        // Go: mad([1,2,3,4], 2.5, true) -> 1.4825999999999999
        assert_eq!(mad(&input, 2.5, true), 1.482_6);
        // Go: mad([1,2,3,4,100], 3, true) -> 1.4825999999999999
        assert_eq!(mad(&[1.0, 2.0, 3.0, 4.0, 100.0], 3.0, true), 1.482_6);
        // Go: mad([1,2,3,4], 2.5, false) -> 1
        assert_eq!(mad(&input, 2.5, false), 1.0);
        // Go: mad([1,2,3,4,100], 3, false) -> 1
        assert_eq!(mad(&[1.0, 2.0, 3.0, 4.0, 100.0], 3.0, false), 1.0);
        // Go: mad([7], 7, false) -> 0
        assert_eq!(mad(&[7.0], 7.0, false), 0.0);
        // Go: mad([]) -> 0
        assert_eq!(mad(&[], 0.0, false), 0.0);
        // The input is untouched (Go's detectorMAD copies before sorting).
        assert_eq!(input, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn median_point_interval_matches_go_fixtures() {
        assert_eq!(median_point_interval(&[]), 0);
        assert_eq!(median_point_interval(&points(&[1])), 0);
        // Gaps 2, 3 -> median 3.
        assert_eq!(median_point_interval(&points(&[1, 3, 6])), 3);
        // Gaps 10, 20, 30 -> median 20.
        assert_eq!(median_point_interval(&points(&[0, 10, 30, 60])), 20);
        // Gaps 0, 0 -> median 0.
        assert_eq!(median_point_interval(&points(&[5, 5, 5])), 0);
    }

    #[test]
    fn median_point_interval_ignores_point_values() {
        let with_values = vec![
            Point { second: 0, value: 1.0 },
            Point {
                second: 10,
                value: 99.0,
            },
            Point {
                second: 30,
                value: -4.0,
            },
            Point { second: 60, value: 7.0 },
        ];
        assert_eq!(median_point_interval(&with_values), 20);
    }

    #[test]
    fn median_timestamp_interval_matches_go_fixtures() {
        assert_eq!(median_timestamp_interval(&[]), 0);
        assert_eq!(median_timestamp_interval(&[1]), 0);
        assert_eq!(median_timestamp_interval(&[1, 3, 6]), 3);
        assert_eq!(median_timestamp_interval(&[0, 10, 30, 60]), 20);
    }

    #[test]
    fn rank_biserial_correlation_matches_go_fixtures() {
        assert_eq!(rank_biserial_correlation(0.0, 3, 4), 1.0);
        assert_eq!(rank_biserial_correlation(6.0, 3, 4), 0.0);
        assert_eq!(rank_biserial_correlation(12.0, 3, 4), -1.0);
        assert_eq!(rank_biserial_correlation(5.0, 0, 4), 0.0);
        assert_eq!(rank_biserial_correlation(5.0, 3, 0), 0.0);
        // Go: rbc(7.5, 5, 5) -> 0.40000000000000002
        assert_eq!(rank_biserial_correlation(7.5, 5, 5), 0.4);
    }

    #[test]
    fn normal_cdf_upper_matches_go_fixtures() {
        // Go `math.Exp`/`math.Sqrt` and Rust's libm may differ in the last ulp, so a 1e-12 absolute
        // tolerance is documented per case.
        // Written as the shortest round-tripping f64 literals for the `%.17g` outputs of the Go helpers.
        let fixtures = [
            (0.0, 0.49999999947519136),
            (0.5, 0.30853753221267505),
            (1.0, 0.158_655_259_563_131_6),
            (1.96, 0.024997825156635605),
            (2.5, 0.006_209_679_853_494_86),
            (3.0, 0.0013499672222351908),
            (4.5, 3.400_803_060_269_187e-6),
            (-1.96, 0.975_002_174_843_364_4),
            (-3.0, 0.998_650_032_777_764_8),
        ];
        for (z, expected) in fixtures {
            let actual = normal_cdf_upper(z);
            assert!(
                (actual - expected).abs() <= 1e-12,
                "normal_cdf_upper({z}) = {actual}, expected {expected} within 1e-12"
            );
        }
        // The negative branch is the exact complement of the positive branch.
        assert_eq!(normal_cdf_upper(1.96) + normal_cdf_upper(-1.96), 1.0);
    }

    #[test]
    fn append_point_window_matches_go_test() {
        // Ported verbatim from Go `TestAppendPointWindow`: five points through a buffer capped at three
        // retain the last three.
        let mut buffer: Vec<Point> = Vec::with_capacity(3);
        for second in 1..=5 {
            append_point_window(&mut buffer, 3, Point { second, value: 0.0 });
        }
        assert_eq!(buffer, points(&[3, 4, 5]));
    }

    #[test]
    fn append_point_window_preserves_values() {
        let mut buffer: Vec<Point> = Vec::new();
        append_point_window(&mut buffer, 2, Point { second: 1, value: 1.5 });
        append_point_window(&mut buffer, 2, Point { second: 2, value: 2.5 });
        append_point_window(&mut buffer, 2, Point { second: 3, value: 3.5 });
        assert_eq!(
            buffer,
            vec![Point { second: 2, value: 2.5 }, Point { second: 3, value: 3.5 },]
        );
    }

    #[test]
    fn parse_aggregate_suffix_matches_go() {
        assert_eq!(parse_aggregate_suffix("avg"), Some(Aggregate::Average));
        assert_eq!(parse_aggregate_suffix("sum"), Some(Aggregate::Sum));
        assert_eq!(parse_aggregate_suffix("count"), Some(Aggregate::Count));
        assert_eq!(parse_aggregate_suffix("none"), None);
        assert_eq!(parse_aggregate_suffix(""), None);
        assert_eq!(parse_aggregate_suffix("AVG"), None);
    }

    #[test]
    fn parse_aggregate_config_matches_go() {
        let names: Vec<String> = ["avg", "sum", "bogus", "count"]
            .iter()
            .map(|name| name.to_string())
            .collect();
        assert_eq!(
            parse_aggregate_config(&names),
            vec![Aggregate::Average, Aggregate::Sum, Aggregate::Count]
        );
        assert_eq!(parse_aggregate_config(&[]), Vec::new());
        assert_eq!(parse_aggregate_config(&["nope".to_string()]), Vec::new());
    }

    #[test]
    fn assign_ranks_matches_go_fixtures() {
        let mut workspace = ScanDetectorWorkspace::new();

        let (ranks, tie_correction) = workspace.assign_ranks(&[3.0, 1.0, 2.0]);
        assert_eq!(ranks, [3.0, 1.0, 2.0]);
        assert_eq!(tie_correction, 0.0);

        // Go: ranks([10,10,20,30,30,30]) = [1.5, 1.5, 3, 5, 5, 5], tie correction 30.
        let (ranks, tie_correction) = workspace.assign_ranks(&[10.0, 10.0, 20.0, 30.0, 30.0, 30.0]);
        assert_eq!(ranks, [1.5, 1.5, 3.0, 5.0, 5.0, 5.0]);
        assert_eq!(tie_correction, 30.0);

        let (ranks, tie_correction) = workspace.assign_ranks(&[]);
        assert!(ranks.is_empty());
        assert_eq!(tie_correction, 0.0);
    }

    #[test]
    fn assign_ranks_reuses_buffer() {
        let mut workspace = ScanDetectorWorkspace::new();
        let _ = workspace.assign_ranks(&[10.0, 10.0, 20.0, 30.0, 30.0, 30.0]);
        // A shorter second call must not leak the previous, longer ranks.
        let (ranks, tie_correction) = workspace.assign_ranks(&[5.0, 5.0]);
        assert_eq!(ranks, [1.5, 1.5]);
        assert_eq!(tie_correction, 6.0);
    }

    #[test]
    fn workspace_statistics_match_free_helpers() {
        let mut workspace = ScanDetectorWorkspace::new();
        let values = [7.0, 1.0, 3.0, 2.0, 9.0, 8.0];

        assert_eq!(workspace.median(&values), median(&values));

        let center = workspace.median(&values);
        assert_eq!(workspace.mad(&values, center), mad(&values, center, false));
    }

    #[test]
    fn workspace_values_from_points_matches_point_values() {
        let mut workspace = ScanDetectorWorkspace::new();
        let points = vec![Point { second: 1, value: 1.5 }, Point { second: 2, value: -2.0 }];
        let values = workspace.values_from_points(&points).to_vec();
        assert_eq!(values, [1.5, -2.0]);
        // A second call replaces the buffer rather than appending to it.
        let values = workspace.values_from_points(&points[..1]).to_vec();
        assert_eq!(values, [1.5]);
    }

    #[test]
    fn workspace_median_point_interval_matches_free_helper() {
        let mut workspace = ScanDetectorWorkspace::new();
        let sample = points(&[0, 10, 30, 60]);
        assert_eq!(workspace.median_point_interval(&sample), median_point_interval(&sample));
        let single = points(&[1]);
        assert_eq!(workspace.median_point_interval(&single), 0);
        assert_eq!(workspace.median_point_interval(&[]), 0);
    }

    #[test]
    fn workspace_handles_empty_values() {
        let mut workspace = ScanDetectorWorkspace::new();
        assert_eq!(workspace.median(&[]), 0.0);
        assert_eq!(workspace.mad(&[], 0.0), 0.0);
    }
}
