//! R5b: push a filter upstream across a stateful operator, as emission suppression inside the state owner.
//!
//! Moving a filter above a state owner changes which keys it admits under capacity, and how its idle state back-fills,
//! so R5b never moves the filter. For the kinds where the filter commutes with the state owner's emission, the state
//! owner skips building and emitting the records the filter would drop, judging them with the filter's predicate as
//! compiled in the generation each flush is stamped with; the filter stays where it is, for every other kind and every
//! other input. The plan is unchanged, so validation and placement are unaffected: like R2 and R4, R5b changes only
//! the compiled programs, whose state parameters carry each pushed filter, and the records it suppresses are counted
//! in the filter's own series.
//!
//! Compilation finds the sites on the logical plan after the logical rewrites, and program compilation
//! hands each site's filter, a [`PushedFilter`], to its state owner's per-call parameters
//! ([`OperatorDef::step_params`][crate::operators::OperatorDef::step_params]).
//!
//! # Applicability
//!
//! A state owner `O` and a filter `F` form a site when:
//!
//! 1. `F` is a `split` with one port leading straight to a `drop`, reached by every path from `O`'s output port, with
//!    only `merge`s, or synchronous one-to-one maps without state or signals that preserve identity and kind and
//!    write nothing `F` reads for the kinds that reach them, in between: no `tee`, other `split`, expansion, terminal,
//!    host, or dynamic node;
//! 2. `O` runs as a kernel and can apply a filter at emission (aggregation);
//! 3. `F`'s predicate reads only what a record's context, kind, and metadata carry: never its points;
//! 4. the kinds `K` it suppresses are those of `O`'s input for which `O` preserves identity, emits one kind `k'`
//!    (counters leave as rates, every other kind as it is), and writes nothing `F` reads but the kind, judged as
//!    `k'`; and histograms, whose statistics leave under names and kinds of their own with the context's host and
//!    tags, when `F` reads none of the name, kind, or metadata. `K` must not be empty.
//!
//! Then whatever `O` emits of `K`, `F` judges by exactly what `O` judges at emission, so skipping what `F` would drop
//! changes no output on any trace: under capacity, on context collisions and kind conflicts, across installations that
//! change `F` (each flush judges with its own generation's predicate), and with live bindings. State itself is
//! untouched: admission, capacity, expiry, back-fill, and metadata are what they are without the rewrite.
//!
//! For the native post-aggregation filter, which drops by kind and intake name, `K` is every kind but histograms: the
//! specification's partial push-down, with histogram statistics left to the filter.

use std::{fmt, sync::Arc};

use saluki_core::data_model::event::metric::{context::Context, MetricMetadata};
use saluki_metric_kernels::aggregate::{EmissionFilter, OutputKind, SiteIndex};

use super::{
    filters::{fields, input_kinds, kinds, Filter, FilterForm},
    RewriteId, RewriteOutcome, RewriteRecord, TelemetryChange, TelemetryEffect,
};
use crate::{
    ir::{NodeId, Plan, PortName},
    model::{EventView, Field, FieldSet, Kind, KindSet},
    operators::{
        names, Cardinality, ConfiguredOperator, Identity, OperatorRegistry, StateContract, FALSE_PORT, TRUE_PORT,
    },
    predicate::{BindingResolver, CompiledPredicate, Predicate, PredicateError},
    validate::{graph::Graph, ValidationReport},
};

/// What a filter pushed into a state owner may read: everything but points.
const READABLE_AT_EMISSION: FieldSet = FieldSet::of(&[
    Field::Name,
    Field::Host,
    Field::Tags,
    Field::OriginTags,
    Field::Kind,
    Field::Metadata,
]);

/// What a histogram's statistics do not keep from the histogram: their names, kinds, and metadata are their own.
const RENAMED_BY_HISTOGRAMS: FieldSet = FieldSet::of(&[Field::Name, Field::Kind, Field::Metadata]);

