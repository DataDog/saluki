//! R7: reorder operands of pure `all` / `any`.
//!
//! A compiled predicate evaluates the operands of `all` and `any` in the order the plan writes them, and stops at the
//! first that decides the junction: a false one for `all`, a true one for `any`. R7 rewrites every node's predicates
//! so that, in every junction, the operands that are cheapest to decide come first. One application rewrites one node,
//! the first in plan order whose order changes.
//!
//! # Obligations
//!
//! - **All operands are pure.** Every predicate leaf is pure: it reads the event and the plan's parameters only, so
//!   evaluating operands in another order, and stopping at the first that decides, gives the same result, with no
//!   effect other than the result. A junction with an operand that is not pure is never reordered (its pure
//!   sub-junctions still are), and neither is any predicate of an operator that reads signals.
//! - **The order is deterministic.** Without a profile, operands are ordered by a static cost order, cheapest first:
//!   `const`, `kind_in`, `flag` and `is_empty`, exact matches (`host_unset`, then `name` and `host`, then
//!   `intake_name`, which is normalized first), prefix matches, then tag scans (`has_tag`, then tag values exact, then
//!   by prefix), origin tags like tags. A junction costs the sum of its operands, which is what evaluating it costs
//!   at worst. Ties are broken by canonical form. With an [`OperandProfile`], given explicitly to compilation and never
//!   read from a runtime flag, operands are ordered by their expected cost per decision: the static cost divided by
//!   the observed fraction of evaluations in which the operand decides its junction.
//! - **Structure never depends on configuration values.** The order looks at the form of an operand and its binding
//!   names, never at a binding's resolved value: a `matches` whose mode is bound counts as a prefix match, and
//!   canonical forms are compared without resolved values. A new value of a live binding never changes the rewritten plan.
//!
//! Reordering changes no output and no telemetry: it routes every record as before, and R7 does not rely on the
//! capacity rule.
//!
//! # Static costs
//!
//! The static costs rank leaves by what evaluating one costs when it does not decide early. Measured in a release build
//! over a DogStatsD-like metric (a 27-byte name, ten tags, a host), per event: a constant, a kind test, or a
//! configuration leaf about 1.5 ns; `host_unset` 2; an exact or prefix match of the name or host about 7; of the
//! intake name 17 to 19, since the name is checked for normalization first; `has_tag` 11 when the first tag has the
//! key, 90 when no tag does; a tag value 58 to 63. A tag scan is linear in the number of tags, and R7 assumes it
//! misses, which is when the order matters most, so tags come after every name match.
//!
//! R1's estimates of a `split`'s cost ([`OperatorDef::cost`][crate::operators::OperatorDef::cost]) put a tag
//! predicate at 10 and an intake-name match at 15. They serve another end: ranking whole `split`s against the kernels
//! of a chain, coarsely, to decide where regions are cut, and no `split` of the default program reads tags. R7's order
//! is checked against a hand-written selection baseline by the stage 4 selection bench.
//!
//! # Canonical form
//!
//! The canonical form sorts operands by canonical JSON, so R7, which writes its order into the plan's predicate
//! parameter, changes no canonical form: R2 and the duplicate analysis, which compare canonical forms, see the same
//! predicates. R7 never flattens, deduplicates, or folds operands; it only reorders them. What a compiled program
//! evaluates does change, so a program's [fingerprint][crate::compile::RegionProgram::fingerprint] covers each
//! predicate as written too ([`SharedPredicate::written`][crate::compile::SharedPredicate::written]): a region runs a
//! new order as a new program.

use std::{cmp::Ordering, collections::BTreeMap, fmt};

use super::{Rewrite, RewriteContext, RewriteId, Rewritten};
use crate::{
    canonical::{strip_resolved_values, to_canonical_string},
    ir::{Node, Operand, ParamName, ParamValue, Params, Plan},
    predicate::{Predicate, SetMatch, StringMatch},
};

/// How often one operand matched, among the evaluations a profile observed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Observation {
    /// The number of evaluations observed.
    pub evaluated: u64,

    /// How many of them matched: the operand was true.
    pub matched: u64,
}

