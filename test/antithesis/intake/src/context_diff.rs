//! Pure symmetric difference for the differential contexts oracle.

use std::cmp::Ordering;
use std::sync::Arc;

use crate::capture::{Context, EpochSeconds, Target};

#[cfg(test)]
mod tests;

/// A context on one lane and not the other, aged against the intake's clock.
///
/// `lane` names where the context was observed and nothing more. A context on the Agent lane alone
/// means ADP did not emit it. A context on the ADP lane alone means the Agent did not emit it. Which
/// side is at fault is a triage judgment this report does not make, since either lane can be the one
/// misbehaving.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Diverging<'a> {
    pub(crate) lane: Target,
    pub(crate) context: &'a Context,
    pub(crate) age_secs: i64,
}

/// One lane's cumulative set, ordered by context.
pub(crate) type LaneSet<'a> = [(&'a Arc<Context>, EpochSeconds)];

/// Symmetric difference of the two lanes, walking both in order rather than building a set.
///
/// Both inputs come from a `BTreeMap`, so they arrive sorted by context and the merge is linear with
/// no intermediate allocation beyond the result.
pub(crate) fn difference<'a>(agent: &LaneSet<'a>, adp: &LaneSet<'a>, now: EpochSeconds) -> Vec<Diverging<'a>> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    let age = |seen: EpochSeconds| now.secs() - seen.secs();

    while i < agent.len() || j < adp.len() {
        let ordering = match (agent.get(i), adp.get(j)) {
            (Some((a, _)), Some((b, _))) => a.as_ref().cmp(b.as_ref()),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => break,
        };
        match ordering {
            Ordering::Less => {
                let (context, seen) = agent[i];
                out.push(Diverging {
                    lane: Target::Agent,
                    context,
                    age_secs: age(seen),
                });
                i += 1;
            }
            Ordering::Greater => {
                let (context, seen) = adp[j];
                out.push(Diverging {
                    lane: Target::Adp,
                    context,
                    age_secs: age(seen),
                });
                j += 1;
            }
            // Present on both lanes, so not a divergence.
            Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }
    out
}

/// How many members have sat in the difference longer than `budget` seconds.
pub(crate) fn overdue(difference: &[Diverging<'_>], budget: i64) -> usize {
    difference.iter().filter(|d| d.age_secs > budget).count()
}