/// The kinds a histogram expands into.
const HISTOGRAM_EXPANSION: KindSet = KindSet::of(&[Kind::Rate, Kind::Gauge, Kind::Distribution]);

/// Where R5b pushed one filter: the state owner that suppresses the records, the filter, the port it drops by, whose
/// series count the suppressed records, and how the owner judges what it emits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushedFilterSite {
    /// The state owner that suppresses the records.
    pub owner: NodeId,

    /// The filter: a `split` with one port straight to a `drop`, below the state owner.
    pub filter: NodeId,

    /// The filter's port that leads to the `drop`, whose series count the records the state owner suppresses.
    pub drop_port: PortName,

    /// The `drop` the filter's port leads to, whose policy loss counts the points of the records the state owner
    /// suppresses.
    pub drop: NodeId,

    /// The filter's predicate, as written in the plan; [`compile`][Self::compile] compiles it.
    pub predicate: Predicate,

    /// Whether the filter drops the records its predicate matches (its `true` port leads to the `drop`) or those it
    /// does not.
    pub drops_matching: bool,

    /// The kinds of the records the state owner emits that it judges, as the filter receives them: a counter's rate is
    /// `rate`. A record of any other kind is emitted, and judged by the filter downstream.
    pub emitted: KindSet,

    /// Whether the state owner judges a histogram's statistics too, all alike, as the filter judges each of them.
    pub histograms: bool,
}

impl PushedFilterSite {
    /// Compiles the site's filter, resolving the configuration bindings its predicate uses as the generation that runs
    /// it does, and numbering it `index` among the sites of its program, which the records it suppresses are counted
    /// by.
    ///
    /// # Errors
    ///
    /// Returns an error if a binding the predicate uses has no value.
    pub fn compile<R>(&self, index: SiteIndex, resolver: &R) -> Result<PushedFilter, PredicateError>
    where
        R: BindingResolver + ?Sized,
    {
        Ok(PushedFilter::new(
            self,
            index,
            Arc::new(self.predicate.compile(resolver)?),
        ))
    }
}

/// A filter that R5b pushed into a state owner: the [`EmissionFilter`] the state owner's kernel applies when it emits.
///
/// It judges a record the state owner is about to emit as the filter downstream would judge it, by its context, the
/// kind it leaves as, and its metadata, with the filter's predicate as compiled in the generation the flush is stamped
/// with, and only for the kinds R5b found the filter to commute with the state owner for.
#[derive(Clone)]
pub struct PushedFilter {
    site: SiteIndex,
    predicate: Arc<CompiledPredicate>,
    drops_matching: bool,
    emitted: KindSet,
    histograms: bool,
}

impl PushedFilter {
    /// Creates the filter of a site from its compiled predicate, numbered `index` among the sites of its program.
    pub fn new(site: &PushedFilterSite, index: SiteIndex, predicate: Arc<CompiledPredicate>) -> Self {
        Self {
            site: index,
            predicate,
            drops_matching: site.drops_matching,
            emitted: site.emitted,
            histograms: site.histograms,
        }
    }

    fn drops_view(&self, view: &mut EventView<'_>) -> bool {
        self.predicate.evaluate(view) == self.drops_matching
    }
}

impl fmt::Debug for PushedFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PushedFilter")
            .field("site", &self.site)
            .field("drops_matching", &self.drops_matching)
            .field("emitted", &self.emitted)
            .field("histograms", &self.histograms)
            .finish_non_exhaustive()
    }
}

/// Returns the kind of record a flush emits of `kind`.
fn emitted_kind(kind: OutputKind) -> Kind {
    match kind {
        OutputKind::Rate => Kind::Rate,
        OutputKind::Gauge => Kind::Gauge,
        OutputKind::Set => Kind::Set,
        OutputKind::Distribution => Kind::Distribution,
    }
}

