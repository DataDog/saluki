//! The banded discrete-Frechet aligner: sup/bottleneck fold with the J1 refinement.
//!
//! Per context per settled column the aligner computes ONE monotone in-band coupling pi over the
//! Sakoe-Chiba band and folds the sup verdict, applying the [`RuleSet`] to each over-band column. Every
//! downstream mechanism reads this single coupling; nothing runs a second aligner.

use std::fmt;

use crate::contexts::{Bucket, BucketValue};
use crate::ground::{d_v, Band, GroundError, Leash, RuleContext, RuleSet};

/// The mechanism name a surviving sup-fold trip carries.
const SUP_FOLD: &str = "sup_fold";

/// The mechanism name a present/absent bottom trip carries.
const PRESENT_ABSENT_BOTTOM: &str = "present_absent_bottom";

/// A surviving trip on a context's curve: an equivalence violation the guard rules did not suppress.
/// `rule` names the mechanism that fired (the sup fold, a CUSUM arm, or an absolute-law detector).
#[derive(Clone, Debug, PartialEq)]
pub struct Trip {
    /// The name of the mechanism that produced the trip, surfaced in triage details.
    pub rule: String,
}

/// The aligner's verdict for one curve: the trips that survived the guard rules, and the worst leash
/// the sup fold reached along the chosen coupling.
///
/// `worst_leash` is the single coupling reading the decision layer needs: the sup gap and the band that
/// admitted it. It is an anytime quantity, so it stays identical however long the curve grows. The full
/// per-column coupling is deliberately NOT carried here: it would make the verdict length dependent and
/// break the equality the length-independence property rests on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CurveVerdict {
    /// The surviving trips, empty when the two curves are visually equivalent.
    pub trips: Vec<Trip>,
    /// The worst leash the chosen coupling reached: the sup gap and its band. `None` before any column
    /// is folded.
    pub worst_leash: Option<Leash>,
}

impl CurveVerdict {
    /// Whether the curve tripped.
    #[must_use]
    pub fn tripped(&self) -> bool {
        !self.trips.is_empty()
    }
}

/// Why the aligner could not produce a [`CurveVerdict`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AlignerError {
    /// A curve has no settled buckets to align.
    EmptyCurve,
    /// A ground-distance computation failed.
    Ground(GroundError),
    /// The stub has no implementation yet.
    Unimplemented,
}

impl fmt::Display for AlignerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AlignerError::EmptyCurve => write!(f, "a curve has no settled buckets to align"),
            AlignerError::Ground(error) => write!(f, "ground metric failed: {error}"),
            AlignerError::Unimplemented => write!(f, "banded Frechet aligner is not yet implemented"),
        }
    }
}

impl std::error::Error for AlignerError {}

impl From<GroundError> for AlignerError {
    fn from(error: GroundError) -> Self {
        AlignerError::Ground(error)
    }
}

/// The banded discrete-Frechet frontier: the in-band monotone coupling the aligner computes once per
/// context per settled column. Holds the Sakoe-Chiba band half-width in buckets.
#[derive(Clone, Copy, Debug)]
pub struct Frontier {
    band: usize,
}

impl Frontier {
    /// A frontier with the given Sakoe-Chiba band half-width, in buckets.
    #[must_use]
    pub fn new(band: usize) -> Self {
        Self { band }
    }

    /// The Sakoe-Chiba band half-width in buckets.
    #[must_use]
    pub fn band(self) -> usize {
        self.band
    }

