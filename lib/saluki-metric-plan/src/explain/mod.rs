//! `explain`: a stable, versioned description of a plan.
//!
//! [`Explain::of`] describes a plan and what validation computed for it: its logical graph (nodes, their operators,
//! parameters with every configuration binding and its resolved value, ports, and provenance), the port type carried
//! by every edge, its outputs and scopes, its resource bounds against their limits, and the validation findings. The
//! document has a deterministic JSON form ([`Explain::to_json`]) and a deterministic text rendering
//! ([`Explain::to_text`]).
//!
//! # Versioning
//!
//! Every document carries [`EXPLAIN_VERSION`] as `explain_version`. Within a version, changes are additive: new
//! members and new sections may appear, and consumers ignore members they do not know, as this crate's own
//! deserialization does. A member is never removed, renamed, or given a different meaning within a version; such a
//! change increments the version.
//!
//! Sections arrive with the capabilities they describe. Version 1 has the logical graph, port types, provenance, and
//! resource bounds, and, for a plan placed into a frame ([`Explain::with_physical`]), the physical sections: the
//! frame's fixed components with their admission points, its slots with their entries and exits (idle ones marked),
//! the physical edges including direct ones, rewrite R1's fusion decision for every edge of the boot plan, the
//! segments between admission points, and each output's binding with its recipients and delivery policy, and, for a
//! compiled plan ([`Explain::with_rewrites`]), the rewrites the compiler considered, applied or skipped, each with its
//! justification, whether it relies on the capacity rule, and its telemetry effects. Later stages add the plan diff,
//! and state owners with their transitions.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    canonical::{PlanHash, ResolvedValues},
    compile::{RewriteOutcome, RewriteRecord},
    extension::DeliveryPolicy,
    ir::{
        ArtifactRevision, BindingName, ExitName, InputName, NodeId, OperatorName, OutputId, ParamName, Plan,
        PlanProvenance, PortName, PortRef, Provenance, RecipientId, ScopeName,
    },
    model::PortType,
    operators::{Boundary, Loss, LossClass, OperatorRegistry, StateContract},
    validate::{binding_uses, BoundScope, Severity, ValidationReport},
};

mod physical;
pub use self::physical::{
    AdmissionPointEntry, ExitEntry, FixedEntry, FusionEntry, IsolationEntry, OutputBindingEntry, PhysicalEdgeEntry,
    PhysicalPlan, SegmentEntry, SlotEntry, SlotInputEntry,
};

mod text;

/// The version of the explain document this crate produces.
pub const EXPLAIN_VERSION: u32 = 1;

/// The explain document of one plan.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Explain {
    /// The version of the document's format: [`EXPLAIN_VERSION`].
    pub explain_version: u32,

    /// The plan as a whole.
    pub plan: PlanSummary,

    /// The logical graph.
    pub graph: LogicalGraph,

    /// The resource bounds validation measured, against their limits.
    pub bounds: Vec<BoundEntry>,

    /// The validation findings, in the order validation found them.
    pub findings: Vec<FindingEntry>,

    /// The rewrites the compiler considered for the plan, applied or skipped, if it was compiled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewrites: Option<Vec<RewriteEntry>>,

    /// How the plan runs in the frame built at boot, if it was placed into one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical: Option<PhysicalPlan>,
}

/// The plan as a whole.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PlanSummary {
    /// The IR version the plan is written in.
    pub ir_version: u32,

    /// The plan's hash, including resolved configuration values; `None` if the plan has no canonical form.
    pub hash: Option<PlanHash>,

    /// The plan's hash without resolved configuration values, which changes only when its structure or literal
    /// parameters change; `None` if the plan has no canonical form.
    pub structural_hash: Option<PlanHash>,

    /// Where the plan came from, if it says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<PlanProvenance>,

    /// Whether the plan is valid: whether validation found no errors.
    pub valid: bool,
}

/// A plan's logical graph.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LogicalGraph {
    /// The nodes, in plan order.
    pub nodes: Vec<NodeEntry>,

    /// The edges, in plan order, each with the type of what it carries.
    pub edges: Vec<EdgeEntry>,

    /// The declared outputs, in plan order.
    pub outputs: Vec<OutputEntry>,

    /// The replaceable scopes, by name.
    pub scopes: Vec<ScopeEntry>,
}

