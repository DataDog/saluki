//! What ADP will do with an artifact, computed before it is published: composition into ADP's default program,
//! validation of the composed plan, and the plan diff against the active plan.
//!
//! This is what ADP's installer computes when the artifact arrives (`saluki_metric_plan::compose::receive_artifact`,
//! `compose`, and `install::diff_operations`, which compares what plans do, without what does nothing as configured),
//! run against what ADP serves: the default program it last lowered (`GET /programmable-metrics/default-program`), the
//! catalog of its export, and its active plan (`GET /programmable-metrics/plan`). So the editor shows ADP's own
//! verdict, each finding placed in the source through the artifact node each plan node came from
//! ([`Composed::ids`](saluki_metric_plan::compose::Composed::ids)), and the diff publishing would install. ADP still
//! decides: it composes the artifact again when it arrives, against the default program and catalog of that moment, and compiles the plan into its regions, which can refuse a plan that validates: a new output, or a state
//! owner the frame cannot run where it is placed, such as the aggregation moved after enrichment. The preview does
//! not place the plan.

use std::collections::BTreeMap;

use saluki_metric_plan::{
    canonical::PlanHash,
    compose::{compose, demanded_outputs, Approval, ComposeError, ComposeOutcome},
    elaborate::elaborate_default,
    export::eliminate,
    install::{
        diff::{NodeChange, OutputChange},
        diff_operations,
    },
    ir::{
        ArtifactNodeId, ArtifactPortRef, NodeId, OutputBinding, OutputDecl, OutputId, Plan, PortRef, ReceivedArtifact,
        RecipientName, Selector,
    },
    normalize::normalize_artifact_with_renames,
    operators::{names, ConfigureError, OperatorRegistry, StateContract},
    validate::{Catalog as _, CatalogOutput, Finding, Location, Rule, Severity as ValidationSeverity},
};
use saluki_metric_plan_syntax::{Parsed, Span};
use serde::Serialize;

use crate::{
    checks::Severity,
    compile::{LineIndex, Origin, Range, RecipientLabels},
    export::ExportBundle,
};

/// The active plan an artifact is diffed against.
#[derive(Clone, Copy, Debug)]
pub struct ActivePlan<'a> {
    /// The plan, as ADP serves it.
    pub plan: &'a Plan,

    /// Its hash, with resolved values.
    pub hash: PlanHash,

    /// The generation running it, if ADP reports it.
    pub generation: Option<u64>,

    /// The revision of the artifact in effect, or `None` when the default program runs.
    pub artifact_revision: Option<u64>,
}

/// What a source is compiled against.
#[derive(Clone, Copy, Debug)]
pub struct CompileContext<'a> {
    /// ADP's export bundle, with the catalog.
    pub export: &'a ExportBundle,

    /// The default program the export was taken from, which artifacts are composed into. Without it, only the
    /// structural checks run.
    pub default_program: Option<&'a Plan>,

    /// The active plan, which the plan diff compares against. Without it, the diff compares against the default
    /// program.
    pub active: Option<ActivePlan<'a>>,

    /// The revision a publish would give the artifact, which composition records in the plan's provenance.
    pub revision: u64,

    /// The labels the demo gives endpoints, which the editor's comments write beside recipients named by URL.
    pub labels: &'a RecipientLabels,

    /// The hash of the artifact ADP runs, if one runs: compiling that artifact again composes it as ADP does, as the
    /// artifact in effect, which delivers to the recipients that remain when some are gone.
    pub in_effect: Option<PlanHash>,
}

/// What the plan diff compares against.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseKind {
    /// The active plan.
    Active,

    /// The default program, because the active plan was not available.
    DefaultProgram,
}

/// The plan the diff compares against.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PreviewBase {
    /// What it is.
    pub kind: BaseKind,

    /// Its hash, with resolved values.
    pub plan_hash: String,

    /// The generation running it, if it is the active plan and ADP reports it.
    pub generation: Option<u64>,

    /// The revision of the artifact it runs, if it is the active plan and runs an artifact.
    pub artifact_revision: Option<u64>,
}

/// How a node differs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// Only the composed plan has it.
    Added,

    /// Only the plan diffed against has it.
    Removed,

    /// Both have it, and it differs.
    Changed,
}

