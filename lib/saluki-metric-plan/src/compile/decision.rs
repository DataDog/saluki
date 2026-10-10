//! Decisions: groups of `split` and `tee` nodes, compiled to one function from an event to its exit set.
//!
//! A **decision** is a maximal connected group of `split` and `tee` nodes within one slot's program, with no
//! intervening map that writes a field one of its predicates reads. It compiles to one pure function from an event to
//! its **exit set**: the set of the decision's [exits][ExitTarget] the event leaves by, each an output port of one of
//! the group's nodes that leads outside the group. A [`Step::Decide`][super::Step::Decide] computes each event's exit
//! set, then moves the events into their exits' buffers: the original into its last destination, copies into the
//! others. There is no per-event graph walk in the steps.
//!
//! A decision is a tree: it contains no `merge`, so every event reaches each node of the group at most once, through
//! one path, and the events that leave a node's port are exactly those whose exit set meets the exits below that port.
//! That makes the per-node, per-port counts telemetry needs derivable from counts per exit set ([`ExitSetCounts`],
//! [`Decision::port_counts`]), exactly, off the data path.
//!
//! The specification fixes the function's semantics, not its form: the reference interpreter's literal walk over the
//! group's nodes is the semantics, and any form that matches it is valid. [`Decision::evaluate`] is that walk.
//! [`DecisionFunction`] is the form regions run: straight-line code with forward jumps only, in which each event runs
//! each instruction at most once, every exit set that does not depend on the event is precomputed, and a decision with
//! one live predicate is a single test.
//!
//! # Grouping
//!
//! Decision compilation groups the `split` and `tee` nodes of a slot that are joined directly by edges. Every other
//! node ends a group: a `merge`, because a decision is a tree, and every map, whether or not it writes a field a
//! predicate below it reads, because the events below a map are the map's output, which a decision computed on its
//! input could not route (a map may drop, add, or change events). Within a group nothing writes a field, so the
//! specification's condition on intervening maps holds trivially. A group whose exits would exceed
//! [`EXIT_SET_CAPACITY`] is cut at node boundaries, in depth-first order: a node that would take the group over the
//! capacity starts a decision of its own.
//!
//! # Rewrites at decision compilation
//!
//! - **R2, shared predicate results.** The decision's [predicate table][PredicateTable] holds each predicate of its
//!   `split`s once, by canonical form with resolved values, so an event evaluates each distinct predicate at most once.
//!   Within a decision nothing writes a field between two of its predicates, so equal predicates have equal results.
//! - **R3, bound constants.** A predicate that is constant given its resolved values, such as a `flag` or an
//!   `is_empty` over a bound list, is folded inside the compiled function ([`fold_bound_constants`],
//!   [`SharedPredicate::folded`]). The decision's exits stay: an exit no event can take is idle. Structure never
//!   depends on a binding's value; only the program of a generation does.
//! - **R4, memoization.** When every predicate of a decision reads only a record's context fields and its kind
//!   ([`Decision::memoizable`]), the function's result depends only on the context and the kind, so a region may cache
//!   it per context identity plus kind in a [`DecisionMemo`]: bounded, direct-mapped, owned by one generation's
//!   program in one region, and never allocating on the data path. Keying by the 64-bit context identity reproduces
//!   native collision behavior. Predicates that read signals are never memoized. Decision compilation memoizes a
//!   memoizable decision ([`Decision::memoized`]) when the `split`s it evaluates, those R3 did not fold, are estimated
//!   to cost at least [`MEMO_COST_FLOOR`] by R1's cost model: a cheaper decision, such as one on a name, the kind, or
//!   one intake-name match, saves too little on a hit to make up for what a miss adds, unless most lookups hit.
//!
//! Each of these is recorded on the decision ([`Decision::rewrites`]), so that `explain` lists it.

use std::{fmt, sync::Arc};

use saluki_core::data_model::event::{metric::Metric, Event};
use snafu::Snafu;

use super::rewrite::{RewriteId, RewriteOutcome, RewriteRecord};
use crate::{
    ir::{BindingName, NodeId, PortName},
    limits::values::DECISION_MEMO_ENTRIES,
    model::{EventView, Field, FieldSet, Kind},
    predicate::{CompiledPredicate, Predicate},
};

/// The most exits a decision has: the width of an [`ExitSet`].
///
/// As many as the nodes of the largest artifact, so that a `tee` at the fan-out limit and the `split`s below it fit
/// many times over. Grouping cuts a group that would have more exits into several decisions, at node boundaries;
/// compiling a single node with more output ports than this fails, as a program with more registers than a program
/// numbers does.
pub const EXIT_SET_CAPACITY: usize = crate::limits::values::ARTIFACT_NODES as usize;

const EXIT_SET_WORDS: usize = EXIT_SET_CAPACITY.div_ceil(64);

/// The most distinct exit sets a region counts for one decision between two publications of its telemetry.
///
/// A decision whose events take more distinct exit sets than this between publications derives its per-port counts
/// early, which costs a pass over the counted sets, and counts on: nothing is lost.
pub const COUNTED_EXIT_SETS: usize = 64;

/// The least estimated cost, in nanoseconds per event by R1's cost model ([`OperatorDef::cost`]), of the `split`s a
/// decision evaluates for decision compilation to memoize it (R4): that of a `split` on two intake-name leaves, such as
/// an endpoint allowlist's names and prefixes (25, the intake name normalized once), but not one leaf (20) or one leaf
/// and the kind (21). R1 prices a decision with the same floor (`operators::decision_cost`), so that the frame is cut
/// by what regions run.
///
/// A lookup that hits costs about 10 ns, and one that misses adds about 4 ns to the evaluation (the probe of an entry
/// that is rarely in the nearest cache), whatever the decision (`memo_misses` in `benches/selection.rs`). At a hit
/// rate `p`, memoizing a decision that takes `t` ns to evaluate pays when `p * (t - 10) > (1 - p) * 4`. An endpoint
/// allowlist (25 by the cost model, about 25 ns measured) pays from a hit rate of about one in five; a `split` on one
/// intake-name leaf (20, about 18 ns) only from about one in three, and with every lookup a miss, as for a
/// pre-aggregation filter over more contexts than the memo holds, it would take about a fifth longer than evaluating.
/// The memo also takes memory: 96 KiB for a decision of up to 64 exits.
///
/// [`OperatorDef::cost`]: crate::operators::OperatorDef::cost
pub const MEMO_COST_FLOOR: u32 = 25;

/// A multiplier with well-mixed bits, for hashing exit sets and memo keys.
const MIX: u64 = 0x9E37_79B9_7F4A_7C15;

index!(
    /// The index of one exit of a decision, in the decision's exit order.
    DecisionExit
);

index!(
    /// The index of a predicate in a decision's predicate table.
    PredicateIndex
);

/// A set of a decision's exits: a fixed-width bitset of [`EXIT_SET_CAPACITY`] bits.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExitSet([u64; EXIT_SET_WORDS]);

impl ExitSet {
    /// The empty set: an event that leaves by no exit, such as one every path drops.
    pub const EMPTY: Self = Self([0; EXIT_SET_WORDS]);