    /// Aligns the reference and SUT curves under the band, folds the sup verdict, and applies the rule
    /// set and the J1 envelope guard to each over-band column. Reads the single coupling; never runs a
    /// second aligner.
    ///
    /// The fold is a banded discrete-Frechet sup over the ground metric [`d_v`]. Each coupled column
    /// carries the effective normalized excess `|gap|/band`, zeroed green-ward when the column is within
    /// band or a guard suppresses it. The DP minimizes the worst such excess over every monotone
    /// in-band coupling; the coupling wins green whenever one exists that stays acceptable everywhere.
    /// The frontier holds only the `2B+1` band cells of two rows, so the fold is `O((n+m)*B)` in time
    /// and `O(B)` in space, never the `O(n*m)` blow-up.
    ///
    /// A length mismatch wider than the band is a present/absent bottom: a context on one lane the other
    /// never carried, past what a benign flush-phase slide could absorb. It trips at an infinite leash.
    /// A shorter mismatch is a transient the band absorbs.
    ///
    /// # Errors
    ///
    /// Returns [`AlignerError::EmptyCurve`] when a curve is empty and [`AlignerError::Ground`] when the
    /// ground metric fails on a coupled column.
    pub fn align(
        &self, reference: &[Bucket], sut: &[Bucket], value_band: Band, rules: &RuleSet,
    ) -> Result<CurveVerdict, AlignerError> {
        if reference.is_empty() || sut.is_empty() {
            return Err(AlignerError::EmptyCurve);
        }
        let n = reference.len();
        let m = sut.len();

        // A length mismatch the band cannot absorb is a present/absent bottom: the corner is off band,
        // so no monotone banded coupling reaches it. Trip at an infinite leash before folding.
        if n.abs_diff(m) > self.band {
            return Ok(CurveVerdict {
                trips: vec![Trip {
                    rule: PRESENT_ABSENT_BOTTOM.to_string(),
                }],
                worst_leash: Some(Leash {
                    gap: f64::INFINITY,
                    band: value_band.floor,
                }),
            });
        }

        // The scalar tracks the guard rules read. Sketches fold to their mean so the guard has a
        // comparable level on every kind.
        let reference_track: Vec<f64> = reference.iter().map(|b| project(&b.value)).collect();
        let sut_track: Vec<f64> = sut.iter().map(|b| project(&b.value)).collect();

        // The banded min-max frontier: two rows, each holding only its `2B+1` in-band cells.
        let mut previous: Vec<Option<Cell>> = Vec::new();
        let mut previous_low = 0usize;
        for (i, reference_bucket) in reference.iter().enumerate() {
            let low = i.saturating_sub(self.band);
            let high = (i + self.band).min(m - 1);
            let mut current: Vec<Option<Cell>> = vec![None; high - low + 1];
            for (j, sut_bucket) in sut.iter().enumerate().take(high + 1).skip(low) {
                let leash = d_v(&reference_bucket.value, &sut_bucket.value, value_band)?;
                let excess = self.effective_excess(leash, &reference_track, &sut_track, i, j, value_band, rules);

                // The best predecessor is the reachable neighbor with the smallest worst excess so far.
                let mut best: Option<Cell> = None;
                if i > 0 {
                    best = pick(best, cell_at(&previous, previous_low, j));
                    if j > 0 {
                        best = pick(best, cell_at(&previous, previous_low, j - 1));
                    }
                }
                if j > low {
                    best = pick(best, current[j - 1 - low]);
                }

                current[j - low] = match best {
                    // The corner (0,0) seeds the fold; any other cell with no in-band predecessor is
                    // unreachable and stays `None`.
                    None if i == 0 && j == 0 => Some(Cell { cost: excess, leash }),
                    None => None,
                    // min-max: the path cost is the larger of this column's excess and the best
                    // predecessor's, and the worst leash rides with whichever term won.
                    Some(predecessor) if excess >= predecessor.cost => Some(Cell { cost: excess, leash }),
                    Some(predecessor) => Some(predecessor),
                };
            }
            previous = current;
            previous_low = low;
        }

        let corner = cell_at(&previous, previous_low, m - 1);
        match corner {
            // A monotone banded coupling exists; it trips only if its worst column stays over band after
            // the guards. The sup fold fires once per curve, so the verdict is length independent.
            Some(cell) => {
                let mut trips = Vec::new();
                if cell.cost > 1.0 {
                    trips.push(Trip {
                        rule: SUP_FOLD.to_string(),
                    });
                }
                Ok(CurveVerdict {
                    trips,
                    worst_leash: Some(cell.leash),
                })
            }
            // No coupling reached the corner within the band: a present/absent bottom the width check
            // above did not already catch.
            None => Ok(CurveVerdict {
                trips: vec![Trip {
                    rule: PRESENT_ABSENT_BOTTOM.to_string(),
                }],
                worst_leash: Some(Leash {
                    gap: f64::INFINITY,
                    band: value_band.floor,
                }),
            }),
        }
    }

    /// The column's effective normalized excess: `|gap|/band` when it is over band and neither the rule
    /// set nor the J1 envelope guard suppresses it, and `0` otherwise. Guards are monotone green-ward:
    /// they only ever pull a column's excess to zero, never raise it.
    fn effective_excess(
        &self, leash: Leash, reference_track: &[f64], sut_track: &[f64], reference_index: usize, sut_index: usize,
        value_band: Band, rules: &RuleSet,
    ) -> f64 {
        if !leash.over_band() {
            return 0.0;
        }
        let ctx = RuleContext {
            reference: reference_track,
            sut: sut_track,
            ref_index: reference_index,
            sut_index,
            band: value_band,
            window: self.band,
        };
        if rules.suppressed(&ctx) || j1_envelope_suppresses(&ctx) {
            return 0.0;
        }
        leash.gap.abs() / leash.band
    }
}