/// One node the composed plan changes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NodeChangeView {
    /// The node's plan ID.
    pub node: String,

    /// The artifact node it came from, if it came from the artifact.
    pub artifact_node: Option<String>,

    /// Its operator: the composed plan's, or the removed node's.
    pub op: String,

    /// How it differs.
    pub change: ChangeKind,

    /// What differs, for a changed node: `operator`, `params`, `edges`.
    pub aspects: Vec<&'static str>,

    /// Where it is in the source, if it came from the artifact.
    pub range: Option<Range>,
}

/// One output the composed plan declares differently.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OutputChangeView {
    /// The output.
    pub output: String,

    /// `rebound`, `added`, or `removed`.
    pub change: &'static str,

    /// A note, for an output the editor stood in for: ADP adds it when the program arrives (an implied output).
    pub note: Option<String>,
}

/// The plan diff preview of an artifact.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct Preview {
    /// Whether the artifact was composed, and the diff computed.
    pub available: bool,

    /// What the preview is, or why there is none.
    pub reason: String,

    /// What the diff compares against.
    pub against: Option<PreviewBase>,

    /// The composed plan's hash, with resolved values: the active plan hash ADP reports once it installs the artifact.
    pub plan_hash: Option<String>,

    /// Whether the composed plan validates.
    pub valid: bool,

    /// Whether the composed plan is the plan diffed against: installing it changes nothing.
    pub unchanged: bool,

    /// Notes about the composed plan that are no findings, such as a native output the program leaves unused.
    pub notes: Vec<String>,

    /// Whether the preview stands in for outputs ADP adds when the program arrives (implied outputs): its findings and
    /// diff are ADP's but for those outputs' settings, and its plan hash is not the one ADP installs.
    pub provisional: bool,

    /// Every node that differs, the composed plan's in its order, then the removed ones.
    pub nodes: Vec<NodeChangeView>,

    /// The composed plan's edges the plan diffed against does not have, as `node.port -> node.port`.
    pub edges_added: Vec<String>,

    /// The edges of the plan diffed against that the composed plan does not have.
    pub edges_removed: Vec<String>,

    /// The outputs declared differently.
    pub outputs: Vec<OutputChangeView>,
}

impl Preview {
    /// Returns a preview that was not computed, for the reason given.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            ..Self::default()
        }
    }
}

/// One finding of composition or validation, placed in the source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlacedFinding {
    /// How serious it is.
    pub severity: Severity,

    /// Composition or validation.
    pub origin: Origin,

    /// The validation rule, for a validation finding.
    pub rule: Option<&'static str>,

    /// What is wrong.
    pub message: String,

    /// Where in the source, when it can be placed.
    pub span: Option<Span>,
}