/// What R7 knows of how operands behave on real input, to order them by their expected cost to decide.
///
/// An explicit input to compilation ([`RewriteContext::with_profile`]); without one, R7 uses its static cost order.
/// Observations are keyed by the canonical form of the operand without resolved values, so they apply wherever an equal
/// operand appears, and survive a change of a binding's value. An operand without observations is assumed to decide
/// its junction in half of its evaluations.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OperandProfile {
    operands: BTreeMap<String, Observation>,
}

impl OperandProfile {
    /// Creates an empty profile, which orders operands as the static cost order does.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that `operand` was evaluated `evaluated` times and matched `matched` of them, adding to what the profile
    /// observed of it so far.
    ///
    /// `matched` is capped at `evaluated`.
    pub fn observe(&mut self, operand: &Predicate, evaluated: u64, matched: u64) {
        let observation = self.operands.entry(operand_key(operand)).or_default();
        observation.evaluated = observation.evaluated.saturating_add(evaluated);
        observation.matched = observation
            .matched
            .saturating_add(matched.min(evaluated))
            .min(observation.evaluated);
    }

    /// Returns this profile with an observation of `operand` added; see [`observe`][Self::observe].
    pub fn with_observation(mut self, operand: &Predicate, evaluated: u64, matched: u64) -> Self {
        self.observe(operand, evaluated, matched);
        self
    }

    /// Returns what the profile observed of `operand`, if anything.
    pub fn observation(&self, operand: &Predicate) -> Option<Observation> {
        self.operands.get(&operand_key(operand)).copied()
    }

    /// Returns `true` if the profile observed nothing.
    pub fn is_empty(&self) -> bool {
        self.operands.is_empty()
    }
}

/// R7: reorders the operands of pure `all` and `any` junctions, cheapest to decide first.
#[derive(Clone, Copy, Debug, Default)]
pub struct R7;

impl Rewrite for R7 {
    fn id(&self) -> RewriteId {
        RewriteId::R7
    }

    fn apply(&self, ctx: &RewriteContext<'_>, plan: &Plan) -> Option<Rewritten> {
        let order = Order { profile: ctx.profile() };
        plan.nodes.iter().enumerate().find_map(|(position, node)| {
            let (params, junctions) = reorder_node(ctx, &order, node)?;
            if ctx.is_rejected(RewriteId::R7, std::slice::from_ref(&node.id)) {
                return None;
            }
            let mut nodes = plan.nodes.clone();
            nodes[position] = Node { params, ..node.clone() };
            Some(Rewritten {
                plan: Plan {
                    ir_version: plan.ir_version,
                    provenance: plan.provenance.clone(),
                    nodes,
                    edges: plan.edges.clone(),
                    outputs: plan.outputs.clone(),
                    scopes: plan.scopes.clone(),
                    resources: plan.resources.clone(),
                },
                nodes: vec![node.id.clone()],
                justification: format!(
                    "operands pure: every leaf of the predicate of `{}` reads only the event and the plan's \
                     parameters, and its operator reads no signals; order: {} junction{} reordered by {}, so the \
                     operand cheapest to decide is evaluated first; the canonical form, and with it R2 and the \
                     duplicate analysis, is unchanged; capacity: not relied on, and no record is routed differently",
                    node.id,
                    junctions,
                    if junctions == 1 { "" } else { "s" },
                    order,
                ),
                telemetry: Vec::new(),
                capacity_rule: false,
            })
        })
    }
}

/// Returns a node's parameters with every predicate reordered, and the number of junctions whose order changed, if
/// any did.
fn reorder_node(ctx: &RewriteContext<'_>, order: &Order<'_>, node: &Node) -> Option<(Params, usize)> {
    let op = ctx.registry().configure(node.op.as_str(), &node.params).ok()?;
    if op.contract().is_dynamic() {
        return None;
    }
    let mut params = node.params.clone();
    let mut junctions = 0;
    for (name, value) in op.params().iter() {
        let ParamValue::Predicate(predicate) = value else {
            continue;
        };
        let mut changed = 0;
        let ordered = order.apply(predicate, &mut changed);
        if changed > 0 {
            params.insert(ParamName::new(name).ok()?, serde_json::to_value(&ordered).ok()?);
            junctions += changed;
        }
    }
    (junctions > 0).then_some((params, junctions))
}

