//! Pure comparison core for the differential series oracle.

use std::collections::BTreeMap;

use harness::Phase;

use crate::capture::{MetricKind, SketchValue};
use crate::sketch;

#[cfg(test)]
mod tests;

/// One scalar point flattened out of the store's columns.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Sample {
    pub(crate) timestamp: i64,
    pub(crate) seq: u32,
    pub(crate) interval: u32,
    pub(crate) value: f64,
}

/// A borrowed window onto one context's scalar columns.
///
/// The comparison reads the store's columns in place. Materializing a `Sample` vector per context per
/// lane would allocate twice per context on every poll, for data that already sits contiguous.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ScalarView<'a> {
    pub(crate) timestamps: &'a [i64],
    pub(crate) seqs: &'a [u32],
    pub(crate) intervals: &'a [u32],
    pub(crate) values: &'a [f64],
}

impl<'a> ScalarView<'a> {
    /// Walk the columns as points. Truncates to the shortest column, so a malformed store cannot
    /// index past an end.
    pub(crate) fn iter(self) -> impl Iterator<Item = Sample> + 'a {
        let n = self
            .timestamps
            .len()
            .min(self.seqs.len())
            .min(self.intervals.len())
            .min(self.values.len());
        (0..n).map(move |i| Sample {
            timestamp: self.timestamps[i],
            seq: self.seqs[i],
            interval: self.intervals[i],
            value: self.values[i],
        })
    }

    fn newest(self) -> Option<i64> {
        self.timestamps.iter().copied().max()
    }
}

/// How a timestamp tie is broken before folding.
///
/// The README names three rules. Its POST body carries no way to pick one, so the kind selects it.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Resubmit {
    KeepLast,
    KeepFirst,
    Sum,
}

/// The resubmit rule for `kind`.
///
/// Sum for a count and a sketch, because it finds bugs. A lane that double counts a retry where the other
/// does not then reds, and it matches the sketch merge. Last for a gauge and a rate, whose folds are
/// last-by-timestamp and interval-weighted, so summing two points at one timestamp would report a value
/// neither lane sent.
pub(crate) fn resubmit_rule(kind: MetricKind) -> Resubmit {
    match kind {
        MetricKind::Count | MetricKind::Sketch => Resubmit::Sum,
        MetricKind::Gauge | MetricKind::Rate | MetricKind::Other => Resubmit::KeepLast,
    }
}

/// Collapse points sharing a timestamp, leaving at most one per timestamp, ordered by timestamp.
///
/// `seq` breaks the tie rather than insertion order, so the result does not depend on how the caller
/// walked the columns. `Sum` keeps the least `seq` so the operation is a fixpoint.
pub(crate) fn collapse(points: impl IntoIterator<Item = Sample>, rule: Resubmit) -> Vec<Sample> {
    let mut by_timestamp: BTreeMap<i64, Sample> = BTreeMap::new();
    for point in points {
        by_timestamp
            .entry(point.timestamp)
            .and_modify(|kept| match rule {
                Resubmit::KeepLast => {
                    if point.seq > kept.seq {
                        *kept = point;
                    }
                }
                Resubmit::KeepFirst => {
                    if point.seq < kept.seq {
                        *kept = point;
                    }
                }
                Resubmit::Sum => {
                    kept.value += point.value;
                    kept.seq = kept.seq.min(point.seq);
                }
            })
            .or_insert(point);
    }
    by_timestamp.into_values().collect()
}

