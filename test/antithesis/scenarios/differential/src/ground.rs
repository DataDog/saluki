//! The per-kind ground metric and the operator-extensible named-rule layer.
//!
//! The ground metric `d_v` reduces a coupled column pair to a signed [`Leash`]: the value gap and the
//! [`Band`] that admits benign difference at that value. The [`RuleSet`] holds the named guard rules
//! that may suppress an over-band trip; rules are monotone green-ward and never introduce a trip.

use std::fmt;

use ddsketch::canonical::IndexMapping;
use ddsketch::DDSketch;

use crate::contexts::{BucketValue, SketchValue};

/// The signed vertical gap between the SUT value and its coupled reference value at one column, paired
/// with the band that admits benign difference there. `over_band` when the gap exceeds the band.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Leash {
    /// `gap_t = s_t - a_{pi(t)}`, signed.
    pub gap: f64,
    /// `b_t = max(alpha*|v|, floor)`.
    pub band: f64,
}

impl Leash {
    /// Whether the gap exceeds its band, `|gap| > band`.
    #[must_use]
    pub fn over_band(self) -> bool {
        self.gap.abs() > self.band
    }
}

/// A per-value band `max(alpha*|v|, floor)`. `alpha` is the ddsketch relative accuracy and `floor` its
/// smallest representable magnitude; both derive from the ddsketch public API, never hardcoded.
#[derive(Clone, Copy, Debug)]
pub struct Band {
    /// The ddsketch relative accuracy.
    pub alpha: f64,
    /// The ddsketch value floor.
    pub floor: f64,
}

impl Band {
    /// The band derived entirely from the ddsketch public API: `alpha` is the agent sketch mapping's
    /// relative accuracy and `floor` its smallest representable magnitude, `value_for_key(1)`. Neither
    /// is a hardcoded numeric literal.
    #[must_use]
    pub fn ddsketch() -> Self {
        Self {
            alpha: DDSketch::remap_mapping().relative_accuracy(),
            floor: DDSketch::value_for_key(1),
        }
    }

    /// The band admitted at `value`: `max(alpha*|value|, floor)`.
    #[must_use]
    pub fn at(self, value: f64) -> f64 {
        (self.alpha * value.abs()).max(self.floor)
    }
}

/// Why a ground-distance computation could not produce a [`Leash`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GroundError {
    /// The reference and SUT values are different kinds, so no ground distance is defined.
    KindMismatch,
    /// The stub has no implementation yet.
    Unimplemented,
}

impl fmt::Display for GroundError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GroundError::KindMismatch => write!(f, "reference and SUT bucket values are different kinds"),
            GroundError::Unimplemented => write!(f, "ground metric is not yet implemented"),
        }
    }
}

impl std::error::Error for GroundError {}

/// The ground distance at one coupled column: the signed value gap and the band that admits benign
/// difference. Per kind the value fold differs -- counter/rate summed, gauge last, sketch mean -- and
/// the caller passes the already-folded value pair, so this stays kind-driven only in how it reads.
///
/// # Errors
///
/// Returns [`GroundError::KindMismatch`] when the two values are different kinds, and
/// [`GroundError::Unimplemented`] while the metric is a stub.
pub fn d_v(reference: &BucketValue, sut: &BucketValue, band: Band) -> Result<Leash, GroundError> {
    match (reference, sut) {
        (BucketValue::Scalar(a), BucketValue::Scalar(s)) => Ok(scalar_leash(*a, *s, band)),
        (BucketValue::Sketch(a), BucketValue::Sketch(s)) => Ok(sketch_leash(a, s, band)),
        _ => Err(GroundError::KindMismatch),
    }
}

/// The displayed quantiles the sketch channel takes a sup over. Reading these off the shared log grid
/// keeps the sketch verdict a bottleneck over the visible curve rather than a probability-axis average.
const DISPLAYED_QUANTILES: [f64; 4] = [0.5, 0.9, 0.95, 0.99];

/// The scalar ground distance: the signed value gap and the band admitted at the larger of the two
/// magnitudes, so the leash is symmetric under swapping the lanes.
fn scalar_leash(reference: f64, sut: f64, band: Band) -> Leash {
    Leash {
        gap: sut - reference,
        band: band.at(reference.abs().max(sut.abs())),
    }
}