impl EmissionFilter for PushedFilter {
    fn drops(&self, context: &Context, kind: OutputKind, metadata: &MetricMetadata) -> bool {
        let kind = emitted_kind(kind);
        self.emitted.contains(kind) && self.drops_view(&mut EventView::from_parts(context, kind, metadata))
    }

    fn drops_histogram(&self, context: &Context, metadata: &MetricMetadata) -> bool {
        // The filter reads none of the name, kind, or metadata, which are all a histogram's statistics change, so it
        // judges each of them as it judges any record of the histogram's context.
        self.histograms && self.drops_view(&mut EventView::from_parts(context, Kind::Gauge, metadata))
    }

    fn site(&self) -> SiteIndex {
        self.site
    }
}

/// Finds where R5b applies in a valid plan after the logical rewrites, and records every site considered, applied or
/// skipped, for `explain`.
///
/// Every state owner that runs as a kernel and can apply a filter at emission is considered, in plan order. Returns
/// the sites it applies at, in plan order of their state owners, and one record per state owner considered.
pub(crate) fn analyze(
    plan: &Plan, report: &ValidationReport, registry: &OperatorRegistry,
) -> (Vec<PushedFilterSite>, Vec<RewriteRecord>) {
    let mut validation = ValidationReport::default();
    let graph = Graph::build(plan, registry, &mut validation);
    let mut sites = Vec::new();
    let mut records = Vec::new();
    for owner in 0..graph.nodes.len() {
        let Some(op) = graph.op(owner).filter(|op| suppresses_at_emission(op)) else {
            continue;
        };
        match site_below(&graph, report, owner, op) {
            Ok(found) => {
                records.push(applied(&graph, &found));
                sites.push(found.site);
            }
            Err(Skip { filter, reason }) => {
                let mut nodes = vec![graph.id(owner).clone()];
                nodes.extend(filter.map(|filter| graph.id(filter).clone()));
                records.push(RewriteRecord {
                    id: RewriteId::R5b,
                    outcome: RewriteOutcome::Skipped { reason },
                    nodes,
                    justification: String::new(),
                    telemetry: Vec::new(),
                    capacity_rule: false,
                });
            }
        }
    }
    (sites, records)
}

/// Returns `true` if a node is a state owner that runs as a kernel, in a region, and can skip at emission the records
/// a filter downstream drops: its contract's keyed state declares an emission filter.
///
/// Native aggregation is the one such operator. A state owner run by a host component cannot: its records leave
/// through the host, and no compiled program reaches it.
fn suppresses_at_emission(op: &ConfiguredOperator) -> bool {
    let contract = op.contract();
    matches!(&contract.state, StateContract::Keyed(keyed) if keyed.emission_filter)
        && !contract.boundary.is_host()
        && !contract.is_dynamic()
}

/// Why a state owner has no site, and the filter it considered, if it got as far as one.
struct Skip {
    filter: Option<usize>,
    reason: String,
}

impl Skip {
    fn at(filter: Option<usize>, reason: impl Into<String>) -> Self {
        Self {
            filter,
            reason: reason.into(),
        }
    }
}

/// A site found, with the nodes between the state owner and the filter, for the record.
struct Found {
    owner: usize,
    filter: usize,
    between: Vec<usize>,
    site: PushedFilterSite,
}

