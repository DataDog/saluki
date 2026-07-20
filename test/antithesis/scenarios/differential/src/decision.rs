//! The online decision layer: the clipped signed CUSUM, the per-context verdict, the triage extent,
//! and the single union defect count the always-assertion reads.
//!
//! Every accumulator here is a running/anytime quantity with an absolute bound. Nothing is divided by
//! panel length: the O(1/N) dilution that sank DTW is banned.

use std::cmp::Ordering;

use harness::{CUSUM_H, CUSUM_K_BASE_SCALAR, CUSUM_K_BASE_SKETCH, CUSUM_PHI_MAX, K_MERGE, SAKOE_CHIBA_BAND};

use crate::contexts::{Bucket, BucketValue, SketchValue};
use crate::frechet::{Frontier, Trip};
use crate::ground::{bins_total, quantile_from_bins, Band, RuleContext, RuleSet};

/// The displayed quantiles the sketch channels take a signed CUSUM over. Read off the shared log grid,
/// never a probability-axis average, so no channel is the forbidden 1-Wasserstein dilution.
const DISPLAYED_QUANTILES: [f64; 4] = [0.5, 0.9, 0.95, 0.99];

/// The clipped signed CUSUM over one channel's per-column normalized gap. Both arms are
/// running/anytime quantities that fire at an absolute threshold `h`, never length-normalized.
#[derive(Clone, Copy, Debug)]
pub struct Cusum {
    c_plus: f64,
    c_minus: f64,
    h: f64,
}

impl Cusum {
    /// A CUSUM with firing threshold `h`, both arms zeroed.
    #[must_use]
    pub fn new(h: f64) -> Self {
        Self {
            c_plus: 0.0,
            c_minus: 0.0,
            h,
        }
    }

    /// Folds one column: the clipped normalized gap `u = clip(gap/b, -1, 1)` with trend-aware slack
    /// `k`. `C+ = max(0, C+ + u - k)`, `C- = max(0, C- - u - k)`.
    pub fn update(&mut self, u: f64, k: f64) {
        self.c_plus = (self.c_plus + u - k).max(0.0);
        self.c_minus = (self.c_minus - u - k).max(0.0);
    }

    /// Whether either arm has crossed the firing threshold `h`.
    #[must_use]
    pub fn fired(&self) -> bool {
        self.c_plus > self.h || self.c_minus > self.h
    }

    /// The larger of the two arms, for triage.
    #[must_use]
    pub fn peak(&self) -> f64 {
        self.c_plus.max(self.c_minus)
    }
}

/// Continuous triage magnitude for a context, sorted on `worst_gap` descending. Rides as metadata
/// only; NEVER feeds the verdict.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Extent {
    /// The longest run of consecutive over-band columns.
    pub run_length: u64,
    /// The largest single over-band gap magnitude seen.
    pub worst_gap: f64,
    /// The running sum of over-band gap magnitudes.
    pub accumulated_gap: f64,
}

/// The per-context verdict: whether the context is a defect (any guard-survived sup trip, any CUSUM
/// arm, any absolute-law violation, or a past-horizon presence mismatch) and its triage extent.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContextVerdict {
    /// Whether the context counts as a defect. The union of every trip source, once per context.
    pub tripped: bool,
    /// The mechanisms that fired, for triage.
    pub trips: Vec<Trip>,
    /// Continuous triage magnitude; never decides `tripped`.
    pub extent: Extent,
}

/// The single always-assertion quantity: the number of contexts that tripped. Counts a context once
/// regardless of how many mechanisms fired, and is never length-normalized.
#[must_use]
pub fn defect_count(verdicts: &[ContextVerdict]) -> usize {
    verdicts.iter().filter(|verdict| verdict.tripped).count()
}