    /// Returns the set of the first `count` exits, at most [`EXIT_SET_CAPACITY`].
    pub fn first(count: usize) -> Self {
        let mut set = Self::EMPTY;
        for exit in 0..count.min(EXIT_SET_CAPACITY) {
            set.0[exit / 64] |= 1 << (exit % 64);
        }
        set
    }

    /// Returns the set of one exit.
    pub fn single(exit: DecisionExit) -> Self {
        let mut set = Self::EMPTY;
        set.insert(exit);
        set
    }

    /// Adds an exit.
    ///
    /// Decision compilation numbers exits below [`EXIT_SET_CAPACITY`], so every exit fits.
    pub fn insert(&mut self, exit: DecisionExit) {
        let exit = exit.get();
        debug_assert!(
            exit < EXIT_SET_CAPACITY,
            "decision exits are numbered below the capacity"
        );
        if let Some(word) = self.0.get_mut(exit / 64) {
            *word |= 1 << (exit % 64);
        }
    }

    /// Returns `true` if the set contains `exit`.
    pub fn contains(&self, exit: DecisionExit) -> bool {
        let exit = exit.get();
        self.0.get(exit / 64).is_some_and(|word| word & (1 << (exit % 64)) != 0)
    }

    /// Returns the exits in either set.
    pub fn union(mut self, other: Self) -> Self {
        for (word, other) in self.0.iter_mut().zip(other.0) {
            *word |= other;
        }
        self
    }

    /// Returns the exits in both sets.
    pub fn intersection(mut self, other: Self) -> Self {
        for (word, other) in self.0.iter_mut().zip(other.0) {
            *word &= other;
        }
        self
    }

    /// Returns `true` if the two sets share an exit.
    pub fn intersects(&self, other: &Self) -> bool {
        self.0.iter().zip(other.0).any(|(word, other)| word & other != 0)
    }

    /// Returns `true` if the set has no exit.
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|word| *word == 0)
    }

    /// Returns the number of exits in the set.
    pub fn len(&self) -> usize {
        self.0.iter().map(|word| word.count_ones() as usize).sum()
    }

    /// Returns the exit of the set that comes last in exit order, if it has any.
    pub fn last(&self) -> Option<DecisionExit> {
        self.0
            .iter()
            .enumerate()
            .rev()
            .find(|(_, word)| **word != 0)
            .map(|(index, word)| {
                // At most `EXIT_SET_CAPACITY - 1`, which an exit index numbers.
                DecisionExit::new((index * 64 + 63 - word.leading_zeros() as usize) as u16)
            })
    }

    /// Returns the exits in the set, in increasing order.
    ///
    /// Each step skips every absent exit at once, so iterating costs a few operations per exit in the set, whatever
    /// the set's width.
    pub fn iter(&self) -> impl Iterator<Item = DecisionExit> {
        ExitSetIter {
            words: self.0,
            index: 0,
        }
    }

    /// Returns a well-mixed hash of the set.
    fn mixed(&self) -> u64 {
        let folded = self
            .0
            .iter()
            .enumerate()
            .fold(0u64, |hash, (index, word)| hash ^ word.rotate_left(index as u32 * 17));
        folded.wrapping_mul(MIX)
    }
}

/// The iterator of [`ExitSet::iter`].
struct ExitSetIter {
    words: [u64; EXIT_SET_WORDS],
    index: usize,
}

impl Iterator for ExitSetIter {
    type Item = DecisionExit;

    fn next(&mut self) -> Option<DecisionExit> {
        while let Some(word) = self.words.get_mut(self.index) {
            if *word == 0 {
                self.index += 1;
                continue;
            }
            let bit = word.trailing_zeros() as usize;
            *word &= *word - 1;
            // At most `EXIT_SET_CAPACITY - 1`, which an exit index numbers.
            return Some(DecisionExit::new((self.index * 64 + bit) as u16));
        }
        None
    }
}

/// One exit of a decision: an output port of one of its nodes that leads outside the group.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExitTarget {
    /// The node.
    pub node: NodeId,

    /// Its output port.
    pub port: PortName,
}

/// One predicate of a decision's predicate table.
#[derive(Clone, Debug)]
pub struct SharedPredicate {
    /// The predicate, in canonical form, with resolved values: what R2 compares.
    pub predicate: Predicate,

    /// The predicate as the `split` that it was first interned for writes it, with resolved values: what `compiled`
    /// evaluates, in that order. R7 changes the order of operands, which canonical form does not show, so a program's
    /// fingerprint covers this too.
    pub written: Predicate,

    /// The predicate, compiled.
    pub compiled: Arc<CompiledPredicate>,

    /// The stored fields it reads.
    pub reads: FieldSet,

    /// The predicate's value for every event, if R3 folded it: it is constant given its resolved values.
    pub folded: Option<bool>,

    /// The `split` nodes that use it, in the order they were added: more than one when R2 shares it.
    pub nodes: Vec<NodeId>,
}

/// The predicates of a decision's `split`s, each distinct one once.
#[derive(Clone, Debug, Default)]
pub struct PredicateTable {
    entries: Vec<SharedPredicate>,
}

impl PredicateTable {
    /// Creates an empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the predicate of the `split` node `node`, returning its index, or `None` if the table is full.
    ///
    /// `written` is the predicate as the node writes it, with resolved values, and `compiled` what it compiles to.
    /// With R2, a predicate equal in canonical form to one the table holds returns that one's index, with `node` added
    /// to its nodes, and `compiled` is not kept; with R3, a new entry records what [`fold_bound_constants`] folds it
    /// to.
    pub fn intern(
        &mut self, node: &NodeId, written: &Predicate, compiled: Arc<CompiledPredicate>,
    ) -> Option<PredicateIndex> {
        let predicate = written.canonical();
        if let Some(position) = self.entries.iter().position(|entry| entry.predicate == predicate) {
            let entry = &mut self.entries[position];
            if !entry.nodes.contains(node) {
                entry.nodes.push(node.clone());
            }
            // The table numbers at most `u16::MAX + 1` entries, so a found position fits.
            return Some(PredicateIndex::new(position as u16));
        }
        let folded = fold_bound_constants(&predicate);
        self.push(node, predicate, written, compiled, folded)
    }

    /// Adds the predicate of the `split` node `node` as an entry of its own, never shared and never folded, returning
    /// its index, or `None` if the table is full: a decision without R2 and R3, for comparing against.
    #[cfg(test)]
    pub(crate) fn append(
        &mut self, node: &NodeId, written: &Predicate, compiled: Arc<CompiledPredicate>,
    ) -> Option<PredicateIndex> {
        self.push(node, written.canonical(), written, compiled, None)
    }

    fn push(
        &mut self, node: &NodeId, predicate: Predicate, written: &Predicate, compiled: Arc<CompiledPredicate>,
        folded: Option<bool>,
    ) -> Option<PredicateIndex> {
        let index = PredicateIndex::new(u16::try_from(self.entries.len()).ok()?);
        self.entries.push(SharedPredicate {
            reads: predicate.reads(),
            predicate,
            written: written.clone(),
            compiled,
            folded,
            nodes: vec![node.clone()],
        });
        Some(index)
    }