/// Returns `true` if a predicate is pure: a function of the event and the plan's parameters only.
///
/// Every predicate leaf is pure today. The match is exhaustive so that a leaf that reads anything else, such as a
/// signal, must say so here.
fn is_pure(predicate: &Predicate) -> bool {
    match predicate {
        Predicate::Const(_)
        | Predicate::Flag(_)
        | Predicate::IsEmpty(_)
        | Predicate::Name(_)
        | Predicate::IntakeName(_)
        | Predicate::Host(_)
        | Predicate::HostUnset {}
        | Predicate::HasTag(_)
        | Predicate::Tag(_)
        | Predicate::HasOriginTag(_)
        | Predicate::OriginTag(_)
        | Predicate::KindIn(_) => true,
        Predicate::All(operands) | Predicate::Any(operands) => operands.iter().all(is_pure),
        Predicate::Not(operand) => is_pure(operand),
    }
}

/// Returns the static cost of evaluating a predicate in full: the unit of R7's static order.
///
/// Leaves are ranked as the module documentation lists them; a junction costs the sum of its operands. The cost never
/// depends on a binding's resolved value.
fn static_cost(predicate: &Predicate) -> u32 {
    match predicate {
        Predicate::Const(_) => 1,
        Predicate::KindIn(_) => 2,
        Predicate::Flag(_) | Predicate::IsEmpty(_) => 3,
        Predicate::HostUnset {} => 6,
        Predicate::Name(matcher) | Predicate::Host(matcher) => match_cost(matcher, 8, 16),
        Predicate::IntakeName(matcher) => match_cost(matcher, 12, 20),
        Predicate::HasTag(_) | Predicate::HasOriginTag(_) => 32,
        Predicate::Tag(tag) | Predicate::OriginTag(tag) => match_cost(&tag.value, 40, 48),
        Predicate::All(operands) | Predicate::Any(operands) => {
            operands.iter().map(static_cost).fold(0, u32::saturating_add)
        }
        Predicate::Not(operand) => static_cost(operand),
    }
}

/// Returns the cost of a string match: `exact` for an exact match, `prefix` for a prefix match, including a `matches`
/// whose mode is bound to configuration.
fn match_cost(matcher: &StringMatch, exact: u32, prefix: u32) -> u32 {
    match matcher {
        StringMatch::Eq(_)
        | StringMatch::In(_)
        | StringMatch::Matches(SetMatch {
            prefix: Operand::Literal(false),
            ..
        }) => exact,
        StringMatch::Prefix(_) | StringMatch::PrefixIn(_) | StringMatch::Matches(_) => prefix,
    }
}

/// Returns the key an operand is identified by: its canonical JSON, without resolved values.
fn operand_key(operand: &Predicate) -> String {
    // Predicates contain only strings, booleans, lists, and objects, so this cannot fail; should that change, the
    // debug representation of the canonical form still identifies the operand deterministically.
    let canonical = operand.canonical();
    serde_json::to_value(&canonical)
        .ok()
        .and_then(|mut value| {
            strip_resolved_values(&mut value);
            to_canonical_string(&value).ok()
        })
        .unwrap_or_else(|| format!("{:?}", canonical))
}

/// The order R7 sorts operands into.
struct Order<'a> {
    profile: Option<&'a OperandProfile>,
}

/// One operand of a junction, with what the order compares.
struct Ranked {
    operand: Predicate,
    cost: u32,
    key: String,
    /// The fraction of evaluations in which the operand decides its junction, as `(decided, evaluated)`, with
    /// `evaluated` positive.
    decides: (u64, u64),
}