/// The trip a surviving sup fold or present/absent bottom carries, forwarded from the aligner.
/// The value-channel CUSUM trip.
const VALUE_CUSUM: &str = "value_cusum";
/// The per-quantile sketch CUSUM trip.
const QUANTILE_CUSUM: &str = "quantile_cusum";
/// A context present on one lane and absent on the other past what the band absorbs.
const PRESENT_ABSENT: &str = "present_absent";
/// A malformed align: the two lanes disagree on a bucket's kind, which is a real divergence.
const ALIGN_ERROR: &str = "align_error";

/// The provisional launch parameters for the online decision: the ddsketch-derived value band, the
/// banded-Frechet aligner, and the CUSUM slacks. All are baked defaults, not calibrated values.
#[derive(Clone, Copy, Debug)]
pub struct DecisionParams {
    /// The per-value band `max(alpha*|v|, floor)` derived from the ddsketch API.
    pub band: Band,
    /// The banded-Frechet aligner: the single coupling the sup verdict reads.
    pub frontier: Frontier,
    /// The CUSUM firing threshold `h`.
    pub h: f64,
    /// The scalar channels' base slack `k_base`.
    pub k_base_scalar: f64,
    /// The sketch mean channel's base slack `k_base`.
    pub k_base_sketch: f64,
    /// The per-quantile channels' merge slack `k_merge`.
    pub k_merge: f64,
    /// The trend-aware slack coefficient `phi_max`.
    pub phi_max: f64,
}

impl DecisionParams {
    /// The launch defaults, reading the provisional harness constants.
    #[must_use]
    pub fn launch() -> Self {
        Self {
            band: Band::ddsketch(),
            frontier: Frontier::new(SAKOE_CHIBA_BAND),
            h: CUSUM_H,
            k_base_scalar: CUSUM_K_BASE_SCALAR,
            k_base_sketch: CUSUM_K_BASE_SKETCH,
            k_merge: K_MERGE,
            phi_max: CUSUM_PHI_MAX,
        }
    }
}

/// Evaluates one context's two lane curves into a [`ContextVerdict`]: the union of every trip source
/// (the guard-survived sup fold, the value and per-quantile CUSUM arms, the present/absent bottom, and
/// the per-lane absolute-law detectors) plus the triage extent that never feeds the verdict.
///
/// `reference` is the normative Agent curve, `sut` the ADP curve. The sup fold reads the single banded
/// coupling the aligner computes. The CUSUM channels fold the per-column signed gap of the positional
/// coupling: at the shared flush interval the two lanes settle equal-length and column-aligned, so the
/// positional gap is the coupled gap; a within-band slide is absorbed by the sup's band and is a
/// transient the anytime CUSUM's slack rides out.
#[must_use]
pub fn evaluate(reference: &[Bucket], sut: &[Bucket], rules: &RuleSet, params: &DecisionParams) -> ContextVerdict {
    if reference.is_empty() || sut.is_empty() {
        return presence_verdict(reference.len().max(sut.len()), params.frontier.band());
    }

    let mut trips = Vec::new();

    // The single aligner: the guard-survived sup fold and the present/absent bottom. Every other
    // mechanism reads the positional coupling, never a second alignment.
    match params.frontier.align(reference, sut, params.band, rules) {
        Ok(verdict) => trips.extend(verdict.trips),
        Err(_) => trips.push(trip(ALIGN_ERROR)),
    }

    // The value channel: counter/rate summed value, gauge last value, sketch mean, each projected to a
    // scalar track. The clipped signed CUSUM subsumes the k=3 debounce and extends to sub-band bias.
    let reference_track = scalar_track(reference);
    let sut_track = scalar_track(sut);
    let is_sketch = reference
        .iter()
        .chain(sut)
        .any(|b| matches!(b.value, BucketValue::Sketch(_)));
    let k_base = if is_sketch {
        params.k_base_sketch
    } else {
        params.k_base_scalar
    };
    if cusum_fires(
        &reference_track,
        &sut_track,
        params.band,
        params.h,
        k_base,
        params.phi_max,
        rules,
        params.frontier.band(),
    ) {
        trips.push(trip(VALUE_CUSUM));
    }

    let extent = extent(&reference_track, &sut_track, params.band);

    if is_sketch {
        // The sketch quantile channels: a signed CUSUM per displayed quantile off the shared grid, its
        // slack the merge tolerance. The strict count gate rides in the aligner's sup as an infinite
        // leash, so no separate count channel repeats it.
        let empty_rules = RuleSet::new();
        for &q in &DISPLAYED_QUANTILES {
            let reference_q = quantile_track(reference, q);
            let sut_q = quantile_track(sut, q);
            if cusum_fires(
                &reference_q,
                &sut_q,
                params.band,
                params.h,
                params.k_merge,
                params.phi_max,
                &empty_rules,
                params.frontier.band(),
            ) {
                trips.push(trip(QUANTILE_CUSUM));
                break;
            }
        }
        // The per-lane absolute laws: each lane's sketch must be internally consistent regardless of
        // the other lane. A violation is add-only, never suppressed.
        if let Some(name) = first_absolute_law_violation(reference, sut, params.band) {
            trips.push(trip(&format!("absolute_law:{name}")));
        }
    }

    ContextVerdict {
        tripped: !trips.is_empty(),
        trips,
        extent,
    }
}