    /// Returns one predicate.
    pub fn get(&self, index: PredicateIndex) -> Option<&SharedPredicate> {
        self.entries.get(index.get())
    }

    /// Returns every predicate, in index order.
    pub fn entries(&self) -> &[SharedPredicate] {
        &self.entries
    }

    /// Returns the number of predicates.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if the table has no predicate: the decision has only `tee`s.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the stored fields any of the predicates reads.
    pub fn reads(&self) -> FieldSet {
        self.entries
            .iter()
            .fold(FieldSet::EMPTY, |reads, entry| reads.union(entry.reads))
    }
}

/// Returns the value a predicate with resolved values has for every event, if it is constant: R3's fold inside a
/// compiled decision.
///
/// A predicate is constant when its `const`, `flag`, and `is_empty` leaves decide it, given their resolved values,
/// whatever the event: for example `all(flag(enabled), ...)` with `enabled` false, or `any(is_empty(list), ...)` with
/// an empty list. `all` and `any` follow three-valued logic, so `all` is constant `false` as soon as one operand is,
/// and an empty `all` is `true`. A `flag` or `is_empty` whose binding has no resolved value decides nothing. Folding
/// happens only in compiled programs, never in a plan's structure, so a binding's new value changes a generation's
/// program and nothing else.
pub fn fold_bound_constants(predicate: &Predicate) -> Option<bool> {
    match predicate {
        Predicate::Const(value) => Some(*value),
        Predicate::Flag(flag) => flag.value().copied(),
        Predicate::IsEmpty(list) => list.value().map(Vec::is_empty),
        Predicate::All(operands) => {
            let mut decided = Some(true);
            for operand in operands {
                match fold_bound_constants(operand) {
                    Some(false) => return Some(false),
                    Some(true) => {}
                    None => decided = None,
                }
            }
            decided
        }
        Predicate::Any(operands) => {
            let mut decided = Some(false);
            for operand in operands {
                match fold_bound_constants(operand) {
                    Some(true) => return Some(true),
                    Some(false) => {}
                    None => decided = None,
                }
            }
            decided
        }
        Predicate::Not(operand) => fold_bound_constants(operand).map(|value| !value),
        Predicate::Name(_)
        | Predicate::IntakeName(_)
        | Predicate::Host(_)
        | Predicate::HostUnset {}
        | Predicate::HasTag(_)
        | Predicate::Tag(_)
        | Predicate::HasOriginTag(_)
        | Predicate::OriginTag(_)
        | Predicate::KindIn(_) => None,
    }
}

/// Adds the bindings of a predicate's `flag` and `is_empty` leaves to `bindings`, once each, in predicate order.
fn constant_bindings<'p>(predicate: &'p Predicate, bindings: &mut Vec<&'p BindingName>) {
    let binding = match predicate {
        Predicate::Flag(flag) => flag.binding(),
        Predicate::IsEmpty(list) => list.binding(),
        Predicate::All(operands) | Predicate::Any(operands) => {
            for operand in operands {
                constant_bindings(operand, bindings);
            }
            None
        }
        Predicate::Not(operand) => {
            constant_bindings(operand, bindings);
            None
        }
        _ => None,
    };
    if let Some(binding) = binding {
        if !bindings.contains(&binding) {
            bindings.push(binding);
        }
    }
}

/// What one node of a decision does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecisionKind {
    /// A `split`: an event leaves by its `true` port if the predicate matches it, and by its `false` port otherwise.
    /// Events other than metrics leave by `false`.
    Split {
        /// The predicate, in the decision's predicate table.
        predicate: PredicateIndex,
    },

    /// A `tee`: an event leaves by every port.
    Tee,
}

/// Where one output port of a decision's node leads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Branch {
    /// To another node of the decision, by its position in [`Decision::nodes`].
    Node(u16),

    /// Out of the decision, by one of its exits.
    Exit(DecisionExit),
}

/// One `split` or `tee` node of a decision.
#[derive(Clone, Debug)]
pub struct DecisionNode {
    /// The node.
    pub node: NodeId,

    /// Its output ports, in port order, for telemetry that counts events per port.
    pub ports: Vec<PortName>,

    /// What it does.
    pub kind: DecisionKind,

    /// Where each of its output ports leads, in port order.
    pub branches: Vec<Branch>,
}

impl DecisionNode {
    /// Returns the node's operator: `split` or `tee`.
    pub fn operator(&self) -> &'static str {
        match self.kind {
            DecisionKind::Split { .. } => crate::operators::names::SPLIT,
            DecisionKind::Tee => crate::operators::names::TEE,
        }
    }
}

/// One decision: a maximal connected group of `split` and `tee` nodes of one slot, compiled to one function from an
/// event to its exit set.
#[derive(Clone, Debug)]
pub struct Decision {
    /// The group's nodes, the one the decision's input enters first, and every other node after the node leading to
    /// it.
    pub nodes: Vec<DecisionNode>,

    /// The decision's exits, in exit order: the order of the targets of the step that runs it.
    pub exits: Vec<ExitTarget>,

    /// The predicates of the group's `split`s.
    pub predicates: PredicateTable,

    /// Whether regions memoize the decision's function per context identity plus kind (R4).
    ///
    /// Decision compilation sets it only for a [memoizable][Self::memoizable] decision.
    pub memoized: bool,

    /// The decision-time rewrites (R2, R3's folding of bound constants, R4) applied to it, as `explain` lists them.
    pub rewrites: Vec<RewriteRecord>,
}

impl Decision {
    /// Returns the decision of one `split` or `tee` node alone, whose exits are its output ports in port order.
    ///
    /// `predicates` holds the predicate of a `split`, which `kind` refers to. The decision is not memoized.
    pub fn single(node: NodeId, ports: Vec<PortName>, kind: DecisionKind, predicates: PredicateTable) -> Self {
        let exits = ports
            .iter()
            .map(|port| ExitTarget {
                node: node.clone(),
                port: port.clone(),
            })
            .collect();
        let branches = (0..ports.len())
            .map(|port| Branch::Exit(DecisionExit::new(port as u16)))
            .collect();
        Self {
            nodes: vec![DecisionNode {
                node,
                ports,
                kind,
                branches,
            }],
            exits,
            predicates,
            memoized: false,
            rewrites: Vec::new(),
        }
    }

    /// Returns the node the decision's input enters.
    ///
    /// Decision compilation never builds a decision without nodes.
    pub fn root(&self) -> &DecisionNode {
        &self.nodes[0]
    }

    /// Returns the number of output ports of the decision's nodes, together: the number of its per-port counts.
    pub fn port_count(&self) -> usize {
        self.nodes.iter().map(|node| node.ports.len()).sum()
    }

    /// Returns `true` if a region may memoize the decision's function per context identity plus kind (R4): every
    /// predicate reads only the fields that make up a record's context, and its kind.
    pub fn memoizable(&self) -> bool {
        self.predicates.reads().is_subset(FieldSet::CONTEXT.with(Field::Kind))
    }