/// Finds the filter below `owner`, and checks that it commutes with the owner's emission for some kinds.
fn site_below(
    graph: &Graph<'_>, report: &ValidationReport, owner: usize, op: &ConfiguredOperator,
) -> Result<Found, Skip> {
    if op.ports().outputs.len() != 1 {
        return Err(Skip::at(None, "the state owner does not have exactly one output port"));
    }

    // Condition 1: the one path from the owner's output port, through merges and maps, to a filter.
    let mut between = Vec::new();
    let mut at = (owner, 0);
    let (filter, predicate, drops_true, drop) = loop {
        let Some((next, _)) = graph.target(at.0, at.1) else {
            return Err(Skip::at(None, "the state owner's output port leads nowhere"));
        };
        let Some(next_op) = graph.op(next) else {
            return Err(Skip::at(None, format!("`{}` has no known operator", graph.id(next))));
        };
        if next_op.contract().is_dynamic() {
            return Err(Skip::at(
                None,
                format!(
                    "`{}` reads a host signal, and nothing moves across a dynamic node",
                    graph.id(next)
                ),
            ));
        }
        if let Some(filter) = Filter::at(graph, next) {
            match filter.form {
                FilterForm::Split {
                    predicate,
                    drops_true,
                    drop,
                } => break (next, predicate, drops_true, drop),
                FilterForm::Operator => {
                    return Err(Skip::at(
                        Some(next),
                        format!(
                            "the first filter below it, `{}`, is an operator whose decision is not a predicate the \
                             state owner could evaluate",
                            graph.id(next)
                        ),
                    ))
                }
            }
        }
        if let Some(problem) = uncrossable(next_op) {
            return Err(Skip::at(
                None,
                format!(
                    "`{}`, below it before any filter, {}: a filter beyond it does not judge what the state owner \
                     emits",
                    graph.id(next),
                    problem
                ),
            ));
        }
        between.push(next);
        at = (next, 0);
    };
    // Condition 3: the predicate reads nothing a record's points carry.
    let reads = predicate.reads();
    if reads.contains(Field::Points) || !reads.is_subset(READABLE_AT_EMISSION) {
        return Err(Skip::at(
            Some(filter),
            format!(
                "the filter reads {}, which includes what the state owner computes only as it builds a record",
                fields(reads)
            ),
        ));
    }

    // Condition 4: the kinds whose emission the owner judges as the filter would. A record is judged by the kind it
    // leaves as, which may come from several kinds of the input (rates from counters and from rates), so a kind it
    // leaves as is judged only if every kind of the input that leaves as it commutes.
    let input = input_kinds(graph, report, owner, 0);
    let contract = op.contract();
    let mapping = op.ports().outputs[0].mapping.kinds;
    let unchanged_below = |kind: Kind| {
        between.iter().all(|&node| {
            graph.op(node).is_some_and(|op| {
                op.name() == names::MERGE
                    || (op
                        .ports()
                        .outputs
                        .first()
                        .is_some_and(|port| port.mapping.kinds.get(kind) == KindSet::of(&[kind]))
                        && !op.contract().writes.get(kind).intersects(reads))
            })
        })
    };
    let plain = input.iter().filter(|&kind| kind != Kind::Histogram).collect::<Vec<_>>();
    let commuting = plain
        .iter()
        .copied()
        .filter(|&kind| {
            let leaves_as = mapping.get(kind);
            *contract.identity.get(kind) == Identity::Preserving
                && leaves_as.len() == 1
                && contract
                    .writes
                    .get(kind)
                    .intersection(reads)
                    .is_subset(FieldSet::of(&[Field::Kind]))
                && leaves_as.iter().all(unchanged_below)
        })
        .collect::<KindSet>();
    let emitted = commuting
        .iter()
        .map(|kind| mapping.get(kind))
        .fold(KindSet::EMPTY, KindSet::union)
        .iter()
        .filter(|&out| {
            plain
                .iter()
                .all(|&kind| !mapping.get(kind).contains(out) || commuting.contains(kind))
        })
        .collect::<KindSet>();
    let histograms = input.contains(Kind::Histogram)
        && !reads.intersects(RENAMED_BY_HISTOGRAMS)
        && HISTOGRAM_EXPANSION.iter().all(unchanged_below);
    if emitted.is_empty() && !histograms {
        return Err(Skip::at(
            Some(filter),
            format!(
                "for none of the kinds that reach the state owner ({}) does the filter judge what it emits as it would \
                 at the filter: aggregation writes {} for them",
                kinds(input),
                fields(reads)
            ),
        ));
    }

    let drop_port = PortName::from_static(if drops_true { TRUE_PORT } else { FALSE_PORT });
    Ok(Found {
        owner,
        filter,
        between,
        site: PushedFilterSite {
            owner: graph.id(owner).clone(),
            filter: graph.id(filter).clone(),
            drop_port,
            drop: graph.id(drop).clone(),
            predicate,
            drops_matching: drops_true,
            emitted,
            histograms,
        },
    })
}