/// One node.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NodeEntry {
    /// The node's ID.
    pub id: NodeId,

    /// The node's operator.
    pub op: OperatorName,

    /// Where the operator runs: `synchronous`, `io_host`, or `opaque_host`; `None` if the operator is unknown or its
    /// parameters are invalid.
    pub boundary: Option<String>,

    /// The state the operator keeps: `none`, `keyed`, or `opaque`; `None` if the operator is not configured.
    pub state: Option<String>,

    /// The classes of loss the operator declares: `policy`, `capacity`, `invalid`, or `shutdown`.
    pub loss: Vec<String>,

    /// Where the node came from, which is also the authority for its loss.
    pub provenance: Option<Provenance>,

    /// The parameters as written, with the resolved value of every configuration binding.
    pub params: BTreeMap<ParamName, Value>,

    /// Every configuration binding in the parameters, in parameter order.
    pub bindings: Vec<BindingEntry>,

    /// The input ports, in operator order.
    pub inputs: Vec<InputPortEntry>,

    /// The output ports, in operator order.
    pub outputs: Vec<OutputPortEntry>,
}

/// One configuration binding in a node's parameters.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BindingEntry {
    /// Where in the parameters the binding is, such as `predicate.intake_name.values`.
    pub param: String,

    /// The binding.
    pub binding: BindingName,

    /// The binding's resolved value, if the plan carries one.
    pub resolved: Option<Value>,
}

/// One input port of a node.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct InputPortEntry {
    /// The port's name.
    pub name: PortName,

    /// What the port accepts.
    pub accepts: PortType,

    /// The output port its edge leaves, if it has one.
    pub from: Option<PortRef>,
}

/// One output port of a node.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct OutputPortEntry {
    /// The port's name.
    pub name: PortName,

    /// What the port can carry, if validation could compute it.
    pub port_type: Option<PortType>,

    /// The input port its edge enters, if it has one.
    pub to: Option<PortRef>,
}

/// One edge.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EdgeEntry {
    /// The output port the edge leaves.
    pub from: PortRef,

    /// The input port the edge enters.
    pub to: PortRef,

    /// What the edge can carry: the type of its output port, if validation could compute it.
    pub port_type: Option<PortType>,
}

/// One declared output.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct OutputEntry {
    /// The output's ID.
    pub id: OutputId,

    /// What the output accepts.
    pub accepts: PortType,

    /// The fingerprint of the output's binding.
    pub fingerprint: PlanHash,

    /// The physical recipients the output delivers to.
    pub recipients: Vec<RecipientId>,

    /// The output's delivery policy.
    pub delivery: DeliveryPolicy,

    /// The `emit` nodes that deliver to the output, in plan order.
    pub emitted_by: Vec<NodeId>,
}

/// One replaceable scope.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ScopeEntry {
    /// The scope's name.
    pub name: ScopeName,

    /// The scope's inputs, each with the port that feeds it.
    pub inputs: BTreeMap<InputName, PortRef>,

    /// The scope's exits, each with the port outside the scope that it feeds: where a scope that ends before its paths
    /// do leaves off.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub exits: BTreeMap<ExitName, PortRef>,
}

/// What a bound covers.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundScopeEntry {
    /// The whole plan.
    Plan,

    /// The nodes of one artifact revision.
    Artifact(ArtifactRevision),
}

/// One resource bound, against its limit.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BoundEntry {
    /// The limit's name, as the limits table lists it.
    pub limit: String,

    /// What the bound covers.
    pub scope: BoundScopeEntry,

    /// The measured value, or `None` if it is unbounded.
    pub value: Option<u64>,

    /// The limit's value.
    pub maximum: u64,

    /// Whether the limit is enforced on this scope: only artifacts' bounds are.
    pub enforced: bool,

    /// Whether the value is bounded and at most the limit.
    pub within: bool,
}

/// One validation finding.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FindingEntry {
    /// `error` or `warning`.
    pub severity: String,

    /// The rule's stable name.
    pub rule: String,

    /// Where the finding is, such as ``node `dsd_agg` ``.
    pub location: String,

    /// What is wrong, and how to fix it.
    pub message: String,
}

/// One rewrite the compiler considered: applied, or skipped with the reason.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RewriteEntry {
    /// The rewrite, as the specification names it, such as `R5a`.
    pub id: String,

    /// `applied` or `skipped`.
    pub outcome: String,

    /// Why it was skipped, if it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,

    /// The nodes it changes, removes, or inserts.
    pub nodes: Vec<NodeId>,

    /// Why it is sound: the proof obligations it checked, and how each holds.
    pub justification: String,

    /// Whether its equivalence relies on the capacity rule: equal outputs without capacity pressure, and under
    /// pressure only less capacity loss.
    pub capacity_rule: bool,

    /// What it changes in internal telemetry.
    pub telemetry: Vec<TelemetryEffectEntry>,
}