impl Order<'_> {
    /// Returns `predicate` with the operands of every pure junction in this order, counting the junctions whose order
    /// changed in `changed`.
    fn apply(&self, predicate: &Predicate, changed: &mut usize) -> Predicate {
        match predicate {
            Predicate::All(operands) => Predicate::All(self.junction(operands, false, changed)),
            Predicate::Any(operands) => Predicate::Any(self.junction(operands, true, changed)),
            Predicate::Not(operand) => Predicate::Not(Box::new(self.apply(operand, changed))),
            leaf => leaf.clone(),
        }
    }

    /// Orders the operands of an `all` (`decisive` false) or an `any` (`decisive` true).
    fn junction(&self, operands: &[Predicate], decisive: bool, changed: &mut usize) -> Vec<Predicate> {
        let operands = operands
            .iter()
            .map(|operand| self.apply(operand, changed))
            .collect::<Vec<_>>();
        if !operands.iter().all(is_pure) {
            return operands;
        }
        let mut ranked = operands
            .into_iter()
            .enumerate()
            .map(|(position, operand)| (position, self.rank(operand, decisive)))
            .collect::<Vec<_>>();
        // A stable sort over a total preorder: operands that compare equal keep their order, so sorting an ordered
        // junction again changes nothing.
        ranked.sort_by(|(_, a), (_, b)| self.compare(a, b));
        if ranked
            .iter()
            .enumerate()
            .any(|(index, (position, _))| index != *position)
        {
            *changed += 1;
        }
        ranked.into_iter().map(|(_, ranked)| ranked.operand).collect()
    }

    fn rank(&self, operand: Predicate, decisive: bool) -> Ranked {
        let decides = self
            .profile
            .and_then(|profile| profile.observation(&operand))
            .filter(|observation| observation.evaluated > 0)
            .map_or((1, 2), |observation| {
                let decided = if decisive {
                    observation.matched
                } else {
                    observation.evaluated.saturating_sub(observation.matched)
                };
                scaled(decided, observation.evaluated)
            });
        Ranked {
            cost: static_cost(&operand),
            key: operand_key(&operand),
            decides,
            operand,
        }
    }

    fn compare(&self, a: &Ranked, b: &Ranked) -> Ordering {
        let expected = if self.profile.is_some() {
            expected_cost(a).cmp_rank(&expected_cost(b))
        } else {
            Ordering::Equal
        };
        expected
            .then_with(|| a.cost.cmp(&b.cost))
            .then_with(|| a.key.cmp(&b.key))
    }
}

/// The largest count [`scaled`] keeps, so that comparing two expected costs exactly fits in 128 bits.
const MAX_COUNT: u64 = 1 << 30;

/// Scales `decided` out of `evaluated` down until both are at most [`MAX_COUNT`], keeping their ratio as closely as
/// halving both allows. The result depends on the counts alone, so the order stays deterministic.
fn scaled(mut decided: u64, mut evaluated: u64) -> (u64, u64) {
    while evaluated > MAX_COUNT {
        decided >>= 1;
        evaluated >>= 1;
    }
    (decided, evaluated)
}

/// The expected cost of an operand per decision, `cost * evaluated / decided`, as a fraction, or infinite when it never
/// decided its junction.
struct ExpectedCost {
    numerator: u128,
    denominator: u128,
}

fn expected_cost(ranked: &Ranked) -> ExpectedCost {
    let (decided, evaluated) = ranked.decides;
    ExpectedCost {
        numerator: u128::from(ranked.cost) * u128::from(evaluated),
        denominator: u128::from(decided),
    }
}

impl ExpectedCost {
    /// Compares two expected costs as rational numbers, with every infinite one equal to the others and above every
    /// finite one.
    fn cmp_rank(&self, other: &Self) -> Ordering {
        match (self.denominator, other.denominator) {
            (0, 0) => Ordering::Equal,
            (0, _) => Ordering::Greater,
            (_, 0) => Ordering::Less,
            // Costs are below 2^32 and scaled counts at most 2^30, so each product is below 2^92.
            _ => (self.numerator * other.denominator).cmp(&(other.numerator * self.denominator)),
        }
    }
}

impl fmt::Display for Order<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.profile {
            None => f.write_str(
                "the static cost order (const, kind_in, flag and is_empty, exact matches, prefix matches, tag scans)",
            ),
            Some(_) => f.write_str("the expected cost per decision of the given operand profile"),
        }
    }
}

#[cfg(test)]
mod tests;