/// Composes a received artifact into the default program, validates the composed plan, and diffs it against the active plan (or, without one, the default program), as ADP will when the
/// artifact arrives as revision `revision`.
///
/// Returns the findings and the preview. A composition error is a single finding and no diff: ADP rejects the artifact
/// without composing a plan.
pub fn compose_preview(
    parsed: &Parsed, received: &ReceivedArtifact, context: &CompileContext<'_>, default_program: &Plan,
    index: &LineIndex<'_>,
) -> (Vec<PlacedFinding>, Preview) {
    let registry = OperatorRegistry::native();
    let export = context.export;
    let approval = Approval {
        revision: context.revision,
        // Approval binds to the active plan at compile time, as publishing records it.
        approved_against: context.active.map(|active| active.hash),
        active_plan_hash: context.active.map(|active| active.hash),
        // A new artifact: an output it records that changed refuses it, and it waits for recipients it names that are
        // gone. The artifact ADP runs is composed as ADP composes it again, in effect.
        in_effect: context.in_effect == Some(received.hash),
    };
    // Outputs ADP adds for the recipient sets the program emits to that no output delivers to exactly: the editor
    // composes and validates against stand-ins for them, so that it finds what ADP will before publishing.
    let standing_in = stand_ins(received, export, default_program);
    let catalog = standing_in.iter().fold(export.catalog.clone(), |catalog, stand_in| {
        catalog.with_output(stand_in.decl.id.clone(), stand_in.catalog_output())
    });
    let mut with_stand_ins;
    // The default program as ADP serves it, which a diff without an active plan compares with.
    let served_default_program = default_program;
    let default_program = if standing_in.is_empty() {
        default_program
    } else {
        with_stand_ins = default_program.clone();
        with_stand_ins
            .outputs
            .extend(standing_in.iter().map(|stand_in| stand_in.decl.clone()));
        &with_stand_ins
    };
    let written = WrittenIds::of(parsed);
    let mut adds = standing_in
        .iter()
        .map(|stand_in| PlacedFinding {
            severity: Severity::Warning,
            origin: Origin::Composition,
            rule: None,
            message: format!(
                "{} {}: none delivers to exactly these recipients yet. It is checked here against a stand-in, `{}`; \
                 the program waits for ADP's output a moment before it installs.",
                IMPLIED_OUTPUT_NOTE,
                describe_recipients(&stand_in.names, export, context.labels),
                stand_in.decl.id
            ),
            span: written
                .param(parsed, stand_in.emit.as_str(), "to")
                .or_else(|| written.node(parsed, stand_in.emit.as_str())),
        })
        .collect::<Vec<_>>();
    let composed = match compose(default_program, received, &approval, &catalog, &registry) {
        Ok(composed) => composed,
        Err(error) => {
            // A program that waits is not refused: ADP holds it pending, and installs it once what it waits for exists.
            let (severity, reason) = match &error {
                ComposeError::PendingRecipients { .. } => (
                    Severity::Warning,
                    "No plan diff yet: the program emits to recipients no output delivers to exactly. When ADP \
                     receives it, ADP adds an output for them (an implied output), and the program waits for it, \
                     pending, for a moment before it installs. Compile again once ADP runs it to see the diff.",
                ),
                ComposeError::UnknownRecipient { .. } => (
                    Severity::Warning,
                    "No plan diff: the program emits to a recipient ADP's directory does not name. ADP holds it \
                     pending until its configuration declares the recipient, and keeps running the active plan.",
                ),
                _ => (
                    Severity::Error,
                    "The artifact cannot be composed into ADP's default program, so ADP rejects it and keeps running \
                     the active plan.",
                ),
            };
            let finding = PlacedFinding {
                severity,
                origin: Origin::Composition,
                rule: None,
                message: waiting_message(&error, export, context.labels).unwrap_or_else(|| error.to_string()),
                span: compose_error_span(&error, parsed),
            };
            adds.push(finding);
            return (adds, Preview::unavailable(reason));
        }
    };

    // The artifact node each plan node came from places findings and diff nodes in the source.
    let artifact_nodes = composed
        .ids
        .iter()
        .map(|(artifact_node, node)| (node.clone(), artifact_node.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut findings = adds;
    // The artifact ADP runs, composed again in effect, may run degraded: its emits to recipients that are gone dropped.
    if let ComposeOutcome::Degraded(dropped) = &composed.outcome {
        for dropped in dropped {
            findings.push(PlacedFinding {
                severity: Severity::Warning,
                origin: Origin::Composition,
                rule: None,
                message: format!("ADP runs this program degraded: {}.", dropped),
                span: None,
            });
        }
    }
    // The recipients the composed plan delivers to, through any output an `emit` names.
    let delivered = composed
        .plan
        .nodes
        .iter()
        .filter(|node| node.op.as_str() == names::EMIT)
        .filter_map(|node| node.params.get("output")?.as_str())
        .filter_map(|output| {
            composed
                .plan
                .outputs
                .iter()
                .find(|declared| declared.id.as_str() == output)
        })
        .flat_map(|output| output.recipients.iter().cloned())
        .collect::<std::collections::BTreeSet<_>>();
    let mut notes = Vec::new();
    for finding in composed.report.findings() {
        // A native output the program leaves unused while it delivers to the output's recipients through others, as
        // it does through implied outputs, is no finding: a note says so.
        if let (Rule::UnusedOutput, Location::Output(output)) = (finding.rule, &finding.location) {
            let replaced = composed
                .plan
                .outputs
                .iter()
                .find(|declared| &declared.id == output)
                .is_some_and(|declared| {
                    !declared.recipients.is_empty() && declared.recipients.iter().all(|id| delivered.contains(id))
                });
            if replaced {
                notes.push(format!(
                    "The native output `{}` is unused: the program delivers to its recipients through other outputs.",
                    output
                ));
                continue;
            }
        }
        let span = finding_span(
            finding,
            &composed.plan,
            &composed.ids,
            &artifact_nodes,
            parsed,
            &registry,
        );
        let message = if span.is_some() {
            finding.message.clone()
        } else {
            format!("{}: {}", finding.location, finding.message)
        };
        findings.push(PlacedFinding {
            severity: match finding.severity {
                ValidationSeverity::Error => Severity::Error,
                ValidationSeverity::Warning => Severity::Warning,
            },
            origin: Origin::Validation,
            rule: Some(finding.rule.as_str()),
            message,
            span,
        });
    }

    let default_core;
    let (base, against) = match context.active {
        Some(active) => (
            active.plan,
            PreviewBase {
                kind: BaseKind::Active,
                plan_hash: active.hash.to_string(),
                generation: active.generation,
                artifact_revision: active.artifact_revision,
            },
        ),
        None => {
            // The default program as ADP installs it: its core plan, which the composed plan compares with.
            default_core = elaborate_default(served_default_program, &export.catalog, &registry).plan;
            (
                &default_core,
                PreviewBase {
                    kind: BaseKind::DefaultProgram,
                    plan_hash: default_core.hash().map(|hash| hash.to_string()).unwrap_or_default(),
                    generation: None,
                    artifact_revision: None,
                },
            )
        }
    };
    // By what the plans do: what does nothing as configured, which the detached export leaves out and the default
    // program runs, is no change in what the program does. ADP compares core plans by hash, so dropping those nodes is
    // a generation of its own, which builds no kernel for them: a note says so.
    let diff = diff_operations(base, &composed.plan, &registry);
    let dropped = eliminate(base, &registry)
        .omitted
        .into_iter()
        .filter(|(node, _)| composed.plan.node(node).is_none())
        .count();
    if dropped > 0 {
        notes.push(format!(
            "ADP will drop {} {} that {} nothing as configured, which the program leaves out; that builds no kernel.",
            dropped,
            if dropped == 1 { "node" } else { "nodes" },
            if dropped == 1 { "does" } else { "do" }
        ));
    }
    // A state owner the edit names differently from the plan in effect starts over with empty state: a note says so,
    // and how to keep the ID.
    notes.extend(renamed_state_owners(base, &composed.plan, &registry, |node| {
        artifact_nodes.get(node).map(ToString::to_string)
    }));
    let written = WrittenIds::of(parsed);
    let nodes = diff
        .changes()
        .filter_map(|change| {
            let candidate = composed.plan.node(&change.id);
            let artifact_node = artifact_nodes.get(&change.id);
            let (kind, aspects) = match &change.change {
                NodeChange::Unchanged => return None,
                NodeChange::Added => (ChangeKind::Added, Vec::new()),
                NodeChange::Removed => (ChangeKind::Removed, Vec::new()),
                NodeChange::Changed {
                    operator,
                    params,
                    edges,
                } => {
                    let aspects = [(*operator, "operator"), (*params, "params"), (*edges, "edges")]
                        .into_iter()
                        .filter(|(differs, _)| *differs)
                        .map(|(_, aspect)| aspect)
                        .collect();
                    (ChangeKind::Changed, aspects)
                }
            };
            let op = candidate
                .or_else(|| base.node(&change.id))
                .map(|node| node.op.to_string())
                .unwrap_or_default();
            Some(NodeChangeView {
                node: change.id.to_string(),
                artifact_node: artifact_node.map(ToString::to_string),
                op,
                change: kind,
                aspects,
                range: artifact_node
                    .and_then(|node| written.node(parsed, node.as_str()))
                    .map(|span| index.range(Some(span))),
            })
        })
        .collect();
    let edge = |from: &PortRef, to: &PortRef| format!("{} -> {}", from, to);
    let provisional = if standing_in.is_empty() {
        String::new()
    } else {
        format!(
            " With stand-ins for the {} ADP adds when the program arrives, which ADP builds with its own settings: the \
             plan hash ADP installs differs, and compiling again once it runs shows the plan exactly.",
            if standing_in.len() == 1 { "output" } else { "outputs" }
        )
    };
    let preview = Preview {
        available: true,
        notes,
        provisional: !standing_in.is_empty(),
        reason: match against.kind {
            BaseKind::Active => format!(
                "Composed into ADP's default program and validated as ADP does when the artifact arrives; the diff \
                 compares the composed plan with the active plan. The plan is not placed into ADP's regions here.{}",
                provisional
            ),
            BaseKind::DefaultProgram => format!(
                "Composed into ADP's default program and validated as ADP does when the artifact arrives; ADP's \
                 active plan was not available, so the diff compares the composed plan with the default program. The \
                 plan is not placed into ADP's regions here.{}",
                provisional
            ),
        },
        plan_hash: composed.plan.hash().ok().map(|hash| hash.to_string()),
        valid: composed.report.is_valid(),
        unchanged: diff.is_empty(),
        nodes,
        edges_added: diff.edges_added.iter().map(|e| edge(&e.from, &e.to)).collect(),
        edges_removed: diff.edges_removed.iter().map(|e| edge(&e.from, &e.to)).collect(),
        outputs: diff
            .outputs
            .iter()
            .filter_map(|output| {
                let change = match output.change {
                    OutputChange::Reused => return None,
                    OutputChange::Rebound => "rebound",
                    OutputChange::Added => "added",
                    OutputChange::Removed => "removed",
                };
                let note = standing_in
                    .iter()
                    .find(|stand_in| stand_in.decl.id == output.id)
                    .map(|stand_in| {
                        format!(
                            "ADP will add it: an output for {}",
                            describe_recipients(&stand_in.names, export, context.labels)
                        )
                    });
                Some(OutputChangeView {
                    output: output.id.to_string(),
                    change,
                    note,
                })
            })
            .collect(),
        against: Some(against),
    };
    (findings, preview)
}

/// Returns a note for each state owner that `after` names differently from `before`: a node with keyed state, such as
/// an aggregation, that `before` has and `after` does not, paired, in plan order, with one of the same operator that
/// `after` has and `before` does not.
///
/// Kernel reuse and state retention match nodes by plan ID, so such a node's state does not carry over. The plans are
/// compared rather than the artifacts, since composition decides the plan IDs: an artifact's one aggregation takes the
/// default program's ID whatever the artifact calls it, on a host that runs one aggregation per program, so renaming
/// it there loses nothing and gets no note. `artifact_node` names a plan node of `after` as the artifact writes it; a
/// node of `before` is named by its local ID, which is the ID its artifact, or the default program, gives it.
pub(crate) fn renamed_state_owners(
    before: &Plan, after: &Plan, registry: &OperatorRegistry, artifact_node: impl Fn(&NodeId) -> Option<String>,
) -> Vec<String> {
    let owns_state = |node: &saluki_metric_plan::ir::Node| {
        registry
            .configure(node.op.as_str(), &node.params)
            .is_ok_and(|configured| matches!(configured.contract().state, StateContract::Keyed(_)))
    };
    // The state owners of `plan` that `other` has no node of the same ID and operator for.
    let unmatched = |plan: &'_ Plan, other: &Plan| {
        plan.nodes
            .iter()
            .filter(|node| owns_state(node))
            .filter(|node| other.node(&node.id).is_none_or(|same| same.op != node.op))
            .map(|node| (node.id.clone(), node.op.clone()))
            .collect::<Vec<_>>()
    };
    let mut added = unmatched(after, before);
    let mut notes = Vec::new();
    for (from, op) in unmatched(before, after) {
        let Some(at) = added.iter().position(|(_, added_op)| *added_op == op) else {
            continue;
        };
        let (to, _) = added.remove(at);
        let from = from.local().to_string();
        let to = artifact_node(&to).unwrap_or_else(|| to.local().to_string());
        let what = if op.as_str() == names::AGGREGATE {
            "the aggregation".to_string()
        } else {
            format!("the `{}` node", op)
        };
        notes.push(format!(
            "This edit renames {} from `{}` to `{}`, so its state will not carry over; keep its ID by naming it with \
             `let {} = ...`.",
            what, from, to, from
        ));
    }
    notes
}

/// How the editor starts the note that ADP adds an output for a set of recipients.
pub const IMPLIED_OUTPUT_NOTE: &str = "ADP will add an output for";

/// An output the editor stands in for: ADP adds one when the program arrives, for a set of recipients its `emit`s
/// name that no output delivers to exactly.
struct StandIn {
    /// The output, as the default program would declare it.
    decl: OutputDecl,

    /// The first artifact node that emits to the set.
    emit: ArtifactNodeId,

    /// The names the program writes for the set's recipients, in the directory's order.
    names: Vec<RecipientName>,
}

impl StandIn {
    fn catalog_output(&self) -> CatalogOutput {
        CatalogOutput {
            accepts: self.decl.accepts,
            recipients: self.decl.recipients.clone(),
        }
    }
}

/// Returns the outputs ADP adds for `received`'s demand ([`demanded_outputs`]) that the export does not declare yet,
/// as stand-ins: each a copy of the default program's output on the set's delivery host with the set's recipients.
///
/// ADP adds implied outputs on the primary endpoint's delivery host only, so a set elsewhere gets no stand-in, and
/// composition says what it waits for.
fn stand_ins(received: &ReceivedArtifact, export: &ExportBundle, default_program: &Plan) -> Vec<StandIn> {
    let Ok(demand) = demanded_outputs(&received.artifact, &export.catalog) else {
        return Vec::new();
    };
    let primary_host = export
        .catalog
        .recipient(&"primary".parse().expect("`primary` is a recipient name"))
        .map(|entry| entry.host.clone());
    demand
        .into_iter()
        .filter(|demand| Some(&demand.host) == primary_host.as_ref())
        .filter(|demand| export.catalog.output_for(&demand.recipients).is_none())
        .enumerate()
        .filter_map(|(index, demand)| {
            // An output on the set's host, or, in a plan that binds none, one that shares a recipient with the set.
            let template = default_program
                .outputs
                .iter()
                .find(|output| {
                    output
                        .binding
                        .as_ref()
                        .is_some_and(|binding| binding.host == demand.host)
                })
                .or_else(|| {
                    default_program
                        .outputs
                        .iter()
                        .find(|output| output.recipients.iter().any(|id| demand.recipients.contains(id)))
                })?;
            let id = format!("adp_will_add_{}", index + 1).parse::<OutputId>().ok()?;
            // As ADP binds an implied output: its selector its recipients.
            let binding = template.binding.clone().map(|binding| OutputBinding {
                selector: Selector::Recipients(demand.recipients.clone()),
                ..binding
            });
            let names = export
                .catalog
                .recipients()
                .filter(|(_, entry)| entry.recipients.iter().any(|id| demand.recipients.contains(id)))
                .map(|(name, _)| name.clone())
                .collect();
            Some(StandIn {
                decl: OutputDecl {
                    id,
                    recipients: demand.recipients.clone(),
                    binding,
                    ..template.clone()
                },
                emit: demand.emit,
                names,
            })
        })
        .collect()
}

/// Writes recipient names for a reader: the demo's label of each one's endpoint, through the export's directory, if it
/// has one, else the name as a program writes it, as `Primary and Team C`.
pub fn describe_recipients(names: &[RecipientName], export: &ExportBundle, labels: &RecipientLabels) -> String {
    let named = names
        .iter()
        .map(|name| {
            export
                .catalog
                .recipient(name)
                .and_then(|entry| labels.of(&entry.endpoint))
                .unwrap_or(name.as_str())
                .to_string()
        })
        .collect::<Vec<_>>();
    match named.as_slice() {
        [] => "no recipient".to_string(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {}", init.join(", "), last),
    }
}

/// Returns what the editor says of a program that waits, for a viewer: the recipients by their labels, without the
/// node IDs composition names.
fn waiting_message(error: &ComposeError, export: &ExportBundle, labels: &RecipientLabels) -> Option<String> {
    match error {
        ComposeError::PendingRecipients { recipients, .. } => Some(format!(
            "ADP will add an output for {}: none delivers to exactly these recipients yet, and the program waits for \
             it before it installs.",
            describe_recipients(recipients, export, labels)
        )),
        ComposeError::UnknownRecipient { recipient, .. } => Some(format!(
            "ADP does not know the recipient `{}`: it holds the program pending until its configuration declares it.",
            recipient
        )),
        _ => None,
    }
}

/// Returns the span of the `output` parameter of the first `emit` of the artifact that delivers to `output`.
fn emit_span(parsed: &Parsed, output: &OutputId) -> Option<Span> {
    let node = parsed.artifact.nodes.iter().find(|node| {
        node.op.as_str() == names::EMIT
            && node
                .params
                .get("output")
                .and_then(|value| value.as_str())
                .is_some_and(|emitted| emitted == output.as_str())
    })?;
    parsed
        .spans
        .param(node.id.as_str(), "output")
        .or_else(|| parsed.spans.node(node.id.as_str()))
}

/// Places a composition error in the source: at the node or edge it is about.
fn compose_error_span(error: &ComposeError, parsed: &Parsed) -> Option<Span> {
    let written = WrittenIds::of(parsed);
    match error {
        ComposeError::ReservedSource { node, .. } | ComposeError::UnresolvedBinding { node, .. } => {
            written.node(parsed, node.as_str())
        }
        ComposeError::UnknownRecipient { node, .. }
        | ComposeError::PendingRecipients { node, .. }
        | ComposeError::NoOutputFor { node, .. }
        | ComposeError::RecipientsOnTwoHosts { node, .. } => written
            .param(parsed, node.as_str(), "to")
            .or_else(|| written.node(parsed, node.as_str())),
        ComposeError::UndeclaredOutput { node, .. } => written
            .param(parsed, node.as_str(), "output")
            .or_else(|| written.node(parsed, node.as_str())),
        // The first edge that reads the source.
        ComposeError::UnknownSource { name, .. } => parsed
            .artifact
            .edges
            .iter()
            .position(|edge| matches!(&edge.from, ArtifactPortRef::Input(input) if input == name))
            .and_then(|index| parsed.spans.edge(index)),
        ComposeError::DefaultProgram { .. } | ComposeError::Malformed { .. } => None,
    }
}

/// The IDs an edited program's nodes have as written, by the IDs normalization gives them.
///
/// Composition normalizes an artifact, so it, and the plans and findings it makes, name a terminal whose port
/// normalization moved (a `split` that drops on `true` becomes one that drops on `false`) by its new ID, while the span
/// map knows the node by the ID the author wrote.
struct WrittenIds(BTreeMap<String, String>);

impl WrittenIds {
    fn of(parsed: &Parsed) -> Self {
        let (_, renames) = normalize_artifact_with_renames(&parsed.artifact);
        Self(
            renames
                .into_iter()
                .map(|(written, normal)| (normal.to_string(), written.to_string()))
                .collect(),
        )
    }

    /// Returns the ID the author wrote for the node normalization calls `id`.
    fn get<'a>(&'a self, id: &'a str) -> &'a str {
        self.0.get(id).map_or(id, String::as_str)
    }

    /// Returns the span of the node normalization calls `id`.
    fn node(&self, parsed: &Parsed, id: &str) -> Option<Span> {
        parsed.spans.node(self.get(id))
    }

    /// Returns the span of the parameter at `path` of the node normalization calls `id`.
    fn param(&self, parsed: &Parsed, id: &str, path: &str) -> Option<Span> {
        parsed.spans.param(self.get(id), path)
    }
}

/// Places a validation finding of the composed plan in the source, through the plan node each artifact node became
/// (`ids`) and its inverse (`artifact_nodes`). A finding at a node the artifact does not have (a source), or at the
/// plan as a whole, is not placed.
fn finding_span(
    finding: &Finding, plan: &Plan, ids: &BTreeMap<ArtifactNodeId, NodeId>,
    artifact_nodes: &BTreeMap<NodeId, ArtifactNodeId>, parsed: &Parsed, registry: &OperatorRegistry,
) -> Option<Span> {
    let written = WrittenIds::of(parsed);
    let node_span = |node: &NodeId| {
        artifact_nodes
            .get(node)
            .and_then(|artifact_node| written.node(parsed, artifact_node.as_str()))
    };
    match &finding.location {
        Location::Node { id, .. } => {
            let artifact_node = written.get(artifact_nodes.get(id)?.as_str());
            // A parameter the operator refuses is placed at the parameter, as the operator names it.
            let parameter = (finding.rule == Rule::ValidParameters)
                .then(|| {
                    parsed
                        .artifact
                        .nodes
                        .iter()
                        .find(|node| node.id.as_str() == artifact_node)
                })
                .flatten()
                .and_then(|node| match registry.configure(node.op.as_str(), &node.params) {
                    Err(ConfigureError::InvalidParams { errors, .. }) => errors
                        .first()
                        .and_then(|error| parsed.spans.param(node.id.as_str(), &error.path)),
                    _ => None,
                });
            parameter.or_else(|| parsed.spans.node(artifact_node))
        }
        Location::Edge { index } => {
            let edge = plan.edges.get(*index)?;
            // The artifact edge that entered the same port of the same node, if the node came from the artifact.
            let artifact_edge = parsed
                .artifact
                .edges
                .iter()
                .position(|artifact_edge| match &artifact_edge.to {
                    ArtifactPortRef::Node { node, port } => {
                        port == &edge.to.port && ids.get(node) == Some(&edge.to.node)
                    }
                    ArtifactPortRef::Input(_) => false,
                });
            artifact_edge
                .and_then(|index| parsed.spans.edge(index))
                .or_else(|| node_span(&edge.to.node))
                .or_else(|| node_span(&edge.from.node))
        }
        Location::Output(output) => emit_span(parsed, output),
        // The plan as a whole.
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests;