/// Collapse, bucket by `k = floor(timestamp / w)`, then fold each bucket by its kind.
///
/// `Other` folds to nothing, so its buckets never appear. `div_euclid` keeps a negative timestamp on
/// the correct side of zero, which truncating division would not.
pub(crate) fn fold_buckets(kind: MetricKind, points: ScalarView<'_>, w: i64, rule: Resubmit) -> BTreeMap<i64, f64> {
    if w <= 0 || kind == MetricKind::Other {
        return BTreeMap::new();
    }
    // `collapse` yields in timestamp order, so a bucket's points are contiguous. Folding as they go
    // avoids holding a vector per bucket.
    let mut folded = BTreeMap::new();
    let mut current: Option<(i64, Accumulator)> = None;
    for point in collapse(points.iter(), rule) {
        let k = point.timestamp.div_euclid(w);
        match &mut current {
            Some((bucket, acc)) if *bucket == k => acc.add(point),
            _ => {
                if let Some((bucket, acc)) = current.take() {
                    if let Some(value) = acc.finish() {
                        folded.insert(bucket, value);
                    }
                }
                let mut acc = Accumulator::new(kind);
                acc.add(point);
                current = Some((k, acc));
            }
        }
    }
    if let Some((bucket, acc)) = current {
        if let Some(value) = acc.finish() {
            folded.insert(bucket, value);
        }
    }
    folded
}

/// A bucket's fold in progress, so a bucket never holds its points.
enum Accumulator {
    Count(f64),
    /// Running weighted mean and total weight, for `sum(value * interval) / sum(interval)`.
    ///
    /// Accumulated as a mean rather than as the quotient of two sums. The boundary pool samples
    /// `f64::MAX`, and weighting one overflows the numerator to an infinity the division cannot undo,
    /// though the answer is representable. `distance` reads two matching infinities as agreement, so
    /// lanes orders apart compared equal.
    Rate(f64, f64),
    /// The latest point seen, ordered by timestamp then `seq`.
    Gauge(Option<(i64, u32, f64)>),
    Dropped,
}

impl Accumulator {
    fn new(kind: MetricKind) -> Self {
        match kind {
            MetricKind::Count => Self::Count(0.0),
            MetricKind::Rate => Self::Rate(0.0, 0.0),
            MetricKind::Gauge => Self::Gauge(None),
            MetricKind::Sketch | MetricKind::Other => Self::Dropped,
        }
    }

    fn add(&mut self, point: Sample) {
        match self {
            Self::Count(total) => *total += point.value,
            Self::Rate(mean, weight) => {
                let w = f64::from(point.interval);
                // A zero-interval point carries no weight, so it moves the mean nowhere and would
                // divide by zero.
                if w == 0.0 {
                    return;
                }
                *weight += w;
                *mean += (point.value - *mean) * (w / *weight);
            }
            Self::Gauge(latest) => {
                let key = (point.timestamp, point.seq);
                if latest.is_none_or(|(ts, seq, _)| key > (ts, seq)) {
                    *latest = Some((point.timestamp, point.seq, point.value));
                }
            }
            Self::Dropped => {}
        }
    }

    fn finish(self) -> Option<f64> {
        match self {
            Self::Count(total) => Some(total),
            Self::Rate(mean, weight) => (weight != 0.0).then_some(mean),
            Self::Gauge(latest) => latest.map(|(_, _, value)| value),
            Self::Dropped => None,
        }
    }
}

/// Materialize `k_start..=k_end` as a dense series, filling the buckets that hold no point.
///
/// Count and rate read zero. Gauge carries the previous value forward, and leads with the first value
/// it has so an unfilled head does not read as a drop to zero. An inverted range yields nothing, which
/// the caller reports as not comparable.
pub(crate) fn fill(kind: MetricKind, folded: &BTreeMap<i64, f64>, k_start: i64, k_end: i64) -> Vec<f64> {
    if k_end < k_start {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(usize::try_from(k_end - k_start + 1).unwrap_or(0));
    let mut carried = folded.range(..=k_start).next_back().map(|(_, &v)| v);
    for k in k_start..=k_end {
        let value = match folded.get(&k) {
            Some(&v) => {
                carried = Some(v);
                v
            }
            None => match kind {
                MetricKind::Gauge => carried.unwrap_or(0.0),
                _ => 0.0,
            },
        };
        out.push(value);
    }
    out
}

/// Why a context yielded no distance. Every skipped context lands in exactly one of these so the
/// totals reconcile and a quiet run cannot read as a healthy one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Skip {
    /// One lane or both folded to nothing.
    NoOverlap,
    /// The lanes overlap but truncation leaves no whole bucket.
    ShortSeries,
    /// A kind the fold table drops.
    KindOther,
    /// The compared range spans more buckets than [`MAX_BUCKET_SPAN`], so filling it would materialize a
    /// vector sized by how long the context lived divided by the bucket width.
    SpanTooWide,
}