    /// Returns the estimated cost, in nanoseconds per event, of evaluating the decision: the cost `split_cost` gives
    /// the `split` node of each predicate R3 did not fold, each shared predicate counted once, as it is evaluated once.
    pub fn evaluation_cost<F>(&self, split_cost: F) -> u32
    where
        F: Fn(&NodeId) -> u32,
    {
        self.predicates
            .entries()
            .iter()
            .filter(|entry| entry.folded.is_none())
            .filter_map(|entry| entry.nodes.first())
            .map(split_cost)
            .fold(0, u32::saturating_add)
    }

    /// Returns the exit set of an event: the reference semantics of the decision's function.
    ///
    /// It walks the group from its first node: a `tee` continues on every port, a `split` on its `true` port if its
    /// predicate (or what R3 folded it to) matches the event and on its `false` port otherwise, and a port leading out
    /// of the group adds its exit. An event other than a metric takes every `split`'s `false` port. Each predicate is
    /// evaluated at most once per event (R2). `scratch` is reused across events, so that the walk never allocates once
    /// it has grown to the decision's size.
    pub fn evaluate(&self, event: &Event, scratch: &mut DecisionScratch) -> ExitSet {
        let mut view = event.try_as_metric().map(EventView::new);
        scratch.values.clear();
        scratch.values.resize(self.predicates.len(), None);
        scratch.pending.clear();
        scratch.pending.push(0);

        let mut set = ExitSet::EMPTY;
        while let Some(position) = scratch.pending.pop() {
            let Some(node) = self.nodes.get(usize::from(position)) else {
                continue;
            };
            let mut follow = |branch: &Branch, pending: &mut Vec<u16>| match *branch {
                Branch::Node(next) => pending.push(next),
                Branch::Exit(exit) => set.insert(exit),
            };
            match node.kind {
                DecisionKind::Tee => {
                    for branch in node.branches.iter().rev() {
                        follow(branch, &mut scratch.pending);
                    }
                }
                DecisionKind::Split { predicate } => {
                    let matched = match &mut view {
                        None => false,
                        Some(view) => match scratch.values.get(predicate.get()).copied().flatten() {
                            Some(value) => value,
                            None => {
                                let value = self
                                    .predicates
                                    .get(predicate)
                                    .is_some_and(|entry| entry.folded.unwrap_or_else(|| entry.compiled.evaluate(view)));
                                if let Some(slot) = scratch.values.get_mut(predicate.get()) {
                                    *slot = Some(value);
                                }
                                value
                            }
                        },
                    };
                    if let Some(branch) = node.branches.get(if matched { 0 } else { 1 }) {
                        follow(branch, &mut scratch.pending);
                    }
                }
            }
        }
        set
    }

    /// Returns, for every output port of the decision's nodes, the set of exits below it, laid out as
    /// [`port_counts`][Self::port_counts] lays out counts.
    pub fn port_exits(&self) -> Vec<ExitSet> {
        let offsets = self.port_offsets();
        let mut below = vec![ExitSet::EMPTY; self.nodes.len()];
        let mut ports = vec![ExitSet::EMPTY; self.port_count()];
        // Every node comes after the node leading to it, so walking backwards sees a node's successors first.
        for (position, node) in self.nodes.iter().enumerate().rev() {
            for (port, branch) in node.branches.iter().enumerate() {
                let exits = match *branch {
                    Branch::Node(next) => below.get(usize::from(next)).copied().unwrap_or_default(),
                    Branch::Exit(exit) => ExitSet::single(exit),
                };
                ports[offsets[position] + port] = exits;
                below[position] = below[position].union(exits);
            }
        }
        ports
    }

    /// Adds the events each output port of the decision's nodes received, derived from counts per exit set, to
    /// `ports`: node by node in [`nodes`][Self::nodes] order, port by port in port order.
    ///
    /// Exact, because a decision has no `merge`: an event left a port if and only if its exit set meets the exits
    /// below that port. Runs off the data path, when a region publishes. [`DecisionFunction::port_counts`] does the
    /// same without allocating.
    pub fn port_counts(&self, counts: &ExitSetCounts, ports: &mut [u64]) {
        add_port_counts(&self.port_exits(), counts, ports);
    }

    /// Returns the index of each node's first port in the layout of [`port_counts`][Self::port_counts].
    fn port_offsets(&self) -> Vec<usize> {
        let mut offset = 0;
        self.nodes
            .iter()
            .map(|node| {
                let first = offset;
                offset += node.ports.len();
                first
            })
            .collect()
    }

    /// Returns the records of the decision-time rewrites applied to the decision: R2 for each predicate several
    /// `split`s share, R3 for each predicate folded to a constant, and R4 if it is memoized.
    ///
    /// `position` orders nodes as the plan does, so that each record lists its nodes in plan order, and `cost` is the
    /// decision's [evaluation cost][Self::evaluation_cost], which R4's record states.
    pub(crate) fn rewrite_records<F>(&self, position: F, cost: u32) -> Vec<RewriteRecord>
    where
        F: Fn(&NodeId) -> usize,
    {
        let in_plan_order = |nodes: &[NodeId]| {
            let mut nodes = nodes.to_vec();
            nodes.sort_by_key(|node| position(node));
            nodes
        };
        let quoted = |nodes: &[NodeId]| listed(nodes.iter().map(|node| format!("`{}`", node)).collect());
        let fields = |reads: FieldSet| {
            if reads == FieldSet::EMPTY {
                "no field".to_string()
            } else {
                listed(reads.iter().map(|field| format!("`{}`", field.as_str())).collect())
            }
        };
        let record = |id, nodes, justification| RewriteRecord {
            id,
            outcome: RewriteOutcome::Applied,
            nodes,
            justification,
            telemetry: Vec::new(),
            capacity_rule: false,
        };

        let mut records = Vec::new();
        for entry in self.predicates.entries() {
            if entry.nodes.len() > 1 {
                let nodes = in_plan_order(&entry.nodes);
                let justification = format!(
                    "The predicates of {} are equal in canonical form, with resolved values. They run in one \
                     decision, whose `split` and `tee` nodes write no field, so nothing writes what they read ({}) \
                     between them: each event evaluates the predicate once.",
                    quoted(&nodes),
                    fields(entry.reads)
                );
                records.push(record(RewriteId::R2, nodes, justification));
            }
        }
        for entry in self.predicates.entries() {
            if let Some(value) = entry.folded {
                let mut bindings = Vec::new();
                constant_bindings(&entry.predicate, &mut bindings);
                let given = if bindings.is_empty() {
                    "its literals".to_string()
                } else {
                    format!(
                        "the resolved values of {}",
                        listed(bindings.iter().map(|binding| format!("`{}`", binding)).collect())
                    )
                };
                let (taken, idle) = if value { ("true", "false") } else { ("false", "true") };
                let justification = format!(
                    "The predicate is constant given {}: it is {} for every event, so the compiled decision sends \
                     every metric to the `{}` port without evaluating it. The plan keeps the node, and its `{}` port's \
                     exit stays, idle; a new value of a binding changes only the next generation's program.",
                    given, value, taken, idle
                );
                records.push(record(RewriteId::R3, in_plan_order(&entry.nodes), justification));
            }
        }
        if self.memoized {
            let nodes = self.nodes.iter().map(|node| node.node.clone()).collect::<Vec<_>>();
            let justification = format!(
                "The decision's predicates read only {}, which are context fields or the kind, and evaluating them is \
                 estimated to cost {} ns per event, at least the {} ns from which a lookup that hits saves several \
                 times what one that misses adds. Regions memoize the decision's exit set per context identity plus \
                 kind, in a direct-mapped memo of {} entries that belongs to one generation's program and evicts \
                 without allocating. Keying by the 64-bit context identity treats colliding contexts as one, as native \
                 aggregation does.",
                fields(self.predicates.reads()),
                cost,
                MEMO_COST_FLOOR,
                DECISION_MEMO_ENTRIES
            );
            records.push(record(RewriteId::R4, in_plan_order(&nodes), justification));
        }
        records
    }
}