/// The sketch ground distance: a strict total-count gate then a sup over the displayed quantiles.
///
/// The total count is summed from the bins on both sides; any mismatch snaps to an infinite leash so a
/// count divergence always trips regardless of quantile agreement. Otherwise each displayed quantile is
/// read off the shared log grid and compared against its own per-value band, and the worst normalized
/// excess wins the sup. Identical bins give a zero gap.
fn sketch_leash(reference: &SketchValue, sut: &SketchValue, band: Band) -> Leash {
    if bins_total(&reference.bins) != bins_total(&sut.bins) {
        return Leash {
            gap: f64::INFINITY,
            band: band.at(0.0),
        };
    }
    let mut worst = Leash {
        gap: 0.0,
        band: band.at(0.0),
    };
    let mut worst_excess = 0.0;
    for &q in &DISPLAYED_QUANTILES {
        let (Some(reference_q), Some(sut_q)) =
            (quantile_from_bins(&reference.bins, q), quantile_from_bins(&sut.bins, q))
        else {
            continue;
        };
        let gap = sut_q - reference_q;
        let quantile_band = band.at(reference_q.abs().max(sut_q.abs()));
        let excess = gap.abs() / quantile_band;
        if excess > worst_excess {
            worst_excess = excess;
            worst = Leash {
                gap,
                band: quantile_band,
            };
        }
    }
    worst
}

/// The total sample count a sketch's bins carry, summed on the shared grid.
pub(crate) fn bins_total(bins: &[(i32, u32)]) -> u64 {
    bins.iter().map(|(_, count)| u64::from(*count)).sum()
}

/// The representative value the displayed quantile `q` reads off a sketch's log-grid bins. Walks the
/// key-sorted bins accumulating count and returns the crossing bin's representative value from the
/// shared grid via the ddsketch public API. `None` when the bins carry no samples.
pub(crate) fn quantile_from_bins(bins: &[(i32, u32)], q: f64) -> Option<f64> {
    let total = bins_total(bins);
    if total == 0 {
        return None;
    }
    let wanted = q * total as f64;
    let mut cumulative = 0u64;
    let mut last_key = None;
    for &(key, count) in bins {
        cumulative += u64::from(count);
        last_key = Some(key);
        if cumulative as f64 >= wanted {
            return Some(value_for_key(key));
        }
    }
    last_key.map(value_for_key)
}

/// The representative value the shared log grid assigns a bin key, via the ddsketch public API. Agent
/// sketch keys are `i16`; a key outside that range saturates rather than panicking. The negative bound
/// stops at `i16::MIN + 1` because the ddsketch key space negates the key, and negating `i16::MIN`
/// overflows.
fn value_for_key(key: i32) -> f64 {
    let clamped = key.clamp(i32::from(i16::MIN) + 1, i32::from(i16::MAX));
    let key = i16::try_from(clamped).unwrap_or(i16::MAX);
    DDSketch::value_for_key(key)
}

/// The read-only view a [`GroundRule`] inspects to decide suppression at one coupled column: both
/// lanes' scalar value tracks, the coupled indices, the band, and the envelope half-window `B`.
#[derive(Clone, Copy, Debug)]
pub struct RuleContext<'a> {
    /// The reference (Agent) curve's per-bucket scalar track.
    pub reference: &'a [f64],
    /// The SUT (ADP) curve's per-bucket scalar track.
    pub sut: &'a [f64],
    /// The reference column index the coupling selected.
    pub ref_index: usize,
    /// The SUT column index the coupling selected.
    pub sut_index: usize,
    /// The band settings in force.
    pub band: Band,
    /// The envelope half-window `B`, in buckets.
    pub window: usize,
}

/// A named equivalence rule that may suppress an over-band trip. Rules are monotone green-ward: a rule
/// only ever removes a trip, never adds one. The J1 matched-jump + envelope-containment guard is one.
pub trait GroundRule {
    /// The rule's stable name, surfaced in triage details.
    fn name(&self) -> &'static str;

    /// Whether this rule suppresses the trip at `ctx`. Must never turn a green column red.
    fn suppresses(&self, ctx: &RuleContext<'_>) -> bool;
}

/// The ordered set of named guard rules applied to each over-band column. A trip survives only if no
/// registered rule suppresses it.
#[derive(Default)]
pub struct RuleSet {
    rules: Vec<Box<dyn GroundRule>>,
}

impl RuleSet {
    /// An empty rule set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a rule.
    pub fn register(&mut self, rule: Box<dyn GroundRule>) {
        self.rules.push(rule);
    }