/// One context's outcome.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Verdict {
    Distance(f64),
    Skipped(Skip),
    /// The lanes built their sketches on different grids. Their bin keys stand for different values,
    /// so no quantile comparison between them means anything. A failure of equivalence in its own
    /// right, neither a distance nor a skip.
    QuantizationMismatch,
}

/// One sketch point flattened out of the store's columns.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SketchSample {
    pub(crate) timestamp: i64,
    pub(crate) seq: u32,
    pub(crate) value: SketchValue,
}

/// Compare one sketch context by merging each bucket and projecting onto the seven scalar series.
///
/// The grid check runs first. Comparing quantiles across differently quantized lanes would report a
/// number with no meaning behind it.
pub(crate) fn compare_sketch(
    agent: &[SketchSample], adp: &[SketchSample], w: i64, leash: usize, phase: Phase,
) -> Verdict {
    if w <= 0 {
        return Verdict::Skipped(Skip::NoOverlap);
    }
    let a = merge_by_bucket(agent, w);
    let b = merge_by_bucket(adp, w);
    let (Some((&first_a, _)), Some((&first_b, _))) = (a.iter().next(), b.iter().next()) else {
        return Verdict::Skipped(Skip::NoOverlap);
    };
    for (k, left) in &a {
        if let Some(right) = b.get(k) {
            if !sketch::same_quantization(left, right) {
                return Verdict::QuantizationMismatch;
            }
        }
    }

    let newest = |points: &[SketchSample]| points.iter().map(|p| p.timestamp).max().unwrap_or(i64::MIN);
    let Some((k_start, k_end)) = range(first_a.max(first_b), newest(agent).min(newest(adp)), w, leash, phase) else {
        return Verdict::Skipped(Skip::ShortSeries);
    };
    if k_end - k_start >= MAX_BUCKET_SPAN {
        return Verdict::Skipped(Skip::SpanTooWide);
    }

    // Each projected statistic is its own series. The context's distance is the worst of them, so a
    // lane that agrees on count and diverges at p99 is still caught.
    let mut worst = 0.0f64;
    for (pick, fill) in PROJECTIONS {
        let projected = (
            project_series(&a, k_start, k_end, pick, fill),
            project_series(&b, k_start, k_end, pick, fill),
        );
        let (left, right) = match projected {
            (None, None) => continue,
            // One lane yields the statistic and the other does not.
            (Some(_), None) | (None, Some(_)) => return Verdict::Distance(MAX_DISTANCE),
            (Some(left), Some(right)) => (left, right),
        };
        match frechet(&left, &right, leash) {
            Some(distance) => worst = worst.max(distance),
            None => return Verdict::Skipped(Skip::ShortSeries),
        }
    }
    Verdict::Distance(worst)
}

/// How a bucket with no value for a statistic is filled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Fill {
    /// Zero, as the count, sum and extremes series do.
    Zero,
    /// Not filled, so the whole series is not emitted, as the quantile series do.
    Absent,
}

/// The seven statistics a sketch context compares as, read off a projection, each with its fill rule.
type Pick = fn(&sketch::Projection) -> Option<f64>;
const PROJECTIONS: [(Pick, Fill); 7] = [
    (|p| Some(p.count), Fill::Zero),
    (|p| Some(p.sum), Fill::Zero),
    (|p| Some(p.min), Fill::Zero),
    (|p| Some(p.max), Fill::Zero),
    (|p| p.p75, Fill::Absent),
    (|p| p.p95, Fill::Absent),
    (|p| p.p99, Fill::Absent),
];