/// Returns items as a sentence lists them: `a`, `a and b`, or `a, b, and c`.
fn listed(mut items: Vec<String>) -> String {
    match items.len() {
        0 => String::new(),
        1 => items.remove(0),
        2 => format!("{} and {}", items[0], items[1]),
        _ => {
            let last = items.pop().unwrap_or_default();
            format!("{}, and {}", items.join(", "), last)
        }
    }
}

/// Adds the events of each exit set to the ports below which it has an exit.
fn add_port_counts(port_exits: &[ExitSet], counts: &ExitSetCounts, ports: &mut [u64]) {
    for (set, events, _) in counts.iter() {
        for (count, below) in ports.iter_mut().zip(port_exits) {
            if set.intersects(below) {
                *count += events;
            }
        }
    }
}

/// Storage [`Decision::evaluate`] reuses across events.
#[derive(Clone, Debug, Default)]
pub struct DecisionScratch {
    /// Each predicate's value for the current event, once evaluated.
    values: Vec<Option<bool>>,
    /// The nodes still to visit.
    pending: Vec<u16>,
}

/// A decision that cannot be compiled to a function: its nodes do not form a tree whose branches reach every exit
/// once, or it refers to a predicate its table lacks.
///
/// Decision compilation builds only well-formed decisions, so this describes a program built some other way.
#[derive(Clone, Debug, Eq, PartialEq, Snafu)]
#[snafu(display("decision `{}` is malformed: {}", root, problem))]
pub struct MalformedDecision {
    /// The decision's first node, or `<none>` for a decision without nodes.
    pub root: String,

    /// What is wrong.
    pub problem: String,
}

/// One instruction of a decision function's code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Instruction {
    /// Adds exits to the event's exit set.
    Add(ExitSet),

    /// Evaluates a predicate, remembering its value in a cache slot if other instructions test it too (R2). When it
    /// is false, skips `skip` instructions: the code of the `split`'s `true` branch.
    Test {
        predicate: u16,
        cache: Option<u8>,
        skip: u16,
    },

    /// Skips `skip` instructions: the code of a `split`'s `false` branch, after its `true` branch ran.
    Jump(u16),
}

/// The form of a decision function.
#[derive(Clone, Debug)]
enum Form {
    /// Every metric takes the same exits: the decision has only `tee`s and folded `split`s.
    Constant(ExitSet),

    /// One live predicate decides: `base` and either `matched` or `unmatched`.
    Single {
        base: ExitSet,
        predicate: u16,
        matched: ExitSet,
        unmatched: ExitSet,
    },

    /// Independent live predicates, each deciding between two sets of exits, such as a `tee` to `split`s whose ports
    /// all leave the decision: `base`, and for each test either its `matched` or its `unmatched` exits.
    Tests { base: ExitSet, tests: Box<[LeafTest]> },

    /// Straight-line code with forward jumps, run once per metric from `base`.
    Code { base: ExitSet, code: Box<[Instruction]> },
}

/// One test of [`Form::Tests`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LeafTest {
    predicate: u16,
    cache: Option<u8>,
    matched: ExitSet,
    unmatched: ExitSet,
}

impl LeafTest {
    /// Returns the test four instructions make, if they are one: a test whose branches each add exits and end.
    fn of(code: &[Instruction]) -> Option<Self> {
        let [test, Instruction::Add(matched), Instruction::Jump(1), Instruction::Add(unmatched)] = *code else {
            return None;
        };
        let Instruction::Test {
            predicate,
            cache,
            skip: 2,
        } = test
        else {
            return None;
        };
        Some(Self {
            predicate,
            cache,
            matched,
            unmatched,
        })
    }
}

/// The values of the predicates several instructions test, remembered for one event (R2).
#[derive(Default)]
struct Remembered {
    known: [u64; 4],
    values: [u64; 4],
}

impl Remembered {
    /// Returns the value remembered in `slot`, or `evaluate`'s, which it remembers.
    fn get_or(&mut self, slot: u8, evaluate: impl FnOnce() -> bool) -> bool {
        let (word, bit) = (usize::from(slot) / 64, 1u64 << (slot % 64));
        if self.known[word] & bit != 0 {
            return self.values[word] & bit != 0;
        }
        let value = evaluate();
        self.known[word] |= bit;
        if value {
            self.values[word] |= bit;
        }
        value
    }
}

/// The code of one subtree of a decision: exits every event reaching it takes, then instructions for the rest.
#[derive(Default)]
struct Block {
    base: ExitSet,
    code: Vec<Instruction>,
}

impl Block {
    fn exit(exit: DecisionExit) -> Self {
        Self {
            base: ExitSet::single(exit),
            code: Vec::new(),
        }
    }

    /// Returns the block's instructions with its base as an instruction of its own: what a branch of a `split` runs.
    fn into_branch(self) -> Vec<Instruction> {
        let mut code = Vec::with_capacity(self.code.len() + 1);
        if !self.base.is_empty() {
            code.push(Instruction::Add(self.base));
        }
        code.extend(self.code);
        code
    }
}

/// A decision compiled to the function regions run: from an event to its exit set.
///
/// It computes exactly what [`Decision::evaluate`] computes, in one of four forms: a constant, for a decision whose
/// every metric takes the same exits; a single test, for a decision with one live predicate; a list of independent
/// tests, for a fan-out to `split`s whose ports all leave the decision, the shape endpoint routing lowers to; or
/// straight-line code with forward jumps, in which an event runs each instruction at most once. Every exit set that
/// does not depend on the event is precomputed. A predicate that R2 shares between `split`s whose branches can both
/// run for one event is evaluated once and its value remembered for the event; R3's folded predicates are never
/// evaluated: their `split`s take their constant branch at compilation. Evaluating never allocates.
#[derive(Clone)]
pub struct DecisionFunction {
    form: Form,
    /// The exit set of an event other than a metric: every `split`'s `false` port.
    otherwise: ExitSet,
    predicates: Box<[Arc<CompiledPredicate>]>,
    /// The exits below each port of the decision's nodes, as [`Decision::port_exits`] lays them out.
    port_exits: Box<[ExitSet]>,
    exits: usize,
}

impl fmt::Debug for DecisionFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecisionFunction")
            .field("form", &self.form)
            .field("otherwise", &self.otherwise)
            .field("exits", &self.exits)
            .finish_non_exhaustive()
    }
}