/// One cell of the banded frontier: the min-max path cost to here and the worst leash that path met.
#[derive(Clone, Copy)]
struct Cell {
    cost: f64,
    leash: Leash,
}

/// The lower-cost of two candidate cells, treating an absent cell as unreachable.
fn pick(current: Option<Cell>, candidate: Option<Cell>) -> Option<Cell> {
    match (current, candidate) {
        (Some(a), Some(b)) => Some(if b.cost < a.cost { b } else { a }),
        (Some(a), None) => Some(a),
        (None, other) => other,
    }
}

/// The band cell at column `j` of a stored row that begins at `low`, or `None` when `j` is off band.
fn cell_at(row: &[Option<Cell>], low: usize, j: usize) -> Option<Cell> {
    j.checked_sub(low).and_then(|offset| row.get(offset).copied().flatten())
}

/// The scalar level a bucket value presents to the guard: the value itself for a scalar, the sketch
/// mean for a sketch. An empty sketch folds to zero.
fn project(value: &BucketValue) -> f64 {
    match value {
        BucketValue::Scalar(scalar) => *scalar,
        BucketValue::Sketch(sketch) if sketch.count != 0 => sketch.sum / sketch.count as f64,
        BucketValue::Sketch(_) => 0.0,
    }
}

/// The J1 matched-jump plus envelope-containment guard, monotone green-ward. It suppresses an over-band
/// trip at a level transition only when both lanes jump the same sign and matched magnitude, the pre and
/// post levels agree across lanes, and the whole `+/-window` on both lanes lies inside the two levels'
/// band-widened envelope. A jump only one lane makes, a mismatched magnitude, or an interior spike that
/// escapes the envelope all leave the trip standing.
fn j1_envelope_suppresses(ctx: &RuleContext<'_>) -> bool {
    let (i, j) = (ctx.ref_index, ctx.sut_index);
    if i == 0 || j == 0 {
        return false;
    }
    let (Some(&r0), Some(&r1)) = (ctx.reference.get(i - 1), ctx.reference.get(i)) else {
        return false;
    };
    let (Some(&s0), Some(&s1)) = (ctx.sut.get(j - 1), ctx.sut.get(j)) else {
        return false;
    };
    if ![r0, r1, s0, s1].iter().all(|level| level.is_finite()) {
        return false;
    }

    // Both lanes must jump the same nonzero sign.
    let delta_reference = r1 - r0;
    let delta_sut = s1 - s0;
    if delta_reference == 0.0 || delta_sut == 0.0 || (delta_reference > 0.0) != (delta_sut > 0.0) {
        return false;
    }

    // Matched magnitude: the two jumps agree within the band at the larger jump.
    let jump = delta_reference.abs().max(delta_sut.abs());
    if (delta_reference - delta_sut).abs() > ctx.band.at(jump) {
        return false;
    }

    // Pre and post levels agree across lanes.
    if (r0 - s0).abs() > ctx.band.at(r0.abs().max(s0.abs())) {
        return false;
    }
    if (r1 - s1).abs() > ctx.band.at(r1.abs().max(s1.abs())) {
        return false;
    }

    // Envelope containment: every bucket in the `+/-window` on both lanes lies inside the two levels
    // widened by the band. An interior spike breaches the ceiling and defeats the guard.
    let low_level = r0.min(r1).min(s0).min(s1);
    let high_level = r0.max(r1).max(s0).max(s1);
    let margin = ctx.band.at(high_level.abs().max(low_level.abs()));
    let (floor, ceiling) = (low_level - margin, high_level + margin);
    window_contained(ctx.reference, i, ctx.window, floor, ceiling)
        && window_contained(ctx.sut, j, ctx.window, floor, ceiling)
}

/// Whether every finite bucket in the `+/-window` around `center` on `track` lies in `[floor, ceiling]`.
/// A non-finite bucket is a presence concern handled elsewhere, so it does not defeat containment.
fn window_contained(track: &[f64], center: usize, window: usize, floor: f64, ceiling: f64) -> bool {
    let low = center.saturating_sub(window);
    let high = (center + window).min(track.len().saturating_sub(1));
    (low..=high).all(|index| {
        let value = track[index];
        !value.is_finite() || (floor..=ceiling).contains(&value)
    })
}

#[cfg(test)]
mod tests;
