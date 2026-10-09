//! Duplicate contribution analysis. See [the module documentation](super#duplicate-analysis) for the rule.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::{
    graph::{emit_output, split_predicate, Endpoint, Graph},
    types::Types,
    Finding, Location, Rule, ValidationReport,
};
use crate::{
    canonical::to_canonical_string,
    ir::{OutputId, Plan},
    limits::values,
    model::FieldSet,
    operators::{Cardinality, PortRelation, TRUE_PORT},
};

/// A path from a source to an `emit`: the output port it leaves each node by, and the `emit` it ends at.
struct Path {
    hops: Vec<Endpoint>,
    end: usize,
}

/// The protected `split` crossings of a path after a divergence, as a set of (predicate, port) pairs.
///
/// Predicates are numbered by their canonical form, so equal predicates share a number, and the set is a bitset: bit
/// `2k` stands for crossing a `split` on predicate `k` by its `true` port, and bit `2k + 1` by its `false` port. Two
/// paths are separated when one crosses some predicate by the opposite port from the other, which is one bitwise
/// operation per word, however long the predicates are.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct Crossings(Vec<u64>);

impl Crossings {
    /// Bits for the `true` port of every predicate, in each word.
    const TRUE_BITS: u64 = 0x5555_5555_5555_5555;

    fn new(predicates: usize) -> Self {
        Self(vec![0; (2 * predicates).div_ceil(64)])
    }

    fn insert(&mut self, predicate: usize, takes_true: bool) {
        let bit = 2 * predicate + usize::from(!takes_true);
        self.0[bit / 64] |= 1 << (bit % 64);
    }

    /// Returns these crossings with every `split` taken by its other port.
    fn opposite(&self) -> Self {
        Self(
            self.0
                .iter()
                .map(|word| ((word & Self::TRUE_BITS) << 1) | ((word >> 1) & Self::TRUE_BITS))
                .collect(),
        )
    }

    fn intersects(&self, other: &Self) -> bool {
        self.0.iter().zip(&other.0).any(|(a, b)| a & b != 0)
    }

    fn intersection(&self, other: &Self) -> Self {
        Self(self.0.iter().zip(&other.0).map(|(a, b)| a & b).collect())
    }

    /// Returns `true` if a path with these crossings and a path with `other` cross some predicate by opposite ports.
    fn separates(&self, other: &Self) -> bool {
        self.opposite().intersects(other)
    }
}

/// Groups the plan's outputs so that outputs sharing a recipient are in one group.
///
/// Returns the group of each output, and the outputs of each group, in declaration order.
fn output_groups(plan: &Plan) -> (HashMap<&OutputId, usize>, Vec<Vec<&OutputId>>) {
    let mut outputs = Vec::new();
    let mut seen = BTreeSet::new();
    for output in &plan.outputs {
        if seen.insert(&output.id) {
            outputs.push(output);
        }
    }

    // Union-find over outputs, joining every pair that shares a recipient.
    let mut parent = (0..outputs.len()).collect::<Vec<_>>();
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    let mut owner = HashMap::new();
    for (i, output) in outputs.iter().enumerate() {
        for recipient in &output.recipients {
            match owner.get(recipient) {
                Some(&j) => {
                    let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                    parent[a.max(b)] = a.min(b);
                }
                None => {
                    owner.insert(recipient, i);
                }
            }
        }
    }

    let mut group_of = HashMap::new();
    let mut groups: Vec<Vec<&OutputId>> = Vec::new();
    let mut group_index = BTreeMap::new();
    for (i, output) in outputs.iter().enumerate() {
        let root = find(&mut parent, i);
        let next = group_index.len();
        let group = *group_index.entry(root).or_insert(next);
        if group == groups.len() {
            groups.push(Vec::new());
        }
        groups[group].push(&output.id);
        group_of.insert(&output.id, group);
    }
    (group_of, groups)
}

struct Analysis<'g, 'p> {
    graph: &'g Graph<'p>,
    types: &'g Types,
    /// For each `split` node, the number of its predicate's canonical form and the fields the predicate reads.
    splits: Vec<Option<(usize, FieldSet)>>,
    /// The number of distinct canonical predicates.
    predicates: usize,
    /// Each divergence already reported, as (group, node, first port, second port).
    reported: BTreeSet<(usize, usize, usize, usize)>,
    /// How many more pairs of crossing sets the analysis may compare.
    pair_budget: u64,
}

/// The analysis gave up because it reached its work limit.
struct WorkLimitReached;

impl Analysis<'_, '_> {
    /// Returns the fields a node may write to the records it receives.
    fn writes(&self, node: usize) -> FieldSet {
        self.graph.op(node).map_or(FieldSet::ALL, |op| {
            op.contract().writes.over(self.types.input_kinds(node))
        })
    }