impl DecisionFunction {
    /// Compiles a decision to its function.
    ///
    /// # Errors
    ///
    /// Returns an error if the decision is malformed: it has no nodes or more exits than [`EXIT_SET_CAPACITY`], a node
    /// has a branch for other than each of its ports, a `split` has other than two ports or a predicate its table
    /// lacks, or its nodes do not form a tree from the first node in which every other node follows the one node
    /// leading to it and every exit is reached by exactly one branch.
    pub fn new(decision: &Decision) -> Result<Self, MalformedDecision> {
        let root = decision
            .nodes
            .first()
            .map_or_else(|| "<none>".to_string(), |node| node.node.to_string());
        let malformed = |problem: String| MalformedDecision {
            root: root.clone(),
            problem,
        };
        validate(decision).map_err(malformed)?;

        let count = decision.nodes.len();
        let mut blocks = (0..count).map(|_| None).collect::<Vec<Option<Block>>>();
        let mut otherwise = vec![ExitSet::EMPTY; count];
        let mut below = vec![ExitSet::EMPTY; count];
        let mut port_exits = vec![ExitSet::EMPTY; decision.port_count()];
        let mut offset = port_exits.len();
        // Every node follows the node leading to it, so walking backwards compiles a node's subtrees before the node.
        for (position, node) in decision.nodes.iter().enumerate().rev() {
            offset -= node.ports.len();
            let mut branches = Vec::with_capacity(node.branches.len());
            for (port, branch) in node.branches.iter().enumerate() {
                let (block, unmatched, exits) = match *branch {
                    Branch::Exit(exit) => (Block::exit(exit), ExitSet::single(exit), ExitSet::single(exit)),
                    Branch::Node(next) => {
                        let next = usize::from(next);
                        let block = blocks[next].take().unwrap_or_default();
                        (block, otherwise[next], below[next])
                    }
                };
                port_exits[offset + port] = exits;
                below[position] = below[position].union(exits);
                branches.push((block, unmatched));
            }
            let block = match node.kind {
                DecisionKind::Tee => {
                    let mut block = Block::default();
                    for (branch, unmatched) in branches {
                        block.base = block.base.union(branch.base);
                        block.code.extend(branch.code);
                        otherwise[position] = otherwise[position].union(unmatched);
                    }
                    block
                }
                DecisionKind::Split { predicate } => {
                    let mut branches = branches.into_iter();
                    let (Some((matched, _)), Some((unmatched, if_not_metric))) = (branches.next(), branches.next())
                    else {
                        return Err(malformed(format!("split `{}` must have two ports", node.node)));
                    };
                    otherwise[position] = if_not_metric;
                    match decision.predicates.get(predicate).and_then(|entry| entry.folded) {
                        Some(true) => matched,
                        Some(false) => unmatched,
                        None => {
                            let matched = matched.into_branch();
                            let unmatched = unmatched.into_branch();
                            let skip = |length: usize| {
                                u16::try_from(length).map_err(|_| malformed("its code is too long".to_string()))
                            };
                            let mut code = Vec::with_capacity(matched.len() + unmatched.len() + 2);
                            code.push(Instruction::Test {
                                // Validated: every predicate index is one of the table's.
                                predicate: predicate.get() as u16,
                                cache: None,
                                skip: skip(matched.len() + 1)?,
                            });
                            code.extend(matched);
                            code.push(Instruction::Jump(skip(unmatched.len())?));
                            code.extend(unmatched);
                            Block {
                                base: ExitSet::EMPTY,
                                code,
                            }
                        }
                    }
                }
            };
            blocks[position] = Some(block);
        }
        let Block { base, mut code } = blocks[0].take().unwrap_or_default();

        // R2: a predicate several instructions test is remembered for the event in a cache slot of its own.
        let mut tests = vec![0usize; decision.predicates.len()];
        for instruction in &code {
            if let Instruction::Test { predicate, .. } = instruction {
                tests[usize::from(*predicate)] += 1;
            }
        }
        let mut slots = vec![None; decision.predicates.len()];
        let mut next = 0usize;
        for (slot, count) in slots.iter_mut().zip(&tests) {
            if *count > 1 {
                // A tree has fewer `split`s than exits, so at most `EXIT_SET_CAPACITY - 1` predicates are tested.
                *slot = Some(u8::try_from(next).map_err(|_| malformed("it shares too many predicates".to_string()))?);
                next += 1;
            }
        }
        for instruction in &mut code {
            if let Instruction::Test { predicate, cache, .. } = instruction {
                *cache = slots[usize::from(*predicate)];
            }
        }

        // Code made only of leaf tests, each choosing between two sets of exits, is a list of tests, or one.
        let tests = (!code.is_empty() && code.len() % 4 == 0)
            .then(|| code.chunks_exact(4).map(LeafTest::of).collect::<Option<Box<[_]>>>())
            .flatten();
        let form = match tests {
            _ if code.is_empty() => Form::Constant(base),
            Some(tests) => match *tests {
                [test] => Form::Single {
                    base,
                    predicate: test.predicate,
                    matched: test.matched,
                    unmatched: test.unmatched,
                },
                _ => Form::Tests { base, tests },
            },
            None => Form::Code {
                base,
                code: code.into_boxed_slice(),
            },
        };
        Ok(Self {
            form,
            otherwise: otherwise[0],
            predicates: decision
                .predicates
                .entries()
                .iter()
                .map(|entry| Arc::clone(&entry.compiled))
                .collect(),
            port_exits: port_exits.into_boxed_slice(),
            exits: decision.exits.len(),
        })
    }

    /// Returns the number of the decision's exits.
    pub fn exit_count(&self) -> usize {
        self.exits
    }

    /// Returns the exit set every event takes, if the function does not depend on the event: the decision has only
    /// `tee`s, or only `split`s R3 folded to send every event, metric or not, the same way.
    pub fn constant(&self) -> Option<ExitSet> {
        match self.form {
            Form::Constant(set) if set == self.otherwise => Some(set),
            _ => None,
        }
    }

    /// Returns the exit set of an event.
    pub fn evaluate(&self, event: &Event) -> ExitSet {
        match event.try_as_metric() {
            Some(metric) => self.evaluate_metric(metric),
            None => self.otherwise,
        }
    }

    /// Returns the exit set of an event, from `memo` if it holds the event's context and kind, and otherwise
    /// evaluated, and memoized (R4).
    ///
    /// Only a decision whose every predicate reads context fields and the kind alone may be memoized
    /// ([`Decision::memoizable`]), and a memo must serve one decision of one generation's program. A hit costs one
    /// lookup. Events other than metrics, and decisions whose exit set is the same for every metric, bypass the memo.
    pub fn evaluate_memoized(&self, event: &Event, memo: &mut DecisionMemo) -> ExitSet {
        let Some(metric) = event.try_as_metric() else {
            return self.otherwise;
        };
        if let Form::Constant(set) = self.form {
            return set;
        }
        memo.get_or_insert_with(MemoKey::of(metric), || self.evaluate_metric(metric))
    }