    /// Whether any registered rule suppresses the trip at `ctx`.
    #[must_use]
    pub fn suppressed(&self, ctx: &RuleContext<'_>) -> bool {
        self.rules.iter().any(|rule| rule.suppresses(ctx))
    }

    /// The three initial guard rules, in registration order: the missing-counter-zero equivalence, the
    /// unknown-kind presence-only pass, and the sketch adjacent-bin quantization tolerance. Each only
    /// ever suppresses a trip, so the set only widens `same`.
    #[must_use]
    pub fn default_rules() -> Self {
        let mut set = Self::new();
        set.register(Box::new(MissingCounterZero));
        set.register(Box::new(UnknownKindPresenceOnly));
        set.register(Box::new(SketchAdjacentBinQuantization));
        set
    }

    /// The registered rules' names, in registration order.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.rules.iter().map(|rule| rule.name()).collect()
    }
}

/// A counter bucket that never flushed reads as a missing column, which the aligned track carries as a
/// zero. This rule suppresses an over-band trip when one coupled lane is zero and the same value the
/// other lane carries also appears on the zero lane within the `+/-window`: the counter's increment
/// merely landed in an adjacent flush bucket, a benign phase slide, not lost or invented data.
struct MissingCounterZero;

impl GroundRule for MissingCounterZero {
    fn name(&self) -> &'static str {
        "missing_counter_zero"
    }

    fn suppresses(&self, ctx: &RuleContext<'_>) -> bool {
        let (Some(&reference), Some(&sut)) = (ctx.reference.get(ctx.ref_index), ctx.sut.get(ctx.sut_index)) else {
            return false;
        };
        if !reference.is_finite() || !sut.is_finite() {
            return false;
        }
        let (zero_lane, value, value_index) = if reference == 0.0 && sut != 0.0 {
            (ctx.reference, sut, ctx.ref_index)
        } else if sut == 0.0 && reference != 0.0 {
            (ctx.sut, reference, ctx.sut_index)
        } else {
            return false;
        };
        if zero_lane.is_empty() {
            return false;
        }
        let tolerance = ctx.band.at(value.abs());
        let low = value_index.saturating_sub(ctx.window);
        let high = (value_index + ctx.window).min(zero_lane.len() - 1);
        (low..=high).any(|i| (zero_lane[i] - value).abs() <= tolerance)
    }
}

/// An unknown-kind (`Other`) context has no defined value fold, so its aligned track carries a
/// non-finite placeholder rather than a comparable scalar. Presence is checked elsewhere, so this rule
/// suppresses any value trip at a column where either lane's value is non-finite. Known-kind tracks are
/// finite, so it never fires on them.
struct UnknownKindPresenceOnly;

impl GroundRule for UnknownKindPresenceOnly {
    fn name(&self) -> &'static str {
        "unknown_kind_presence_only"
    }

    fn suppresses(&self, ctx: &RuleContext<'_>) -> bool {
        match (ctx.reference.get(ctx.ref_index), ctx.sut.get(ctx.sut_index)) {
            (Some(&reference), Some(&sut)) => !reference.is_finite() || !sut.is_finite(),
            _ => false,
        }
    }
}

/// DDSketch quantization can place identical underlying data one bin apart across the two lanes. This
/// rule suppresses a trip when the coupled values are same-signed and at most one adjacent bin apart on
/// the shared log grid, read via the ddsketch public API. A larger gap is a real quantile divergence.
struct SketchAdjacentBinQuantization;

impl GroundRule for SketchAdjacentBinQuantization {
    fn name(&self) -> &'static str {
        "sketch_adjacent_bin_quantization"
    }

    fn suppresses(&self, ctx: &RuleContext<'_>) -> bool {
        let (Some(&reference), Some(&sut)) = (ctx.reference.get(ctx.ref_index), ctx.sut.get(ctx.sut_index)) else {
            return false;
        };
        if !reference.is_finite() || !sut.is_finite() {
            return false;
        }
        if reference == sut {
            return true;
        }
        let same_sign = (reference >= 0.0) == (sut >= 0.0);
        if !same_sign {
            return false;
        }
        let mapping = DDSketch::remap_mapping();
        let floor = ctx.band.floor;
        let reference_key = mapping.index(reference.abs().max(floor));
        let sut_key = mapping.index(sut.abs().max(floor));
        (reference_key - sut_key).abs() <= 1
    }
}

#[cfg(test)]
mod tests;
