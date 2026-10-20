//! Elaboration: the one pass from a program as written to the core program that installs.
//!
//! A program as written refers to configuration by binding, leaves parameters at their defaults, writes sets in any
//! order, lists its nodes and edges in any order, and names sources, outputs, and recipients. Elaboration resolves all
//! of it and writes the **core program**: every name resolved to a plan ID or port, every binding written as its value,
//! every parameter in its operator's encoded normal form, and the nodes and edges in canonical order. The core plan is
//! what installs, what [`Plan::hash`] hashes (the one hash), what the plan diff, kernel keys, and adoption compare, and
//! what explain shows. Which binding each literal came from is kept beside the plan, as [`BindingUses`], and is not
//! hashed. The default program goes through the same passes ([`elaborate_default`]), so a program that differs from
//! the default program only in how it is written elaborates to the default program's core plan, with its hash.
//!
//! # The core form
//!
//! Each node's parameters are written, in order:
//!
//! 1. with every configuration binding replaced by its resolved value (the value the plan carries, or else the
//!    catalog's), recorded in [`BindingUses`]. A binding without a value is left as it is, for validation to report;
//! 2. in [normal form](crate::normalize::normalize_params): every default written, every predicate normalized. A
//!    binding is a value by now, so a binding and the same literal normalize alike: a `flag` of a bound signal is the
//!    constant its value is;
//! 3. as their operator decodes and encodes them, when it configures them, so that a set written in another order is
//!    the same set.
//!
//! Each step is idempotent, so the core form of a core plan is itself.
//!
//! # Canonical order
//!
//! Nodes and edges mean nothing by their order, so the core plan lists them in an order that depends only on the
//! graph, the one the pipeline syntax prints a program in and lowering writes the default program in: the `source`
//! nodes first, in the order the host lists them, then a depth-first traversal from the edges that leave them, taken in
//! order of the source's binding. Taking an edge appends it; when it is the last edge into its target to be taken, the
//! target is appended and its edges are taken in the order of its operator's output ports. Nodes the traversal does not
//! reach follow in ID order, those that no edge enters first, and edges from nodes that do not exist come last.
//!
//! # Validation
//!
//! The report is the plan's as written, in canonical order, with every binding resolved: the bindings a program names
//! are checked against the catalog, and what a binding's value decides is not reported as if the program had written
//! it, such as a split on a signal that is off now.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::Value;

use crate::{
    canonical::{CanonicalError, PlanHash, BINDING_MEMBER, RESOLVED_MEMBER},
    compose::{
        describe_names, inline_blobs, recipient_names, resolve_recipients, sources, Approval, ComposeError,
        ComposeOutcome, DropReason, DropTarget, DroppedEmits, Unavailable,
    },
    export::{emitted_output, non_source_nodes},
    ir::{
        ArtifactNodeId, ArtifactPortRef, ArtifactRevision, BindingName, Edge, InputName, LocalId, Node, NodeId,
        OperatorName, OutputId, Params, Plan, PlanProvenance, PolicyArtifact, PortName, PortRef, ReceivedArtifact,
        RecipientName,
    },
    normalize::{normalize_artifact, normalize_params, normalize_plan},
    operators::{names, OperatorRegistry, StateContract},
    validate::{Catalog, ValidationReport, Validator},
};

/// A program elaborated into its core form.
#[derive(Clone, Debug)]
pub struct Elaborated {
    /// The core plan: every binding written as its value, every parameter in normal form, the nodes and edges in
    /// canonical order. An artifact's names its revision in its provenance.
    pub plan: Plan,

    /// The plan's validation report, as written with its bindings resolved. A program whose report has errors is
    /// rejected.
    pub report: ValidationReport,

    /// Which plan node each artifact node became: its reserved ID, the policy namespace, or the default program's one
    /// aggregation's ID (amendment 182). A node elaboration adds, such as the nodes of a partly available `emit`, has
    /// none. Empty for the default program.
    pub ids: BTreeMap<ArtifactNodeId, NodeId>,

    /// Which literal of the core plan came from which binding: an annotation explain prints, not part of the hash.
    pub bindings: BindingUses,

    /// Whether the artifact was approved against an active plan other than the one it is elaborated against. The
    /// artifact still installs, with its review reported stale.
    pub stale_review: bool,

    /// Whether the artifact's `emit`s were composed as written, or, for the artifact in effect, some were dropped.
    pub outcome: ComposeOutcome,
}

impl Elaborated {
    /// Returns the core plan's hash: the one hash of what the program does.
    ///
    /// # Errors
    ///
    /// Returns an error if the plan has no canonical form.
    pub fn hash(&self) -> Result<PlanHash, CanonicalError> {
        self.plan.hash()
    }

    /// Returns what the artifact's acknowledgment says about its composition beside its state, in plain text: why it
    /// runs degraded, or `None` if there is nothing to say.
    pub fn notes(&self) -> Option<String> {
        match &self.outcome {
            ComposeOutcome::Degraded(dropped) => {
                Some(dropped.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))
            }
            ComposeOutcome::Composed => None,
        }
    }
}