/// One change a rewrite makes to internal telemetry.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TelemetryEffectEntry {
    /// The node whose series change.
    pub node: NodeId,

    /// `removed`, `added`, or `changed`.
    pub change: String,

    /// What changes, and why.
    pub detail: String,
}

impl RewriteEntry {
    fn of(record: &RewriteRecord) -> Self {
        let (outcome, reason) = match &record.outcome {
            RewriteOutcome::Applied => ("applied", None),
            RewriteOutcome::Skipped { reason } => ("skipped", Some(reason.clone())),
        };
        Self {
            id: record.id.as_str().to_string(),
            outcome: outcome.to_string(),
            reason,
            nodes: record.nodes.clone(),
            justification: record.justification.clone(),
            capacity_rule: record.capacity_rule,
            telemetry: record
                .telemetry
                .iter()
                .map(|effect| TelemetryEffectEntry {
                    node: effect.node.clone(),
                    change: effect.change.as_str().to_string(),
                    detail: effect.detail.clone(),
                })
                .collect(),
        }
    }
}

impl Explain {
    /// Describes a plan, using the operator registry it was validated against and its validation report.
    pub fn of(plan: &Plan, registry: &OperatorRegistry, report: &ValidationReport) -> Self {
        let mut targets = BTreeMap::new();
        let mut sources = BTreeMap::new();
        for edge in &plan.edges {
            targets.entry(edge.from.clone()).or_insert_with(|| edge.to.clone());
            sources.entry(edge.to.clone()).or_insert_with(|| edge.from.clone());
        }

        let nodes = plan
            .nodes
            .iter()
            .map(|node| {
                let op = registry.configure(node.op.as_str(), &node.params).ok();
                let port = |name: &PortName| PortRef::new(node.id.clone(), name.clone());
                let inputs = op.as_ref().map_or_else(Vec::new, |op| {
                    op.ports()
                        .inputs
                        .iter()
                        .map(|input| InputPortEntry {
                            name: input.name.clone(),
                            accepts: input.accepts,
                            from: sources.get(&port(&input.name)).cloned(),
                        })
                        .collect()
                });
                let outputs = op.as_ref().map_or_else(Vec::new, |op| {
                    op.ports()
                        .outputs
                        .iter()
                        .map(|output| OutputPortEntry {
                            name: output.name.clone(),
                            port_type: report.port_type(&port(&output.name)),
                            to: targets.get(&port(&output.name)).cloned(),
                        })
                        .collect()
                });
                let bindings = op.as_ref().map_or_else(Vec::new, |op| {
                    binding_uses(op)
                        .into_iter()
                        .map(|found| BindingEntry {
                            param: found.path,
                            binding: found.binding.clone(),
                            resolved: found.resolved.as_ref().map(sorted),
                        })
                        .collect()
                });
                let contract = op.as_ref().map(|op| op.contract());
                NodeEntry {
                    id: node.id.clone(),
                    op: node.op.clone(),
                    boundary: contract.map(|contract| boundary_name(contract.boundary).to_string()),
                    state: contract.map(|contract| state_name(&contract.state).to_string()),
                    loss: contract.map_or_else(Vec::new, |contract| match &contract.loss {
                        Loss::Lossless => Vec::new(),
                        Loss::Lossy(declared) => declared
                            .iter()
                            .map(|loss| loss_class_name(loss.class).to_string())
                            .collect(),
                    }),
                    provenance: node.provenance.clone(),
                    params: node
                        .params
                        .iter()
                        .map(|(name, value)| (name.clone(), sorted(value)))
                        .collect(),
                    bindings,
                    inputs,
                    outputs,
                }
            })
            .collect();

        let edges = plan
            .edges
            .iter()
            .map(|edge| EdgeEntry {
                from: edge.from.clone(),
                to: edge.to.clone(),
                port_type: report.port_type(&edge.from),
            })
            .collect();

        let outputs = plan
            .outputs
            .iter()
            .map(|output| OutputEntry {
                id: output.id.clone(),
                accepts: output.accepts,
                fingerprint: output.fingerprint,
                recipients: output.recipients.clone(),
                delivery: output.delivery,
                emitted_by: plan
                    .nodes
                    .iter()
                    .filter(|node| {
                        node.op.as_str() == crate::operators::names::EMIT
                            && node.params.get("output").and_then(Value::as_str) == Some(output.id.as_str())
                    })
                    .map(|node| node.id.clone())
                    .collect(),
            })
            .collect();

        let scopes = plan
            .scopes
            .iter()
            .map(|(name, scope)| ScopeEntry {
                name: name.clone(),
                inputs: scope.inputs.clone(),
                exits: scope.exits.clone(),
            })
            .collect();

        let bounds = report
            .bounds()
            .iter()
            .map(|bound| BoundEntry {
                limit: bound.limit.to_string(),
                scope: match &bound.scope {
                    BoundScope::Plan => BoundScopeEntry::Plan,
                    BoundScope::Artifact(revision) => BoundScopeEntry::Artifact(revision.clone()),
                },
                value: bound.value,
                maximum: bound.maximum,
                enforced: bound.is_enforced(),
                within: bound.is_within(),
            })
            .collect();

        let findings = report
            .findings()
            .iter()
            .map(|finding| FindingEntry {
                severity: match finding.severity {
                    Severity::Error => "error",
                    Severity::Warning => "warning",
                }
                .to_string(),
                rule: finding.rule.as_str().to_string(),
                location: finding.location.to_string(),
                message: finding.message.clone(),
            })
            .collect();

        Self {
            explain_version: EXPLAIN_VERSION,
            plan: PlanSummary {
                ir_version: plan.ir_version,
                hash: plan.hash(ResolvedValues::Include).ok(),
                structural_hash: plan.hash(ResolvedValues::Exclude).ok(),
                provenance: plan.provenance.clone(),
                valid: report.is_valid(),
            },
            graph: LogicalGraph {
                nodes,
                edges,
                outputs,
                scopes,
            },
            bounds,
            findings,
            rewrites: None,
            physical: None,
        }
    }