    fn evaluate_metric(&self, metric: &Metric) -> ExitSet {
        match &self.form {
            Form::Constant(set) => *set,
            Form::Single {
                base,
                predicate,
                matched,
                unmatched,
            } => {
                let mut view = EventView::new(metric);
                if self.test(*predicate, &mut view) {
                    base.union(*matched)
                } else {
                    base.union(*unmatched)
                }
            }
            Form::Tests { base, tests } => {
                let mut view = EventView::new(metric);
                let mut remembered = Remembered::default();
                tests.iter().fold(*base, |set, test| {
                    let value = match test.cache {
                        None => self.test(test.predicate, &mut view),
                        Some(slot) => remembered.get_or(slot, || self.test(test.predicate, &mut view)),
                    };
                    set.union(if value { test.matched } else { test.unmatched })
                })
            }
            Form::Code { base, code } => {
                let mut view = EventView::new(metric);
                let mut remembered = Remembered::default();
                let mut set = *base;
                let mut pc = 0;
                while let Some(instruction) = code.get(pc) {
                    pc += 1;
                    match *instruction {
                        Instruction::Add(exits) => set = set.union(exits),
                        Instruction::Jump(skip) => pc += usize::from(skip),
                        Instruction::Test { predicate, cache, skip } => {
                            let value = match cache {
                                None => self.test(predicate, &mut view),
                                Some(slot) => remembered.get_or(slot, || self.test(predicate, &mut view)),
                            };
                            if !value {
                                pc += usize::from(skip);
                            }
                        }
                    }
                }
                set
            }
        }
    }

    fn test(&self, predicate: u16, view: &mut EventView<'_>) -> bool {
        self.predicates
            .get(usize::from(predicate))
            .is_some_and(|compiled| compiled.evaluate(view))
    }

    /// Adds the events each output port of the decision's nodes received, derived from counts per exit set, to
    /// `ports`, laid out as [`Decision::port_counts`] lays them out. Never allocates.
    pub fn port_counts(&self, counts: &ExitSetCounts, ports: &mut [u64]) {
        add_port_counts(&self.port_exits, counts, ports);
    }

    /// Returns the number of per-port counts: the output ports of the decision's nodes, together.
    pub fn port_count(&self) -> usize {
        self.port_exits.len()
    }

    /// Returns the number of predicates the function remembers per event because several instructions test them.
    #[cfg(test)]
    pub(crate) fn cache_slots(&self) -> usize {
        let mut slots = match &self.form {
            Form::Code { code, .. } => code
                .iter()
                .filter_map(|instruction| match instruction {
                    Instruction::Test { cache, .. } => *cache,
                    _ => None,
                })
                .collect::<Vec<_>>(),
            Form::Tests { tests, .. } => tests.iter().filter_map(|test| test.cache).collect(),
            Form::Constant(_) | Form::Single { .. } => Vec::new(),
        };
        slots.sort_unstable();
        slots.dedup();
        slots.len()
    }
}

/// Checks that a decision's nodes form a tree from its first node, as [`DecisionFunction::new`] documents.
fn validate(decision: &Decision) -> Result<(), String> {
    if decision.nodes.is_empty() {
        return Err("it has no nodes".to_string());
    }
    if decision.exits.len() > EXIT_SET_CAPACITY {
        return Err(format!(
            "it has {} exits, more than an exit set holds ({})",
            decision.exits.len(),
            EXIT_SET_CAPACITY
        ));
    }
    if decision.nodes.len() > usize::from(u16::MAX) {
        return Err(format!(
            "it has {} nodes, more than a branch numbers",
            decision.nodes.len()
        ));
    }
    let mut entered = vec![false; decision.nodes.len()];
    let mut reached = vec![false; decision.exits.len()];
    for (position, node) in decision.nodes.iter().enumerate() {
        if node.branches.len() != node.ports.len() {
            return Err(format!(
                "node `{}` has {} ports but {} branches",
                node.node,
                node.ports.len(),
                node.branches.len()
            ));
        }
        match node.kind {
            DecisionKind::Split { predicate } => {
                if node.branches.len() != 2 {
                    return Err(format!("split `{}` must have two ports", node.node));
                }
                if decision.predicates.get(predicate).is_none() {
                    return Err(format!("split `{}` has no predicate {}", node.node, predicate));
                }
            }
            DecisionKind::Tee if node.branches.is_empty() => {
                return Err(format!("tee `{}` has no ports", node.node));
            }
            DecisionKind::Tee => {}
        }
        for branch in &node.branches {
            match *branch {
                Branch::Node(next) => {
                    let next = usize::from(next);
                    if next <= position || next >= decision.nodes.len() {
                        return Err(format!(
                            "node `{}` leads to node {}, which is not a later node of the decision",
                            node.node, next
                        ));
                    }
                    if std::mem::replace(&mut entered[next], true) {
                        return Err(format!("node {} is entered by more than one branch", next));
                    }
                }
                Branch::Exit(exit) => {
                    let Some(reached) = reached.get_mut(exit.get()) else {
                        return Err(format!("node `{}` leads to exit {}, which it lacks", node.node, exit));
                    };
                    if std::mem::replace(reached, true) {
                        return Err(format!("exit {} is reached by more than one branch", exit));
                    }
                }
            }
        }
    }
    if let Some(position) = entered.iter().skip(1).position(|entered| !entered) {
        return Err(format!("node {} is entered by no branch", position + 1));
    }
    if let Some(exit) = reached.iter().position(|reached| !reached) {
        return Err(format!("exit {} is reached by no branch", exit));
    }
    Ok(())
}

/// Events and their points counted per exit set, as a region counts a decision's events between publications of its
/// telemetry.
///
/// The counts live in a hash table allocated once, holding a fixed number of distinct exit sets, so that counting a
/// run of events costs one probe. Per-node, per-port counts are derived from them when the region publishes
/// ([`Decision::port_counts`], [`DecisionFunction::port_counts`]), and so is the policy loss of each exit that leads to
/// a `drop`, in events and points.
#[derive(Clone, Debug)]
pub struct ExitSetCounts {
    /// Each counted set, with its events and their points.
    slots: Box<[Option<(ExitSet, u64, u64)>]>,
    len: usize,
    capacity: usize,
}