    /// Returns the `split` crossings of a path after its divergence at `depth` that are protected: the predicate and
    /// whether the path takes the `true` port, for each `split` whose predicate reads no field written from the
    /// divergence up to the `split`.
    fn protected_crossings(&self, path: &Path, depth: usize) -> Crossings {
        let mut crossings = Crossings::new(self.predicates);
        let mut written = self.writes(path.hops[depth].0);
        for &(node, port) in &path.hops[depth + 1..] {
            if let Some((predicate, reads)) = self.splits[node] {
                if !reads.intersects(written) {
                    crossings.insert(predicate, self.graph.output_name((node, port)) == TRUE_PORT);
                }
            }
            written = written.union(self.writes(node));
        }
        crossings
    }

    /// Returns the distinct protected crossings of the given paths after their divergence at `depth`, each with the
    /// first path that has them.
    fn distinct_crossings<'a>(&self, paths: &[&'a Path], depth: usize) -> BTreeMap<Crossings, &'a Path> {
        let mut distinct = BTreeMap::new();
        for path in paths {
            distinct.entry(self.protected_crossings(path, depth)).or_insert(*path);
        }
        distinct
    }

    /// Returns a pair of paths, one from each side of a divergence, that no pair of complementary `split`s separates.
    ///
    /// Paths with equal crossings are compared once. When every path on one side crosses some predicate by the port
    /// opposite to every path on the other, all pairs are separated at once; otherwise pairs are compared one by one,
    /// each charged against the analysis's work budget.
    fn unseparated<'a>(
        &mut self, first: &[&'a Path], second: &[&'a Path], depth: usize,
    ) -> Result<Option<(&'a Path, &'a Path)>, WorkLimitReached> {
        let first = self.distinct_crossings(first, depth);
        let second = self.distinct_crossings(second, depth);
        let common = |sets: &BTreeMap<Crossings, &Path>| {
            sets.keys()
                .cloned()
                .reduce(|common, crossings| common.intersection(&crossings))
        };
        if let (Some(first_common), Some(second_common)) = (common(&first), common(&second)) {
            if first_common.separates(&second_common) {
                return Ok(None);
            }
        }

        for (first_crossings, first_path) in &first {
            let opposite = first_crossings.opposite();
            for (second_crossings, second_path) in &second {
                if self.pair_budget == 0 {
                    return Err(WorkLimitReached);
                }
                self.pair_budget -= 1;
                if !opposite.intersects(second_crossings) {
                    return Ok(Some((first_path, second_path)));
                }
            }
        }
        Ok(None)
    }

    fn describe(&self, path: &Path) -> String {
        let mut text = String::new();
        for &(node, port) in &path.hops {
            text.push_str(&format!(
                "{}.{} -> ",
                self.graph.id(node),
                self.graph.output_name((node, port))
            ));
        }
        text.push_str(&self.graph.id(path.end).to_string());
        text
    }

    /// Checks every pair of paths from one source, reporting each divergence at copies that no exception covers.
    fn check_pairs(
        &mut self, group: usize, outputs: &str, paths: Vec<&Path>, report: &mut ValidationReport,
    ) -> Result<(), WorkLimitReached> {
        let mut pending = vec![(paths, 0)];
        while let Some((paths, depth)) = pending.pop() {
            if paths.len() < 2 {
                continue;
            }
            // Every path shares its first `depth` hops with the others, so all are at the same node.
            let mut by_port: BTreeMap<usize, Vec<&Path>> = BTreeMap::new();
            for path in paths {
                if let Some(&(_, port)) = path.hops.get(depth) {
                    by_port.entry(port).or_default().push(path);
                }
            }
            let node = by_port.values().next().map(|paths| paths[0].hops[depth].0);
            if let (Some(node), true) = (node, by_port.len() > 1) {
                let copies = self
                    .graph
                    .op(node)
                    .is_none_or(|op| op.contract().ports == PortRelation::Copies);
                if copies {
                    let ports = by_port.iter().collect::<Vec<_>>();
                    for (i, (&first_port, first_paths)) in ports.iter().enumerate() {
                        for (&second_port, second_paths) in &ports[i + 1..] {
                            if self.reported.contains(&(group, node, first_port, second_port)) {
                                continue;
                            }
                            // The exception applies to a pair when one path crosses a protected `split` by its
                            // `true` port and the other crosses a protected `split` with an equal predicate by its
                            // `false` port.
                            if let Some((first, second)) = self.unseparated(first_paths, second_paths, depth)? {
                                self.reported.insert((group, node, first_port, second_port));
                                let op = self.graph.op(node).map_or("?", |op| op.name());
                                report.push(Finding::new(
                                    Rule::DuplicateContribution,
                                    self.graph.location(node),
                                    format!(
                                        "one contribution can reach {} twice: the paths `{}` and `{}` diverge here, at \
                                         `{}`, whose output ports `{}` and `{}` carry copies, and no pair of `split`s \
                                         with equal predicates sends the copies opposite ways afterwards; route each \
                                         record to the output once",
                                        outputs,
                                        self.describe(first),
                                        self.describe(second),
                                        op,
                                        self.graph.output_name((node, first_port)),
                                        self.graph.output_name((node, second_port)),
                                    ),
                                ));
                            }
                        }
                    }
                }
            }
            for (_, paths) in by_port {
                pending.push((paths, depth + 1));
            }
        }
        Ok(())
    }
}