/// A context on one lane only: an infinite-leash present/absent bottom once the present curve carries
/// more buckets than the band can absorb, a transient one-bucket absence otherwise. The extent's
/// infinite worst gap sorts it first in triage.
fn presence_verdict(present_len: usize, band: usize) -> ContextVerdict {
    if present_len > band {
        ContextVerdict {
            tripped: true,
            trips: vec![trip(PRESENT_ABSENT)],
            extent: Extent {
                run_length: present_len as u64,
                worst_gap: f64::INFINITY,
                accumulated_gap: f64::INFINITY,
            },
        }
    } else {
        ContextVerdict::default()
    }
}

/// Folds the clipped signed CUSUM over the two scalar tracks' positional gaps and reports whether either
/// arm crossed `h`. `u_t = clip(gap_t/b_t, -1, 1)`; the slack is trend-aware, `k_t = k_base + phi_max *
/// |a_t - a_{t-1}| / b_t`, so a legitimate reference transition widens the slack rather than tripping.
/// A guard-suppressed over-band column contributes zero, keeping the fold monotone green-ward. A
/// non-finite column (an unknown kind's placeholder) contributes zero and breaks the trend.
fn cusum_fires(
    reference: &[f64], sut: &[f64], band: Band, h: f64, k_base: f64, phi_max: f64, rules: &RuleSet, window: usize,
) -> bool {
    let mut cusum = Cusum::new(h);
    let mut previous: Option<f64> = None;
    for t in 0..reference.len().min(sut.len()) {
        let (a, s) = (reference[t], sut[t]);
        if !a.is_finite() || !s.is_finite() {
            cusum.update(0.0, k_base);
            previous = None;
            continue;
        }
        let b = band.at(a.abs().max(s.abs()));
        let gap = s - a;
        let suppressed = gap.abs() > b
            && rules.suppressed(&RuleContext {
                reference,
                sut,
                ref_index: t,
                sut_index: t,
                band,
                window,
            });
        let u = if suppressed { 0.0 } else { (gap / b).clamp(-1.0, 1.0) };
        let trend = previous.map_or(0.0, |p| phi_max * (a - p).abs() / b);
        cusum.update(u, k_base + trend);
        if cusum.fired() {
            return true;
        }
        previous = Some(a);
    }
    cusum.fired()
}

/// The triage extent over the positional coupling's raw gaps: the longest over-band run, the largest
/// single gap magnitude, and the summed over-band magnitude. Computed over every finite column
/// regardless of guard suppression, so it reflects the raw divergence for triage while never deciding
/// the verdict.
fn extent(reference: &[f64], sut: &[f64], band: Band) -> Extent {
    let mut extent = Extent::default();
    let mut run = 0u64;
    for t in 0..reference.len().min(sut.len()) {
        let (a, s) = (reference[t], sut[t]);
        if !a.is_finite() || !s.is_finite() {
            run = 0;
            continue;
        }
        let gap = (s - a).abs();
        extent.worst_gap = extent.worst_gap.max(gap);
        if gap > band.at(a.abs().max(s.abs())) {
            run += 1;
            extent.run_length = extent.run_length.max(run);
            extent.accumulated_gap += gap;
        } else {
            run = 0;
        }
    }
    extent
}