/// Bucket sketch points and merge each bucket. Points sharing a timestamp merge rather than resubmit,
/// since two sketches at one instant describe one distribution between them.
fn merge_by_bucket(points: &[SketchSample], w: i64) -> BTreeMap<i64, SketchValue> {
    let mut buckets: BTreeMap<i64, Vec<SketchValue>> = BTreeMap::new();
    for point in points {
        buckets
            .entry(point.timestamp.div_euclid(w))
            .or_default()
            .push(point.value.clone());
    }
    buckets
        .into_iter()
        .filter_map(|(k, values)| sketch::merge(&values).map(|m| (k, m)))
        .collect()
}

/// One statistic across `k_start..=k_end`. A bucket with no sketch reads zero, matching the count
/// series fill rule. Yields nothing when the statistic is absent everywhere, which the caller skips.
fn project_series(
    buckets: &BTreeMap<i64, SketchValue>, k_start: i64, k_end: i64, pick: Pick, fill: Fill,
) -> Option<Vec<f64>> {
    let mut out = Vec::with_capacity(usize::try_from(k_end - k_start + 1).unwrap_or(0));
    for k in k_start..=k_end {
        let value = buckets.get(&k).map(sketch::project).and_then(|p| pick(&p));
        match (value, fill) {
            (Some(value), _) => out.push(value),
            (None, Fill::Zero) => out.push(0.0),
            (None, Fill::Absent) => return None,
        }
    }
    Some(out)
}

/// Compare one context across both lanes.
///
/// Truncates to `k_start = max(first bucket on A, first bucket on B)` and
/// `k_end = floor(min(newest_A, newest_B) / w) - 1`, dropping the trailing bucket because it is still
/// filling, then drops a further `leash` buckets from each end before reading the Fréchet measure over
/// the filled series.
///
/// The edge buckets go because the two lanes can put the same input in different buckets, so at the cut one
/// lane counts a point the other placed outside the range. A pinned endpoint charges full distance for that
/// gap.
pub(crate) fn compare(
    kind: MetricKind, agent: ScalarView<'_>, adp: ScalarView<'_>, w: i64, leash: usize, rule: Resubmit, phase: Phase,
) -> Verdict {
    if kind == MetricKind::Other {
        return Verdict::Skipped(Skip::KindOther);
    }
    let a = fold_buckets(kind, agent, w, rule);
    let b = fold_buckets(kind, adp, w, rule);
    let (Some((&first_a, _)), Some((&first_b, _))) = (a.iter().next(), b.iter().next()) else {
        return Verdict::Skipped(Skip::NoOverlap);
    };
    let (Some(newest_a), Some(newest_b)) = (agent.newest(), adp.newest()) else {
        return Verdict::Skipped(Skip::NoOverlap);
    };
    let Some((k_start, k_end)) = range(first_a.max(first_b), newest_a.min(newest_b), w, leash, phase) else {
        return Verdict::Skipped(Skip::ShortSeries);
    };
    if k_end - k_start >= MAX_BUCKET_SPAN {
        return Verdict::Skipped(Skip::SpanTooWide);
    }

    let left = fill(kind, &a, k_start, k_end);
    let right = fill(kind, &b, k_start, k_end);
    match frechet(&left, &right, leash) {
        Some(distance) => Verdict::Distance(distance),
        None => Verdict::Skipped(Skip::ShortSeries),
    }
}

/// The compared bucket range, or `None` when truncation leaves nothing. Drops the leash width from each
/// end, and the newest bucket while load runs, since that one is still filling.
fn range(first: i64, newest: i64, w: i64, leash: usize, phase: Phase) -> Option<(i64, i64)> {
    let margin = i64::try_from(leash).unwrap_or(i64::MAX);
    let filling = match phase {
        Phase::Eventually => 1,
        Phase::Finally => 0,
    };
    let k_start = first.saturating_add(margin);
    let k_end = newest.div_euclid(w).saturating_sub(filling).saturating_sub(margin);
    (k_end >= k_start).then_some((k_start, k_end))
}