/// Returns why the records a state owner emits change on their way through `op` in a way a filter beyond it could
/// judge differently, or `None` if it is a `merge` or a map they pass unchanged, apart from what its contract writes.
fn uncrossable(op: &ConfiguredOperator) -> Option<&'static str> {
    let contract = op.contract();
    match op.name() {
        names::MERGE => return None,
        names::TEE => return Some("copies every record to each of its ports"),
        names::SPLIT => return Some("routes records between its ports, and is not a filter"),
        _ => {}
    }
    if contract.boundary.is_fence() {
        return Some("is a fence");
    }
    if contract.state != StateContract::None {
        return Some("keeps state");
    }
    match contract.cardinality {
        Cardinality::Map => {}
        Cardinality::Sink => return Some("ends the path"),
        _ => return Some("is not a one-to-one map"),
    }
    if op.ports().inputs.len() != 1 || op.ports().outputs.len() != 1 {
        return Some("does not have exactly one input and one output port");
    }
    if Kind::ALL
        .iter()
        .any(|&kind| *contract.identity.get(kind) != Identity::Preserving)
    {
        return Some("does not preserve the identity of what it maps");
    }
    None
}

/// Returns the record of an applied site.
fn applied(graph: &Graph<'_>, found: &Found) -> RewriteRecord {
    let owner = graph.id(found.owner);
    let filter = graph.id(found.filter);
    let site = &found.site;
    let mut kinds_judged = kinds(site.emitted);
    if site.histograms {
        kinds_judged.push_str(", and histograms' statistics");
    }
    let path = match found.between.as_slice() {
        [] => "directly".to_string(),
        [node] => format!(
            "through `{}`, which neither copies nor changes what it reads",
            graph.id(*node)
        ),
        between => format!(
            "through {}, which neither copy nor change what it reads",
            between
                .iter()
                .map(|&node| format!("`{}`", graph.id(node)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    let justification = format!(
        "path: `{}` feeds the filter `{}` {}; reads: the filter reads {}, \
         all known when a record is built; kinds: `{}` judges the records it emits of {} with the filter's predicate, \
         as the filter receives them, and skips those its `{}` port would drop; the filter stays for every other \
         record, and state is unchanged, so every output is the same on every trace",
        owner,
        filter,
        path,
        fields(site.predicate.reads()),
        owner,
        kinds_judged,
        site.drop_port
    );
    let mut telemetry = vec![TelemetryEffect {
        node: owner.clone(),
        change: TelemetryChange::Changed,
        detail: format!(
            "it sends fewer records downstream: none that `{}` drops, which count in `{}`'s own series of its `{}` \
             port and its policy loss, as if it had received them",
            filter, filter, site.drop_port
        ),
    }];
    telemetry.extend(
        found
            .between
            .iter()
            .chain([&found.filter])
            .map(|&node| TelemetryEffect {
                node: graph.id(node).clone(),
                change: TelemetryChange::Changed,
                detail: format!(
            "`plan_node_events_total{{component_id:{}}}` of its input counts fewer records: those `{}` suppresses",
            graph.id(node),
            owner
        ),
            }),
    );
    RewriteRecord {
        id: RewriteId::R5b,
        outcome: RewriteOutcome::Applied,
        nodes: vec![owner.clone(), filter.clone()],
        justification,
        telemetry,
        capacity_rule: false,
    }
}

#[cfg(test)]
mod tests;