    /// Adds the rewrites section: every rewrite the compiler considered for the plan, applied or skipped, in the order
    /// it considered them, as [`CompiledPlan::rewrites`][crate::compile::CompiledPlan::rewrites] lists them.
    pub fn with_rewrites(mut self, records: &[RewriteRecord]) -> Self {
        self.rewrites = Some(records.iter().map(RewriteEntry::of).collect());
        self
    }

    /// Adds the physical sections: how `plan`, the plan this document describes, runs in `frame`, as `placement`
    /// placed it.
    pub fn with_physical(
        mut self, plan: &Plan, frame: &crate::compile::Frame, placement: &crate::compile::Placement,
    ) -> Self {
        self.physical = Some(PhysicalPlan::of(plan, frame, placement));
        self
    }

    /// Returns the document as pretty-printed JSON, with a trailing newline.
    ///
    /// The output is deterministic: members appear in the order the types declare them, maps and the members of
    /// parameter values in sorted order, and lists in plan order.
    ///
    /// # Errors
    ///
    /// Returns an error if the document cannot be serialized, which no document built by [`Explain::of`] causes.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self).map(|json| json + "\n")
    }

    /// Reads a document from JSON, ignoring members this version does not know.
    ///
    /// # Errors
    ///
    /// Returns an error if the text is not an explain document.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Returns the document as human-readable text, with a trailing newline. The output is deterministic.
    pub fn to_text(&self) -> String {
        text::render(self)
    }
}

/// Returns a JSON value with the members of every object in sorted order, whatever order it was parsed in.
///
/// Without the `preserve_order` feature of `serde_json`, which nothing in the workspace enables, every JSON object is
/// already sorted and this returns a copy. It keeps the document's member order independent of that feature, should a
/// dependency enable it.
fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(members) => {
            let mut entries = members.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(name, _)| *name);
            Value::Object(
                entries
                    .into_iter()
                    .map(|(name, value)| (name.clone(), sorted(value)))
                    .collect::<Map<_, _>>(),
            )
        }
        Value::Array(values) => Value::Array(values.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

fn boundary_name(boundary: Boundary) -> &'static str {
    match boundary {
        Boundary::Synchronous => "synchronous",
        Boundary::IoHost => "io_host",
        Boundary::OpaqueHost => "opaque_host",
        Boundary::Isolated => "isolated",
    }
}

fn state_name(state: &StateContract) -> &'static str {
    match state {
        StateContract::None => "none",
        StateContract::Keyed(_) => "keyed",
        StateContract::Opaque => "opaque",
    }
}

pub(crate) fn loss_class_name(class: LossClass) -> &'static str {
    match class {
        LossClass::Policy => "policy",
        LossClass::Capacity => "capacity",
        LossClass::Invalid => "invalid",
        LossClass::Shutdown => "shutdown",
    }
}

#[cfg(test)]
mod tests;