/// Each bucket projected to the scalar track the value channel and the extent read: a scalar's value,
/// a sketch's mean. An empty sketch folds to zero.
fn scalar_track(buckets: &[Bucket]) -> Vec<f64> {
    buckets
        .iter()
        .map(|bucket| match &bucket.value {
            BucketValue::Scalar(value) => *value,
            BucketValue::Sketch(sketch) if sketch.count > 0 => sketch.sum / sketch.count as f64,
            BucketValue::Sketch(_) => 0.0,
        })
        .collect()
}

/// Each bucket's displayed quantile `q` read off the shared log grid. A scalar or empty sketch column
/// carries a non-finite placeholder the CUSUM treats as a break.
fn quantile_track(buckets: &[Bucket], q: f64) -> Vec<f64> {
    buckets
        .iter()
        .map(|bucket| match &bucket.value {
            BucketValue::Sketch(sketch) => quantile_from_bins(&sketch.bins, q).unwrap_or(f64::NAN),
            BucketValue::Scalar(_) => f64::NAN,
        })
        .collect()
}

/// The first per-lane absolute-law violation on either curve's sketch buckets, or `None` when every
/// sketch is internally consistent. Each law is a within-lane invariant a faithful decode always
/// satisfies, so a violation is a real decode or aggregation break.
fn first_absolute_law_violation(reference: &[Bucket], sut: &[Bucket], band: Band) -> Option<&'static str> {
    reference
        .iter()
        .chain(sut)
        .filter_map(|bucket| match &bucket.value {
            BucketValue::Sketch(sketch) => sketch_law_violation(sketch, band),
            BucketValue::Scalar(_) => None,
        })
        .next()
}

/// The first violated within-lane sketch law: count non-negative, count equal to its bins' total,
/// min at or below max, mean within the band-widened `[min, max]`, and quantile monotonicity across the
/// displayed quantiles.
fn sketch_law_violation(sketch: &SketchValue, band: Band) -> Option<&'static str> {
    if sketch.count < 0 {
        return Some("count_nonneg");
    }
    if bins_total(&sketch.bins) != sketch.count as u64 {
        return Some("count_equals_bins");
    }
    if sketch.count == 0 {
        return None;
    }
    if sketch.min > sketch.max {
        return Some("min_le_max");
    }
    let mean = sketch.sum / sketch.count as f64;
    if mean < sketch.min - band.at(sketch.min.abs()) || mean > sketch.max + band.at(sketch.max.abs()) {
        return Some("mean_in_range");
    }
    let mut previous = f64::NEG_INFINITY;
    for &q in &DISPLAYED_QUANTILES {
        if let Some(value) = quantile_from_bins(&sketch.bins, q) {
            if value + band.at(value.abs()) < previous {
                return Some("quantile_monotone");
            }
            previous = value;
        }
    }
    None
}

/// A trip naming the mechanism that fired.
fn trip(rule: &str) -> Trip {
    Trip { rule: rule.to_string() }
}

/// Orders triage entries by continuous worst gap descending and bounds them to `limit`. The ordering is
/// triage metadata only; it never decides the verdict.
#[must_use]
pub fn bounded_triage<T>(mut entries: Vec<T>, worst_gap: impl Fn(&T) -> f64, limit: usize) -> Vec<T> {
    entries.sort_by(|a, b| worst_gap(b).partial_cmp(&worst_gap(a)).unwrap_or(Ordering::Equal));
    entries.truncate(limit);
    entries
}

#[cfg(test)]
mod tests;