/// A path to one value inside a node's parameters: an RFC 6901 JSON pointer relative to the node's `params` object,
/// such as `/predicate/flag/operand`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ParamPath(pub String);

impl std::fmt::Display for ParamPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One literal of a core plan that a program wrote as a configuration binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindingUse {
    /// Where the program as written binds it, in the node's parameters before they were put in normal form.
    pub path: ParamPath,

    /// The binding.
    pub binding: BindingName,

    /// The value it resolved to, which the core plan writes.
    pub value: Value,
}

/// Which literal of a core plan came from which configuration binding, by node.
///
/// The core plan writes every binding as its value, so that a binding and the same literal elaborate to one plan, with
/// one hash. This keeps the provenance for explain, which prints `<- <binding>` beside the value.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BindingUses(pub BTreeMap<NodeId, Vec<BindingUse>>);

impl BindingUses {
    /// Returns the bindings the node `node` was written with, in parameter order.
    pub fn of(&self, node: &NodeId) -> &[BindingUse] {
        self.0.get(node).map_or(&[], Vec::as_slice)
    }

    /// Returns `true` if no node was written with a binding.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Elaborates a lowered default program into its core form, with the passes [`elaborate`] runs once an artifact's
/// names are resolved: the plan is put in [normal form](crate::normalize::normalize_plan) and canonical order,
/// validated with `registry` and `catalog`, and written in its core form.
///
/// The core form of a core plan is itself: elaborating an elaborated plan again gives the same plan.
pub fn elaborate_default(lowered: &Plan, catalog: &dyn Catalog, registry: &OperatorRegistry) -> Elaborated {
    let plan = canonical_order(&normalize_plan(lowered), registry);
    let report = Validator::new(registry, catalog).validate(&plan);
    let (plan, bindings) = core_form(plan, catalog, registry);
    Elaborated {
        plan,
        report,
        ids: BTreeMap::new(),
        bindings,
        stale_review: false,
        outcome: ComposeOutcome::Composed,
    }
}

/// Elaborates a received artifact, a whole program, with the host's sources and outputs into its core form.
///
/// `default_plan` is the default program as lowered, with its bindings resolved, and `catalog` the host's catalog of
/// the same configuration. Of the default program, only its `source` nodes, its outputs, and its resources are used.
/// The passes, in order:
///
/// 1. The artifact is put in [normal form](crate::normalize), as native lowering writes the default program, and its
///    blob references are replaced with their content.
/// 2. Each artifact node's plan ID is the reserved ID of the default program's non-source node with the same local ID,
///    or its local ID in the policy namespace; where the catalog runs at most one aggregation per program
///    ([`Catalog::single_aggregation`]), the artifact's one state owner takes the reserved ID of the default program's
///    one state owner, whatever the artifact calls it (amendment 182, until V10). A local ID equal to a source node's
///    is refused ([`ComposeError::ReservedSource`]).
/// 3. An edge from `input.<name>` leaves the port of the source whose `binding` is `<name>` ([`sources`]); a name no
///    source has is refused ([`ComposeError::UnknownSource`]).
/// 4. An `emit` to recipients is addressed to the output with exactly their recipients; an `emit` to an output must
///    name one the catalog declares (see [Outputs that change under an
///    artifact](crate::compose#outputs-that-change-under-an-artifact)).
/// 5. Configuration bindings resolve from `catalog`.
/// 6. The plan is the default program's `source` nodes, then the artifact's nodes; its edges are the artifact's; its
///    outputs and resources are the default program's; and its provenance names the artifact's revision. Its nodes and
///    edges are put in [canonical order](#canonical-order), and it is validated with `registry` and `catalog`, against
///    the default program, as written with its bindings resolved; the report is returned, not judged.
/// 7. The plan is written in its [core form](#the-core-form).
///
/// # Errors
///
/// Returns an error if the artifact cannot be composed at all; see [`ComposeError`].
pub fn elaborate(
    default_plan: &Plan, received: &ReceivedArtifact, approval: &Approval, catalog: &dyn Catalog,
    registry: &OperatorRegistry,
) -> Result<Elaborated, ComposeError> {
    // The artifact composes in normal form, as the default program is lowered in it, so that a program that differs
    // from the default program only in how it is written composes to the default program.
    let artifact = &normalize_artifact(&received.artifact);
    let sources = sources(default_plan);
    if let Some(binding) = shared_source_binding(default_plan) {
        return Err(ComposeError::DefaultProgram {
            message: format!("more than one of its sources has the binding `{}`", binding),
        });
    }
    let program = non_source_nodes(default_plan);
    let stale_review = approval
        .approved_against
        .is_some_and(|approved| approval.active_plan_hash != Some(approved));

    // Every artifact node's parameters with blobs inlined, in artifact order.
    let inlined = artifact
        .nodes
        .iter()
        .map(|node| {
            inline_blobs(&node.params, &artifact.blobs).map_err(|error| ComposeError::Malformed {
                message: format!("node `{}`: {}", node.id, error),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let aggregation = catalog
        .single_aggregation()
        .then(|| the_aggregation(default_plan, artifact, &inlined, &program, catalog, registry))
        .flatten();
    let ids = node_ids(default_plan, artifact, &program, aggregation)?;
    let mut edges = artifact
        .edges
        .iter()
        .enumerate()
        .map(|(index, edge)| {
            Ok(Edge {
                from: plan_port(&edge.from, index, End::From, &sources, &ids)?,
                to: plan_port(&edge.to, index, End::To, &sources, &ids)?,
            })
        })
        .collect::<Result<Vec<_>, ComposeError>>()?;

    // Each artifact node with its plan ID, its parameters with blobs inlined and any recipients it emits to resolved
    // to their output, and how it is addressed.
    let mut placed = Vec::with_capacity(artifact.nodes.len());
    let mut outputs = EmittedOutputs::default();
    for (node, params) in artifact.nodes.iter().zip(inlined) {
        let (params, addressed) = address(&node.id, &node.op, params, catalog)?;
        // A new artifact waits for every recipient it names; only the artifact in effect delivers to those that remain.
        if let (Addressed::Partial { missing, .. }, false) = (&addressed, approval.in_effect) {
            outputs
                .pending_recipients
                .get_or_insert(ComposeError::UnknownRecipient {
                    node: node.id.clone(),
                    recipient: missing[0].clone(),
                });
        }
        if let (Addressed::Unavailable { recipients, reason }, false) = (&addressed, approval.in_effect) {
            // A new artifact is refused for recipients on two hosts, and waits for unknown recipients, and for an
            // output for its recipients, unless another error refuses it first.
            let pending = match reason {
                Unavailable::TwoHosts(hosts) => {
                    return Err(ComposeError::RecipientsOnTwoHosts {
                        node: node.id.clone(),
                        recipients: hosts.clone(),
                    })
                }
                Unavailable::Unknown(recipient) => ComposeError::UnknownRecipient {
                    node: node.id.clone(),
                    recipient: recipient.clone(),
                },
                Unavailable::NoOutput => ComposeError::PendingRecipients {
                    node: node.id.clone(),
                    recipients: recipients.clone(),
                },
                Unavailable::Refused(why) => {
                    return Err(ComposeError::NoOutputFor {
                        node: node.id.clone(),
                        recipients: recipients.clone(),
                        reason: why.clone(),
                    })
                }
            };
            outputs.pending_recipients.get_or_insert(pending);
        }
        placed.push((node, ids[&node.id].clone(), params, addressed));
    }

    // The default program's sources, in its order, then the artifact's nodes, in artifact order.
    let mut nodes = default_plan
        .nodes
        .iter()
        .filter(|node| !program.contains(&node.id))
        .cloned()
        .collect::<Vec<_>>();
    // The plan IDs taken so far, which the nodes a partly available `emit` adds must not take.
    let mut taken = ids.values().cloned().collect::<BTreeSet<_>>();
    taken.extend(default_plan.nodes.iter().map(|node| node.id.clone()));
    let counted_drop = |id: &NodeId| Node {
        id: id.clone(),
        op: OperatorName::new(names::DROP).expect("`drop` is an operator name"),
        params: Params::new(),
    };
    for (node, id, params, addressed) in placed {
        match addressed {
            Addressed::Partial {
                output,
                available,
                missing,
            } => {
                // The artifact in effect names recipients some of which are no longer available: the `emit` keeps
                // delivering to those that remain, through the output with exactly their recipients, and what it would
                // have sent the others is counted as dropped. The node becomes a `tee` whose first branch emits to the
                // remaining recipients and whose second is a counted drop.
                let (delivered, dropped) = partial_emit(&id, output, &mut taken);
                let reason = format!(
                    "{}; the `emit` still delivers to {}",
                    Unavailable::Unknown(missing[0].clone()).describe(),
                    describe_names(&available)
                );
                let entry = outputs
                    .dropped
                    .entry(DropTarget::Recipients(missing))
                    .or_insert_with(|| (DropReason::Unavailable(reason), Vec::new()));
                entry.1.push(dropped.id.clone());
                for (port, target) in [("0", &delivered.id), ("1", &dropped.id)] {
                    edges.push(Edge {
                        from: PortRef::new(id.clone(), port.parse().expect("a tee's port is a port name")),
                        to: PortRef::new(target.clone(), "in".parse().expect("`in` is a port name")),
                    });
                }
                let mut ports = Params::new();
                ports.insert("ports".parse().expect("`ports` is a parameter name"), Value::from(2));
                nodes.push(Node {
                    id,
                    op: OperatorName::new(names::TEE).expect("`tee` is an operator name"),
                    params: ports,
                });
                nodes.push(delivered);
                nodes.push(dropped);
            }
            Addressed::Unavailable { recipients, reason } => {
                // The artifact in effect keeps running, but nothing it emits reaches recipients that are no longer
                // available, or no longer on one host: the `emit` becomes a counted drop, under its ID.
                let entry = outputs
                    .dropped
                    .entry(DropTarget::Recipients(recipients))
                    .or_insert_with(|| (DropReason::Unavailable(reason.describe()), Vec::new()));
                entry.1.push(id.clone());
                nodes.push(counted_drop(&id));
            }
            Addressed::Recipients => nodes.push(Node {
                id,
                op: node.op.clone(),
                params,
            }),
            Addressed::Output => {
                let params = resolve_bindings(params, catalog, &node.id)?;
                let undeclared = (node.op.as_str() == names::EMIT)
                    .then(|| emitted_output(&params))
                    .flatten()
                    .filter(|output| catalog.output(output).is_none());
                match undeclared {
                    // The artifact in effect keeps running, but nothing it emits reaches an output that was removed:
                    // the `emit` becomes a counted drop, under its ID.
                    Some(output) if approval.in_effect => {
                        let entry = outputs
                            .dropped
                            .entry(DropTarget::Output(output))
                            .or_insert_with(|| (DropReason::Removed, Vec::new()));
                        entry.1.push(id.clone());
                        nodes.push(counted_drop(&id));
                    }
                    Some(output) => {
                        let declared = default_plan
                            .outputs
                            .iter()
                            .map(|declared| declared.id.clone())
                            .filter(|id| catalog.output(id).is_some())
                            .collect();
                        return Err(ComposeError::UndeclaredOutput {
                            node: node.id.clone(),
                            output,
                            declared,
                        });
                    }
                    None => nodes.push(Node {
                        id,
                        op: node.op.clone(),
                        params,
                    }),
                }
            }
        }
    }
    // A new artifact otherwise fine waits for the first recipients it names that have no output: it is refused with
    // any other error first, since that one would not go away when the recipients are declared.
    if let Some(pending) = outputs.pending_recipients {
        return Err(pending);
    }

    let outcome = if outputs.dropped.is_empty() {
        ComposeOutcome::Composed
    } else {
        ComposeOutcome::Degraded(
            outputs
                .dropped
                .into_iter()
                .map(|(target, (reason, nodes))| DroppedEmits { target, reason, nodes })
                .collect(),
        )
    };
    let plan = Plan {
        ir_version: default_plan.ir_version,
        provenance: Some(PlanProvenance {
            config_change: default_plan
                .provenance
                .as_ref()
                .and_then(|provenance| provenance.config_change),
            artifact: Some(ArtifactRevision {
                revision: approval.revision,
                hash: received.hash,
            }),
            // A composed plan is never a boot plan: the native components describe the default program's layout only.
            native_components: BTreeMap::new(),
        }),
        nodes,
        edges,
        outputs: default_plan.outputs.clone(),
        // Resources, like output declarations, come from the default program only: an artifact cannot declare them.
        resources: default_plan.resources.clone(),
    };
    let plan = canonical_order(&plan, registry);
    let report = Validator::new(registry, catalog)
        .with_baseline(default_plan)
        .validate(&plan);
    let (plan, bindings) = core_form(plan, catalog, registry);
    let ids = ids.into_iter().map(|(local, id)| (local.clone(), id)).collect();
    Ok(Elaborated {
        plan,
        report,
        ids,
        bindings,
        stale_review,
        outcome,
    })
}

/// Returns a binding two of the `source` nodes of `plan` share, if any do: a host names each source once.
fn shared_source_binding(plan: &Plan) -> Option<&str> {
    let mut seen = BTreeSet::new();
    plan.nodes
        .iter()
        .filter(|node| node.op.as_str() == names::SOURCE)
        .filter_map(|node| node.params.get("binding").and_then(Value::as_str))
        .find(|binding| !seen.insert(*binding))
}

/// Which end of an edge a port is at.
#[derive(Clone, Copy)]
enum End {
    From,
    To,
}

/// Returns the plan port an artifact edge's end refers to: the port of the source of that name, or a port of an
/// artifact node, under its plan ID.
///
/// # Errors
///
/// Returns an error if the end is a source the host does not have, a source the edge enters, or a port of a node the
/// artifact does not have.
fn plan_port(
    reference: &ArtifactPortRef, index: usize, end: End, sources: &BTreeMap<InputName, PortRef>,
    ids: &BTreeMap<&ArtifactNodeId, NodeId>,
) -> Result<PortRef, ComposeError> {
    match (reference, end) {
        (ArtifactPortRef::Input(input), End::From) => {
            sources.get(input).cloned().ok_or_else(|| ComposeError::UnknownSource {
                name: input.clone(),
                known: sources.keys().cloned().collect(),
            })
        }
        (ArtifactPortRef::Input(_), End::To) => Err(ComposeError::Malformed {
            message: format!(
                "edge {} enters `{}`, which is a source; edges leave the host's sources",
                index, reference
            ),
        }),
        (ArtifactPortRef::Node { node, port }, end) => ids
            .get(node)
            .map(|id| PortRef::new(id.clone(), port.clone()))
            .ok_or_else(|| ComposeError::Malformed {
                message: format!(
                    "edge {} {} `{}`, but the artifact has no node `{}`",
                    index,
                    match end {
                        End::From => "leaves",
                        End::To => "enters",
                    },
                    reference,
                    node
                ),
            }),
    }
}

/// What composing one artifact's `emit`s found about the outputs they deliver to.
#[derive(Default)]
struct EmittedOutputs {
    /// The outputs, or recipients, whose `emit`s were dropped, with why and the nodes, for the artifact in effect.
    dropped: BTreeMap<DropTarget, (DropReason, Vec<NodeId>)>,

    /// Why a new artifact waits for the recipients an `emit` names, for the first such `emit`: a name the directory
    /// does not have, or no output with exactly their recipients yet.
    pending_recipients: Option<ComposeError>,
}

/// How an artifact node delivers, if it is an `emit`.
enum Addressed {
    /// Not an `emit` addressed to recipients: another operator, or an `emit` naming its output.
    Output,

    /// An `emit` addressed to recipients, resolved to the output whose recipients they are.
    Recipients,

    /// An `emit` of recipients some of which the directory no longer names: the others, `available`, resolve to
    /// `output`, which the artifact in effect goes on delivering to, while a new artifact waits for `missing`.
    Partial {
        /// The output with exactly the available recipients.
        output: OutputId,

        /// The names the directory has, as the artifact writes them.
        available: Vec<RecipientName>,

        /// The names it does not have.
        missing: Vec<RecipientName>,
    },

    /// An `emit` addressed to recipients that have no output now.
    Unavailable {
        /// The names, as the artifact writes them.
        recipients: Vec<RecipientName>,

        /// Why.
        reason: Unavailable,
    },
}

/// Resolves the recipients an `emit` node `node` names in its `to` parameter, if it has one, into the output whose
/// recipients are exactly theirs, which `params` then name in place of them.
///
/// Each name resolves through the host's recipient directory to the recipients it stands for. The names must stand for
/// recipients of one delivery host, since an output is one encoder stream on one host. Their recipients, sorted and
/// without repeats, are matched to an output with exactly those recipients ([`Catalog::output_for`]): a native output,
/// a declared one, or one the host added for a program's demand ([`demanded_outputs`]). The names' order and spelling
/// never matter, only the recipients they stand for.
///
/// # Errors
///
/// Returns an error if `to` is not a list of recipient names, or is given beside `output`, for which validation would
/// report the `emit` anyway.
fn address(
    node: &ArtifactNodeId, op: &OperatorName, mut params: Params, catalog: &dyn Catalog,
) -> Result<(Params, Addressed), ComposeError> {
    if op.as_str() != names::EMIT {
        return Ok((params, Addressed::Output));
    }
    let Some(names) = recipient_names(node, &params)? else {
        return Ok((params, Addressed::Output));
    };
    match resolve_recipients(&names, catalog) {
        Ok(recipients) => match catalog.output_for(&recipients) {
            Some(output) => {
                params = std::iter::once((
                    "output".parse().expect("`output` is a parameter name"),
                    Value::String(output.to_string()),
                ))
                .collect();
                Ok((params, Addressed::Recipients))
            }
            None => {
                let reason = match catalog.implied_output_refused(&recipients) {
                    Some(why) => Unavailable::Refused(why),
                    None => Unavailable::NoOutput,
                };
                Ok((
                    params,
                    Addressed::Unavailable {
                        recipients: names,
                        reason,
                    },
                ))
            }
        },
        Err(reason) => {
            // Some names the directory no longer has: the others may still resolve to an output.
            if let Unavailable::Unknown(_) = reason {
                let (available, missing): (Vec<_>, Vec<_>) = names
                    .iter()
                    .cloned()
                    .partition(|name| catalog.recipient(name).is_some());
                let output = (!available.is_empty())
                    .then(|| resolve_recipients(&available, catalog).ok())
                    .flatten()
                    .and_then(|recipients| catalog.output_for(&recipients));
                if let Some(output) = output {
                    return Ok((
                        params,
                        Addressed::Partial {
                            output,
                            available,
                            missing,
                        },
                    ));
                }
            }
            Ok((
                params,
                Addressed::Unavailable {
                    recipients: names,
                    reason,
                },
            ))
        }
    }
}

/// Returns the two nodes a partly available `emit` with plan ID `id` adds after it, once it is a `tee`: an `emit` to
/// `output`, the output of its remaining recipients, and a counted drop, which counts what it would have sent the
/// recipients that are gone. Their IDs are `id`'s with `_delivered` and `_missing` after its local part, cut to fit an
/// identifier and numbered when taken; each is added to `taken`.
fn partial_emit(id: &NodeId, output: OutputId, taken: &mut BTreeSet<NodeId>) -> (Node, Node) {
    let mut derive = |suffix: &str| {
        let local = id.local().as_str();
        let base = &local[..local.len().min(crate::ir::MAX_IDENTIFIER_LEN - suffix.len() - 3)];
        let base = base.trim_end_matches('_');
        let mut attempt = 1usize;
        loop {
            let candidate = if attempt == 1 {
                format!("{}{}", base, suffix)
            } else {
                format!("{}{}_{}", base, suffix, attempt)
            };
            let node = LocalId::new(candidate)
                .map(NodeId::policy)
                .expect("a derived local ID is an identifier");
            if taken.insert(node.clone()) {
                return node;
            }
            attempt += 1;
        }
    };
    let delivered = Node {
        id: derive("_delivered"),
        op: OperatorName::new(names::EMIT).expect("`emit` is an operator name"),
        params: std::iter::once((
            "output".parse().expect("`output` is a parameter name"),
            Value::String(output.to_string()),
        ))
        .collect(),
    };
    let dropped = Node {
        id: derive("_missing"),
        op: OperatorName::new(names::DROP).expect("`drop` is an operator name"),
        params: Params::new(),
    };
    (delivered, dropped)
}

/// Returns the artifact's one state owner and the reserved ID it takes, for a host that runs at most one aggregation
/// per program: when the artifact has exactly one state owner, the default program has exactly one among `program`, its
/// non-source nodes, and no artifact node has that one's local ID, the artifact's state owner is the default program's,
/// whatever it is called.
///
/// So a program's one aggregation keeps the default program's ID (`dsd_agg`, say), and with it its lineage, its
/// isolation slot's series, and its telemetry, when an author renames it or writes the program from scratch. Whether
/// its state is kept is then decided as for any node with that ID: by its operation and its lineage parameters.
///
/// A node whose operator does not configure, with its bindings resolved from the catalog, is not counted: validation
/// reports it.
fn the_aggregation<'a>(
    default_plan: &Plan, artifact: &'a PolicyArtifact, inlined: &[Params], program: &BTreeSet<NodeId>,
    catalog: &dyn Catalog, registry: &OperatorRegistry,
) -> Option<(&'a ArtifactNodeId, NodeId)> {
    let owns_state = |op: &OperatorName, params: &Params| {
        registry
            .configure(op.as_str(), params)
            .is_ok_and(|configured| matches!(configured.contract().state, StateContract::Keyed(_)))
    };
    let region_owners = default_plan
        .nodes
        .iter()
        .filter(|node| program.contains(&node.id) && owns_state(&node.op, &node.params))
        .collect::<Vec<_>>();
    let artifact_owners = artifact
        .nodes
        .iter()
        .zip(inlined)
        .filter(|(node, params)| {
            resolve_bindings((*params).clone(), catalog, &node.id).is_ok_and(|params| owns_state(&node.op, &params))
        })
        .collect::<Vec<_>>();
    let ([region_owner], [(artifact_owner, _)]) = (&region_owners[..], &artifact_owners[..]) else {
        return None;
    };
    let taken = artifact
        .nodes
        .iter()
        .any(|node| node.id.as_str() == region_owner.id.local().as_str());
    (!taken).then(|| (&artifact_owner.id, region_owner.id.clone()))
}

/// Assigns every artifact node its plan ID: the reserved ID of a node of `program`, the default program's non-source
/// nodes, with the same local ID, or its local ID in the policy namespace. `aggregation`, if given, is the artifact's
/// one state owner and the reserved ID it takes instead ([`the_aggregation`]).
///
/// # Errors
///
/// Returns an error if two artifact nodes have the same ID, or one has the ID of one of the default program's sources.
fn node_ids<'a>(
    default_plan: &Plan, artifact: &'a PolicyArtifact, program: &BTreeSet<NodeId>,
    aggregation: Option<(&'a ArtifactNodeId, NodeId)>,
) -> Result<BTreeMap<&'a ArtifactNodeId, NodeId>, ComposeError> {
    let mut ids = BTreeMap::new();
    for node in &artifact.nodes {
        let reserved = NodeId::reserved(node.id.to_local_id());
        // A source's ID is refused even for the adopted aggregation, whose plan ID would not collide: the artifact's
        // ID names the source, whatever plan ID the node takes.
        if !program.contains(&reserved) && default_plan.node(&reserved).is_some() {
            return Err(ComposeError::ReservedSource {
                node: node.id.clone(),
                reserved,
            });
        }
        let id = if let Some((_, id)) = aggregation.as_ref().filter(|(owner, _)| **owner == node.id) {
            id.clone()
        } else if program.contains(&reserved) {
            reserved
        } else {
            NodeId::policy(node.id.to_local_id())
        };
        if ids.insert(&node.id, id).is_some() {
            return Err(ComposeError::Malformed {
                message: format!("more than one node has the ID `{}`; give each node its own ID", node.id),
            });
        }
    }
    Ok(ids)
}

/// Fills in the resolved value of every configuration binding in `params` that the catalog declares.
///
/// # Errors
///
/// Returns an error for the first binding the catalog declares without a value.
fn resolve_bindings(params: Params, catalog: &dyn Catalog, node: &ArtifactNodeId) -> Result<Params, ComposeError> {
    params
        .iter()
        .map(|(name, value)| {
            let mut value = value.clone();
            resolve_value(&mut value, catalog, node)?;
            Ok((name.clone(), value))
        })
        .collect()
}

/// Fills in the resolved value of every configuration binding in one parameter value.
///
/// A binding is an object with a `config` member, which parameters reserve for bindings. A binding whose name is
/// invalid, or that the catalog does not declare, is left as it is, for validation to report.
fn resolve_value(value: &mut Value, catalog: &dyn Catalog, node: &ArtifactNodeId) -> Result<(), ComposeError> {
    match value {
        Value::Object(members) if members.contains_key(BINDING_MEMBER) => {
            let Some(binding) = members
                .get(BINDING_MEMBER)
                .and_then(Value::as_str)
                .and_then(|name| name.parse::<BindingName>().ok())
            else {
                return Ok(());
            };
            let Some(declared) = catalog.binding(&binding) else {
                return Ok(());
            };
            let resolved = declared.value.clone().ok_or_else(|| ComposeError::UnresolvedBinding {
                node: node.clone(),
                binding,
            })?;
            members.insert(RESOLVED_MEMBER.to_string(), resolved);
            Ok(())
        }
        Value::Object(members) => members
            .values_mut()
            .try_for_each(|member| resolve_value(member, catalog, node)),
        Value::Array(values) => values
            .iter_mut()
            .try_for_each(|element| resolve_value(element, catalog, node)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Ok(()),
    }
}

/// Writes a plan in its [core form](self#the-core-form): each node's bindings as their values, recorded in the
/// returned [`BindingUses`], and its parameters in normal form, as their operator encodes them.
fn core_form(mut plan: Plan, catalog: &dyn Catalog, registry: &OperatorRegistry) -> (Plan, BindingUses) {
    let mut bindings = BTreeMap::new();
    for node in &mut plan.nodes {
        let mut uses = Vec::new();
        let params = node
            .params
            .iter()
            .map(|(name, value)| {
                let mut value = value.clone();
                write_values(
                    &mut value,
                    &mut pointer_to(String::new(), name.as_str()),
                    catalog,
                    &mut uses,
                );
                (name.clone(), value)
            })
            .collect::<Params>();
        let params = normalize_params(&node.op, &params);
        node.params = match registry.configure(node.op.as_str(), &params) {
            Ok(configured) => configured.params().encode(),
            Err(_) => params,
        };
        if !uses.is_empty() {
            bindings.insert(node.id.clone(), uses);
        }
    }
    (plan, BindingUses(bindings))
}

/// Replaces every configuration binding in `value`, one parameter's value at the JSON pointer `path`, with its resolved
/// value: the one it carries, or else the catalog's. Each one replaced is pushed to `uses`; one without a value is left
/// as it is.
///
/// A binding is an object with a `config` member, which parameters reserve for bindings.
fn write_values(value: &mut Value, path: &mut String, catalog: &dyn Catalog, uses: &mut Vec<BindingUse>) {
    match value {
        Value::Object(members) if members.contains_key(BINDING_MEMBER) => {
            let Some(binding) = members
                .get(BINDING_MEMBER)
                .and_then(Value::as_str)
                .and_then(|name| name.parse::<BindingName>().ok())
            else {
                return;
            };
            let resolved = members
                .get(RESOLVED_MEMBER)
                .cloned()
                .or_else(|| catalog.binding(&binding).and_then(|declared| declared.value.clone()));
            if let Some(resolved) = resolved {
                uses.push(BindingUse {
                    path: ParamPath(path.clone()),
                    binding,
                    value: resolved.clone(),
                });
                *value = resolved;
            }
        }
        Value::Object(members) => {
            for (key, member) in members.iter_mut() {
                let len = path.len();
                *path = pointer_to(std::mem::take(path), key);
                write_values(member, path, catalog, uses);
                path.truncate(len);
            }
        }
        Value::Array(elements) => {
            for (index, element) in elements.iter_mut().enumerate() {
                let len = path.len();
                *path = pointer_to(std::mem::take(path), &index.to_string());
                write_values(element, path, catalog, uses);
                path.truncate(len);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// Appends one reference token to a JSON pointer, escaped as RFC 6901 says: `~` as `~0`, `/` as `~1`.
fn pointer_to(mut pointer: String, token: &str) -> String {
    pointer.push('/');
    pointer.push_str(&token.replace('~', "~0").replace('/', "~1"));
    pointer
}

/// Returns `plan` with its nodes and edges in [canonical order](self#canonical-order).
///
/// The order depends only on the graph, and on the order of the `source` nodes, never on the order the other nodes and
/// edges are given in (apart from tie-breaks between nodes with the same ID, or edges with the same ends, which are
/// invalid).
fn canonical_order(plan: &Plan, registry: &OperatorRegistry) -> Plan {
    let nodes = &plan.nodes;
    let mut index: HashMap<&NodeId, usize> = HashMap::new();
    for (i, node) in nodes.iter().enumerate() {
        index.entry(&node.id).or_insert(i);
    }
    let is_source = |i: usize| nodes[i].op.as_str() == names::SOURCE;
    // The output ports of each node in its operator's order, when the operator configures.
    let outputs = nodes
        .iter()
        .map(|node| {
            registry
                .configure(node.op.as_str(), &node.params)
                .map(|configured| {
                    configured
                        .ports()
                        .outputs
                        .iter()
                        .enumerate()
                        .map(|(position, port)| (port.name.clone(), position))
                        .collect::<HashMap<_, _>>()
                })
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    let port_rank = |node: usize, port: &PortName| {
        let declared = outputs[node].get(port).copied();
        (
            declared.is_none(),
            declared,
            port.as_index().is_none(),
            port.as_index(),
            port.as_str().to_string(),
        )
    };

    let mut remaining = vec![0usize; nodes.len()];
    let mut leaving: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    let mut dangling = Vec::new();
    for (i, edge) in plan.edges.iter().enumerate() {
        if let Some(&target) = index.get(&edge.to.node) {
            remaining[target] += 1;
        }
        match index.get(&edge.from.node) {
            Some(&source) => leaving[source].push(i),
            None => dangling.push(i),
        }
    }
    for (node, out) in leaving.iter_mut().enumerate() {
        out.sort_by_key(|&edge| {
            (
                port_rank(node, &plan.edges[edge].from.port),
                plan.edges[edge].to.clone(),
                edge,
            )
        });
    }
    dangling.sort_by_key(|&edge| (plan.edges[edge].from.clone(), plan.edges[edge].to.clone(), edge));

    let mut order = Traversal {
        visited: vec![false; nodes.len()],
        remaining,
        nodes: Vec::with_capacity(nodes.len()),
        edges: Vec::with_capacity(plan.edges.len()),
    };
    // The sources first, in the host's order; then the edges that leave them, in order of their binding.
    let mut sources = (0..nodes.len()).filter(|&i| is_source(i)).collect::<Vec<_>>();
    for &source in &sources {
        order.visited[source] = true;
        order.nodes.push(source);
    }
    let binding = |i: usize| {
        nodes[i]
            .params
            .get("binding")
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    sources.sort_by(|a, b| binding(*a).cmp(binding(*b)).then_with(|| a.cmp(b)));
    for source in sources {
        for &edge in &leaving[source] {
            order.take(edge, &plan.edges, &index, &leaving);
        }
    }

    let mut leftover = (0..nodes.len()).filter(|&i| !order.visited[i]).collect::<Vec<_>>();
    leftover.sort_by(|a, b| nodes[*a].id.cmp(&nodes[*b].id).then_with(|| a.cmp(b)));
    for unentered in [true, false] {
        for &node in &leftover {
            if !order.visited[node] && (!unentered || order.remaining[node] == 0) {
                order.visit(node, &plan.edges, &index, &leaving);
            }
        }
    }
    order.edges.extend(dangling);

    Plan {
        nodes: order.nodes.iter().map(|&i| nodes[i].clone()).collect(),
        edges: order.edges.iter().map(|&i| plan.edges[i].clone()).collect(),
        ..plan.clone()
    }
}

/// The state of the depth-first traversal of [`canonical_order`]: positions of nodes and edges in the plan as given.
struct Traversal {
    visited: Vec<bool>,
    remaining: Vec<usize>,
    nodes: Vec<usize>,
    edges: Vec<usize>,
}

impl Traversal {
    /// Takes the edge at position `edge`, and visits its target if that was the last edge into it.
    fn take(&mut self, edge: usize, edges: &[Edge], index: &HashMap<&NodeId, usize>, leaving: &[Vec<usize>]) {
        self.edges.push(edge);
        if let Some(&target) = index.get(&edges[edge].to.node) {
            self.remaining[target] = self.remaining[target].saturating_sub(1);
            if self.remaining[target] == 0 && !self.visited[target] {
                self.visit(target, edges, index, leaving);
            }
        }
    }

    /// Visits the node at position `start`, and every node its edges complete, depth first, with a stack of its own,
    /// so that a long chain of nodes cannot exhaust the thread's stack.
    fn visit(&mut self, start: usize, edges: &[Edge], index: &HashMap<&NodeId, usize>, leaving: &[Vec<usize>]) {
        self.visited[start] = true;
        self.nodes.push(start);
        let mut stack = vec![(start, 0usize)];
        while let Some((node, next)) = stack.last_mut() {
            let Some(&edge) = leaving[*node].get(*next) else {
                stack.pop();
                continue;
            };
            *next += 1;
            self.edges.push(edge);
            if let Some(&target) = index.get(&edges[edge].to.node) {
                self.remaining[target] = self.remaining[target].saturating_sub(1);
                if self.remaining[target] == 0 && !self.visited[target] {
                    self.visited[target] = true;
                    self.nodes.push(target);
                    stack.push((target, 0));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