/// Runs the duplicate analysis.
///
/// The analysis is bounded twice by the [work limit][crate::limits::WORK_PER_INPUT] `w`: it enumerates at most `w`
/// paths for each group of outputs, and compares at most `w * w` pairs of distinct crossing sets in the whole plan.
/// Beyond either bound it reports `duplicate_analysis_limit` instead of finishing.
pub(super) fn check(graph: &Graph<'_>, types: &Types, plan: &Plan, report: &mut ValidationReport) {
    let limit = values::WORK_PER_INPUT;
    check_within(graph, types, plan, limit.saturating_mul(limit), report);
}

/// Runs the duplicate analysis, comparing at most `pair_budget` pairs of crossing sets.
pub(super) fn check_within(
    graph: &Graph<'_>, types: &Types, plan: &Plan, pair_budget: u64, report: &mut ValidationReport,
) {
    // Paths through a cycle are unbounded; the cycle itself is already reported.
    if graph.cyclic {
        return;
    }
    let limit = usize::try_from(values::WORK_PER_INPUT).unwrap_or(usize::MAX);
    let (group_of, groups) = output_groups(plan);
    let group_of_emit = |node: usize| {
        graph
            .op(node)
            .and_then(emit_output)
            .and_then(|output| group_of.get(output).copied())
    };

    // The groups whose `emit`s each node can reach.
    let mut reaches = vec![BTreeSet::new(); graph.nodes.len()];
    for &node in graph.order.iter().rev() {
        let Some(op) = graph.op(node) else { continue };
        let mut reached = BTreeSet::new();
        if let Some(group) = group_of_emit(node) {
            reached.insert(group);
        }
        for port in 0..op.ports().outputs.len() {
            if let Some((next, _)) = graph.target(node, port) {
                reached.extend(reaches[next].iter().copied());
            }
        }
        reaches[node] = reached;
    }

    let sources = graph
        .order
        .iter()
        .copied()
        .filter(|&node| {
            graph
                .op(node)
                .is_some_and(|op| op.contract().cardinality == Cardinality::Source)
        })
        .collect::<Vec<_>>();

    // Number each distinct canonical predicate once, so that comparing two crossings never compares predicate text.
    let mut numbers: HashMap<String, usize> = HashMap::new();
    let splits = (0..graph.nodes.len())
        .map(|node| {
            let predicate = graph.op(node).and_then(split_predicate)?;
            let canonical = serde_json::to_value(predicate.canonical()).ok()?;
            let key = to_canonical_string(&canonical).ok()?;
            let next = numbers.len();
            Some((*numbers.entry(key).or_insert(next), predicate.reads()))
        })
        .collect();
    let mut analysis = Analysis {
        graph,
        types,
        splits,
        predicates: numbers.len(),
        reported: BTreeSet::new(),
        pair_budget,
    };

    for (group, outputs) in groups.iter().enumerate() {
        let outputs_text = match outputs.as_slice() {
            [output] => format!("output `{}`", output),
            outputs => format!(
                "outputs {}, which share a recipient,",
                outputs
                    .iter()
                    .map(|output| format!("`{}`", output))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };

        // Enumerate every path from each source to an `emit` of the group.
        let mut paths = Vec::new();
        let mut exceeded = false;
        'sources: for &source in &sources {
            let mut pending = vec![(source, Vec::new())];
            while let Some((node, hops)) = pending.pop() {
                let Some(op) = graph.op(node) else { continue };
                if group_of_emit(node) == Some(group) {
                    paths.push(Path { hops, end: node });
                    if paths.len() > limit {
                        exceeded = true;
                        break 'sources;
                    }
                    continue;
                }
                for port in (0..op.ports().outputs.len()).rev() {
                    if let Some((next, _)) = graph.target(node, port) {
                        if reaches[next].contains(&group) {
                            let mut next_hops = hops.clone();
                            next_hops.push((node, port));
                            pending.push((next, next_hops));
                        }
                    }
                }
            }
        }
        if exceeded {
            report.push(Finding::new(
                Rule::DuplicateAnalysisLimit,
                Location::Output(outputs[0].clone()),
                format!(
                    "more than {} distinct paths reach {}, so the duplicate analysis cannot check them; reduce the \
                     number of ways records can reach it",
                    limit, outputs_text
                ),
            ));
            continue;
        }

        // Paths from different sources carry different contributions, so only pairs from one source are compared.
        let mut by_source: BTreeMap<usize, Vec<&Path>> = BTreeMap::new();
        for path in &paths {
            if let Some(&(source, _)) = path.hops.first() {
                by_source.entry(source).or_default().push(path);
            }
        }
        for (_, paths) in by_source {
            if analysis.check_pairs(group, &outputs_text, paths, report).is_err() {
                report.push(Finding::new(
                    Rule::DuplicateAnalysisLimit,
                    Location::Output(outputs[0].clone()),
                    format!(
                        "the paths that reach {} cross so many different `split`s that the duplicate analysis cannot \
                         compare them within its work limit; route records through fewer distinct predicates",
                        outputs_text
                    ),
                ));
                return;
            }
        }
    }
}