impl ExitSetCounts {
    /// Creates counts for at most `capacity` distinct exit sets.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            slots: vec![None; Self::slots_for(capacity)].into_boxed_slice(),
            len: 0,
            capacity,
        }
    }

    /// Returns the bytes counts for at most `capacity` distinct exit sets hold: what
    /// [`with_capacity`][Self::with_capacity] allocates.
    pub fn footprint(capacity: usize) -> usize {
        Self::slots_for(capacity) * std::mem::size_of::<Option<(ExitSet, u64, u64)>>()
    }

    /// Returns the slots of a table for `capacity` sets: at least twice as many, a power of two.
    fn slots_for(capacity: usize) -> usize {
        capacity.saturating_mul(2).max(1).next_power_of_two()
    }

    /// Counts `events` events with the exit set `set`, which carry `points` points.
    ///
    /// The points are what policy loss counts; a caller that derives no loss from the counts may pass zero. Returns
    /// `false`, counting nothing, if `set` is new and the counts already hold as many distinct sets as they can: the
    /// caller then derives what it needs from the counts it has ([`DecisionFunction::port_counts`]),
    /// [clears][Self::clear] them, and counts again. Never allocates.
    #[must_use]
    pub fn add(&mut self, set: ExitSet, events: u64, points: u64) -> bool {
        let mask = self.slots.len() - 1;
        let mut slot = (set.mixed() >> 32) as usize & mask;
        // The table has at least twice as many slots as the counts hold sets, so a probe ends at a free slot.
        for _ in 0..self.slots.len() {
            match &mut self.slots[slot] {
                Some((known, counted, counted_points)) if *known == set => {
                    *counted += events;
                    *counted_points += points;
                    return true;
                }
                Some(_) => slot = (slot + 1) & mask,
                free @ None => {
                    if self.len == self.capacity {
                        return false;
                    }
                    *free = Some((set, events, points));
                    self.len += 1;
                    return true;
                }
            }
        }
        false
    }

    /// Returns each exit set counted, with its number of events and their points, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (ExitSet, u64, u64)> + '_ {
        self.slots.iter().flatten().copied()
    }

    /// Returns `true` if nothing is counted.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Clears the counts, keeping their storage.
    pub fn clear(&mut self) {
        if self.len > 0 {
            self.slots.fill(None);
            self.len = 0;
        }
    }
}

/// The key of a memoized decision result: a record's context identity and its kind.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MemoKey {
    /// The context's 64-bit identity, the hash that interning and native aggregation compare contexts by.
    pub context: u64,

    /// The record's kind.
    pub kind: Kind,
}

impl MemoKey {
    /// Returns the key of a metric.
    pub fn of(metric: &Metric) -> Self {
        Self {
            context: metric.context().key().as_u64(),
            kind: Kind::of(metric.values()),
        }
    }

    /// Returns the kind as a non-zero tag: zero marks an empty memo entry.
    fn tag(self) -> u64 {
        self.kind as u64 + 1
    }
}

/// The R4 memo of one decision in one generation's program: exit sets by context identity plus kind.
///
/// Direct-mapped with a fixed number of entries, allocated once: a lookup is one probe, and an insertion replaces
/// whatever its entry held, so eviction never allocates. A memo belongs to one generation's program in one region and
/// is dropped with it, so a new generation, whose predicates may differ, starts empty. Two contexts with equal
/// identities share an entry, as native aggregation treats them as one context.
///
/// Each entry holds the key and as many words of the exit set as the decision's exits need, so a decision with at most
/// 64 exits takes 24 bytes per entry.
#[derive(Clone, Debug)]
pub struct DecisionMemo {
    /// `2 + words` words per entry: the context identity, the kind's tag (zero for an empty entry), then the exit set's
    /// first `words` words.
    entries: Box<[u64]>,
    words: usize,
    capacity: usize,
    /// How far a mixed key shifts right to give an entry index: 64 less the log of the capacity.
    shift: u32,
}

impl DecisionMemo {
    /// Creates a memo with `capacity` entries, rounded down to a power of two, and at least one, for exit sets of any
    /// width.
    ///
    /// The limits table bounds it per decision ([`DECISION_MEMO_ENTRIES`]).
    pub fn new(capacity: usize) -> Self {
        Self::for_exits(capacity, EXIT_SET_CAPACITY)
    }

    /// Creates a memo with `capacity` entries, rounded down to a power of two, and at least one, for a decision with
    /// `exits` exits: it stores only the words of an exit set that such a decision uses.
    pub fn for_exits(capacity: usize, exits: usize) -> Self {
        let capacity = Self::entries_for(capacity);
        let words = Self::words_for(exits);
        Self {
            entries: vec![0; capacity * (2 + words)].into_boxed_slice(),
            words,
            capacity,
            shift: 64 - capacity.trailing_zeros(),
        }
    }

    /// Returns the bytes a memo of `capacity` entries for a decision with `exits` exits holds: what
    /// [`for_exits`][Self::for_exits] allocates.
    pub fn footprint(capacity: usize, exits: usize) -> usize {
        Self::entries_for(capacity) * (2 + Self::words_for(exits)) * std::mem::size_of::<u64>()
    }

    fn entries_for(capacity: usize) -> usize {
        if capacity <= 1 {
            1
        } else {
            1 << (usize::BITS - 1 - capacity.leading_zeros())
        }
    }

    fn words_for(exits: usize) -> usize {
        exits.div_ceil(64).clamp(1, EXIT_SET_WORDS)
    }

    /// Returns the number of entries.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the exit set memoized for `key`, if its entry holds it.
    pub fn get(&self, key: MemoKey) -> Option<ExitSet> {
        self.get_at(self.start(key), key)
    }

    /// Memoizes the exit set of `key`, replacing what its entry held.
    ///
    /// A set with an exit beyond those the memo was created for is not memoized: its entry is emptied, so that
    /// [`get`][Self::get] misses.
    pub fn insert(&mut self, key: MemoKey, set: ExitSet) {
        self.insert_at(self.start(key), key, set);
    }

    /// Returns the exit set memoized for `key` if its entry holds it, and otherwise memoizes and returns what
    /// `evaluate` returns, as [`insert`][Self::insert] does: one probe either way.
    pub fn get_or_insert_with<F>(&mut self, key: MemoKey, evaluate: F) -> ExitSet
    where
        F: FnOnce() -> ExitSet,
    {
        let start = self.start(key);
        if let Some(set) = self.get_at(start, key) {
            return set;
        }
        let set = evaluate();
        self.insert_at(start, key, set);
        set
    }

    fn get_at(&self, start: usize, key: MemoKey) -> Option<ExitSet> {
        let entry = &self.entries[start..start + 2 + self.words];
        if entry[0] != key.context || entry[1] != key.tag() {
            return None;
        }
        let mut set = ExitSet::EMPTY;
        set.0[..self.words].copy_from_slice(&entry[2..]);
        Some(set)
    }

    fn insert_at(&mut self, start: usize, key: MemoKey, set: ExitSet) {
        let words = self.words;
        let entry = &mut self.entries[start..start + 2 + words];
        if set.0[words..].iter().any(|word| *word != 0) {
            entry[1] = 0;
            return;
        }
        entry[0] = key.context;
        entry[1] = key.tag();
        entry[2..].copy_from_slice(&set.0[..words]);
    }

    /// Empties every entry, keeping their storage.
    pub fn clear(&mut self) {
        self.entries.fill(0);
    }

    /// Returns the index of the first word of `key`'s entry.
    fn start(&self, key: MemoKey) -> usize {
        self.slot(key) * (2 + self.words)
    }

    fn slot(&self, key: MemoKey) -> usize {
        if self.capacity == 1 {
            return 0;
        }
        // The kind moves equal contexts of different kinds apart; the multiplication spreads every bit of the key
        // into the high bits, which index the entry.
        let mixed = (key.context ^ key.tag().wrapping_mul(MIX)).wrapping_mul(MIX);
        (mixed >> self.shift) as usize
    }
}

#[cfg(test)]
mod tests;