/// `|b-a| / max(|a|,|b|)`. The `d(x,x) = 0` case is by definition, covering the shared zero the
/// quotient cannot. Reaches 2 for equal magnitudes of opposite sign, and for a non-finite disagreement.
///
/// Reporting the same non-finite value agrees. Reporting different ones takes the top of the scale.
pub(crate) fn distance(a: f64, b: f64) -> f64 {
    if !a.is_finite() || !b.is_finite() {
        // By bit pattern, since `NaN` never equals itself.
        let same = a.to_bits() == b.to_bits();
        return if same { 0.0 } else { MAX_DISTANCE };
    }
    // An epsilon here would forgive a real difference, so the comparison stays exact.
    #[allow(clippy::float_cmp)]
    if a == b {
        return 0.0;
    }
    let scale = a.abs().max(b.abs());
    (b - a).abs() / scale
}

/// Widest compared range, in buckets. `fill` materializes one entry per bucket, so a long-lived context
/// under a one-second bucket width would size that vector off the run's duration.
pub(crate) const MAX_BUCKET_SPAN: i64 = 4_096;

/// The top of the distance scale, reached by equal magnitudes of opposite sign and by a non-finite
/// disagreement. Any threshold a run would sensibly set is below it.
pub(crate) const MAX_DISTANCE: f64 = 2.0;

/// The Fréchet measure over two equal-length bucket series, both endpoints pinned.
///
/// A walk runs from the first pair to the last, so every bucket of both lanes is matched. A free endpoint
/// lets the walk step over its edge buckets and leave a divergence there unmeasured. At `leash = 1` a lane
/// whose first bucket was a thousandfold off scored 0.0.
///
/// `None` means not comparable, never agreement.
pub(crate) fn frechet(a: &[f64], b: &[f64], leash: usize) -> Option<f64> {
    let n = a.len();
    if n == 0 || b.len() != n {
        return None;
    }

    // Rolling rows, `2 * leash + 1` wide rather than `n`, and never wider than the columns that can
    // exist. An unbounded leash would otherwise size two rows off a caller-supplied number.
    let width = leash.saturating_mul(2).saturating_add(1).min(n);
    let mut prev: Vec<Option<f64>> = vec![None; width];
    let mut cur: Vec<Option<f64>> = vec![None; width];

    // Offset of column `j` in row `i`, `None` when the pair is inadmissible.
    let offset = |i: usize, j: usize| -> Option<usize> {
        let lo = i.saturating_sub(leash);
        (j >= lo && j <= i + leash && j < n).then(|| j - lo)
    };

    let last = n - 1;

    for (i, &ai) in a.iter().enumerate() {
        cur.fill(None);
        let lo = i.saturating_sub(leash);
        let hi = (i + leash).min(last);
        for j in lo..=hi {
            let here = distance(ai, b[j]);
            // The walk starts at the first pair and nowhere else. Starting anywhere on the first row or
            // column would leave the buckets it skipped unmatched.
            let best = if i == 0 && j == 0 {
                Some(0.0)
            } else {
                let up = i.checked_sub(1).and_then(|ip| offset(ip, j)).and_then(|o| prev[o]);
                let diag = i
                    .checked_sub(1)
                    .zip(j.checked_sub(1))
                    .and_then(|(ip, jp)| offset(ip, jp))
                    .and_then(|o| prev[o]);
                let left = j.checked_sub(1).and_then(|jp| offset(i, jp)).and_then(|o| cur[o]);
                [up, diag, left].into_iter().flatten().reduce(f64::min)
            };
            if let Some(best) = best {
                cur[j - lo] = Some(here.max(best));
            }
        }
        std::mem::swap(&mut prev, &mut cur);
    }

    // And it ends at the last pair. `prev` holds the last row after the final swap.
    offset(last, last).and_then(|o| prev[o])
}
