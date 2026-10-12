//! What ADP will do with an artifact, computed before it is published: composition into ADP's default program,
//! validation of the composed plan, and the plan diff against the active plan.
//!
//! This is what ADP's installer computes when the artifact arrives (`saluki_metric_plan::compose::receive_artifact`,
//! `compose_all`, and `install::diff_with_correspondence`), run against what ADP serves: the default program it last
//! lowered (`GET /programmable-metrics/default-program`), the catalog of the export of the artifact's scope, the
//! artifacts it runs for the other scopes (as its status names them, read from the configs the stub serves), and its
//! active plan (`GET /programmable-metrics/plan`). So the editor shows ADP's own verdict, each finding placed in the
//! source through the correspondence of plan nodes to artifact nodes, and the diff publishing would install. ADP still
//! decides: it composes the artifact again when it arrives, against the default program, catalog, and artifacts of that
//! moment, and compiles the plan into its regions, which can refuse a plan that validates: a new output, or a state
//! owner the frame cannot run where it is placed, such as an aggregation in `post_enrich`. The preview does not place
//! the plan.

use saluki_metric_plan::{
    canonical::{PlanHash, ResolvedValues},
    compose::{compose_all, receive_artifact, Approval, ComposeError},
    install::{
        diff::{NodeChange, OutputChange},
        diff_with_correspondence, Correspondence,
    },
    ir::{OutputId, Plan, PortRef, Provenance, ReceivedArtifact},
    operators::{names, ConfigureError, OperatorRegistry},
    validate::{Finding, Location, Rule, Severity as ValidationSeverity},
};
use saluki_metric_plan_syntax::{Parsed, Span};
use serde::Serialize;

use crate::{
    checks::Severity,
    compile::{LineIndex, Origin, Range},
    export::ExportBundle,
    stub::protocol::{ChannelStatus, Envelope},
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

/// An artifact ADP runs for another scope than the edited artifact's, which ADP composes the edited artifact with.
#[derive(Clone, Debug)]
pub struct ScopeArtifact {
    /// The artifact, as ADP received it.
    pub artifact: ReceivedArtifact,

    /// The revision ADP runs it under, which composition records in its nodes' provenance.
    pub revision: u64,
}

/// Returns the artifacts ADP runs for the scopes other than `scope`: each artifact its status reports active in another
/// scope, read from the envelope the stub serves for that scope (`served`, by scope), if the two have the same artifact
/// hash.
///
/// An artifact ADP runs that the stub no longer serves (one it revoked or replaced, whose generation still runs) is
/// left out: ADP no longer composes it either.
pub fn other_scope_artifacts(status: &ChannelStatus, served: &[(String, Envelope)], scope: &str) -> Vec<ScopeArtifact> {
    status
        .active_artifacts()
        .into_iter()
        .filter(|(other, _)| other != scope)
        .filter_map(|(other, active)| {
            let (_, envelope) = served.iter().find(|(served, envelope)| {
                *served == other && active.artifact_hash.as_deref() == Some(envelope.artifact_hash.as_str())
            })?;
            let artifact = receive_artifact(envelope.artifact.clone()).ok()?;
            Some(ScopeArtifact {
                artifact,
                revision: active.revision.unwrap_or(envelope.revision),
            })
        })
        .collect()
}

/// What a source is compiled against.
#[derive(Clone, Copy, Debug)]
pub struct CompileContext<'a> {
    /// ADP's export bundle of the scope the edited artifact replaces, with the catalog.
    pub export: &'a ExportBundle,

    /// The default program the export was taken from, which artifacts are composed into. Without it, only the
    /// structural checks run.
    pub default_program: Option<&'a Plan>,

    /// The active plan, which the plan diff compares against. Without it, the diff compares against the default
    /// program.
    pub active: Option<ActivePlan<'a>>,

    /// The artifacts ADP runs for the other scopes, which ADP composes the edited artifact with (see
    /// [`other_scope_artifacts`]). One for the edited artifact's own scope is ignored: the edited artifact replaces it.
    pub others: &'a [ScopeArtifact],

    /// The revision a publish would give the artifact, which composition records in its nodes' provenance.
    pub revision: u64,
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

    /// What differs, for a changed node: `operator`, `params`, `provenance`, `edges`.
    pub aspects: Vec<&'static str>,

    /// Its provenance in the composed plan: `native`, or `revision <n>`; `None` for a removed node.
    pub provenance: Option<String>,

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

    /// Whether the artifact's base is not the current default program.
    pub stale_base: bool,

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

/// Composes a received artifact into the default program, with the artifacts ADP runs for the other scopes, validates
/// the composed plan, and diffs it against the active plan (or, without one, the default program), as ADP will when the
/// artifact arrives as revision `revision`.
///
/// Returns the findings and the preview. A composition error is a single finding and no diff: ADP rejects the artifact
/// without composing a plan.
pub fn compose_preview(
    source: &str, parsed: &Parsed, received: &ReceivedArtifact, context: &CompileContext<'_>, default_program: &Plan,
    index: &LineIndex<'_>,
) -> (Vec<PlacedFinding>, Preview) {
    let registry = OperatorRegistry::native();
    let export = context.export;
    let approval = Approval {
        revision: context.revision,
        // Approval binds to the active plan at compile time, as publishing records it.
        approved_against: context.active.map(|active| active.hash),
        active_plan_hash: context.active.map(|active| active.hash),
        // The previewed artifact is new: an output it records that changed refuses it.
        in_effect: false,
        upgraded: Default::default(),
    };
    let scope = &received.artifact.scope;
    let others = context
        .others
        .iter()
        .filter(|other| &other.artifact.artifact.scope != scope)
        .map(|other| {
            // ADP runs the other scopes' artifacts: their emits to an output that changed are dropped, not refused.
            let approval = Approval {
                revision: other.revision,
                approved_against: None,
                active_plan_hash: None,
                in_effect: true,
                upgraded: Default::default(),
            };
            (&other.artifact, approval)
        })
        .collect::<Vec<_>>();
    let mut artifacts = vec![(received, &approval)];
    artifacts.extend(others.iter().map(|(artifact, approval)| (*artifact, approval)));
    let composed = match compose_all(default_program, &artifacts, &export.catalog, &registry) {
        Ok(composed) => composed,
        Err(error) => {
            let finding = PlacedFinding {
                severity: Severity::Error,
                origin: Origin::Composition,
                rule: None,
                message: error.to_string(),
                span: compose_error_span(&error, parsed, source),
            };
            let preview = Preview::unavailable(
                "The artifact cannot be composed into ADP's default program, so ADP rejects it and keeps running the \
                 active plan.",
            );
            return (vec![finding], preview);
        }
    };

    // The edited artifact's own composition: its correspondence places findings and diff nodes in its source.
    let (correspondence, stale_base) = match composed.scopes.get(scope) {
        Some(own) => (&own.correspondence, own.stale_base),
        None => (&composed.correspondence, composed.stale_base),
    };
    let mut findings = Vec::new();
    if stale_base {
        findings.push(PlacedFinding {
            severity: Severity::Warning,
            origin: Origin::Composition,
            rule: None,
            message: format!(
                "The base {} is not the current default program, {}. ADP installs the artifact but reports it as \
                 stale: the default program changed since this artifact was derived from it. Load the default program \
                 again and reapply your change to rebase it.",
                parsed.artifact.base.program_hash,
                export.program_hash()
            ),
            span: find(source, &parsed.artifact.base.program_hash.to_string()),
        });
    }
    for finding in composed.report.findings() {
        let span = finding_span(finding, &composed.plan, correspondence, parsed, &registry);
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
        None => (
            default_program,
            PreviewBase {
                kind: BaseKind::DefaultProgram,
                plan_hash: default_program
                    .hash(ResolvedValues::Include)
                    .map(|hash| hash.to_string())
                    .unwrap_or_default(),
                generation: None,
                artifact_revision: None,
            },
        ),
    };
    let diff = diff_with_correspondence(base, &composed.plan, correspondence, &registry);
    let nodes = diff
        .diff
        .changes()
        .filter_map(|change| {
            let candidate = composed.plan.node(&change.id);
            let artifact_node = diff.artifact_nodes.get(&change.id);
            let (kind, aspects) = match &change.change {
                NodeChange::Unchanged => return None,
                NodeChange::Added => (ChangeKind::Added, Vec::new()),
                NodeChange::Removed => (ChangeKind::Removed, Vec::new()),
                NodeChange::Changed {
                    operator,
                    params,
                    provenance,
                    edges,
                } => {
                    let aspects = [
                        (*operator, "operator"),
                        (*params, "params"),
                        (*provenance, "provenance"),
                        (*edges, "edges"),
                    ]
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
                provenance: candidate.map(|node| match &node.provenance {
                    Some(Provenance::Native(_)) => "native".to_string(),
                    Some(Provenance::Artifact(revision)) => format!("revision {}", revision.revision),
                    None => "none".to_string(),
                }),
                range: artifact_node
                    .and_then(|node| parsed.spans.node(node.as_str()))
                    .map(|span| index.range(Some(span))),
            })
        })
        .collect();
    let edge = |from: &PortRef, to: &PortRef| format!("{} -> {}", from, to);
    let with_others = match others.as_slice() {
        [] => String::new(),
        others => {
            let scopes = others
                .iter()
                .map(|(artifact, approval)| format!("`{}` (revision {})", artifact.artifact.scope, approval.revision))
                .collect::<Vec<_>>();
            format!(" with the artifacts ADP runs for {}", scopes.join(", "))
        }
    };
    let preview = Preview {
        available: true,
        reason: match against.kind {
            BaseKind::Active => format!(
                "Composed into ADP's default program{} and validated as ADP does when the artifact arrives; the diff \
                 compares the composed plan with the active plan. The plan is not placed into ADP's regions here.",
                with_others
            ),
            BaseKind::DefaultProgram => format!(
                "Composed into ADP's default program{} and validated as ADP does when the artifact arrives; ADP's \
                 active plan was not available, so the diff compares the composed plan with the default program. The \
                 plan is not placed into ADP's regions here.",
                with_others
            ),
        },
        plan_hash: composed
            .plan
            .hash(ResolvedValues::Include)
            .ok()
            .map(|hash| hash.to_string()),
        valid: composed.report.is_valid(),
        unchanged: diff.diff.is_empty(),
        stale_base,
        nodes,
        edges_added: diff.diff.edges_added.iter().map(|e| edge(&e.from, &e.to)).collect(),
        edges_removed: diff.diff.edges_removed.iter().map(|e| edge(&e.from, &e.to)).collect(),
        outputs: diff
            .diff
            .outputs
            .iter()
            .filter_map(|output| {
                let change = match output.change {
                    OutputChange::Reused => return None,
                    OutputChange::Rebound => "rebound",
                    OutputChange::Added => "added",
                    OutputChange::Removed => "removed",
                };
                Some(OutputChangeView {
                    output: output.id.to_string(),
                    change,
                })
            })
            .collect(),
        against: Some(against),
    };
    (findings, preview)
}

/// Returns the span of the first occurrence of `text` in `source`, if any.
fn find(source: &str, text: &str) -> Option<Span> {
    source.find(text).map(|start| Span::new(start, start + text.len()))
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

/// Places a composition error in the source: at the node it names, or the output or scope it is about.
fn compose_error_span(error: &ComposeError, parsed: &Parsed, source: &str) -> Option<Span> {
    match error {
        ComposeError::ReservedOutsideScope { node, .. } | ComposeError::UnresolvedBinding { node, .. } => {
            parsed.spans.node(node.as_str())
        }
        ComposeError::UnrecordedOutput { node, .. } | ComposeError::UndeclaredOutput { node, .. } => parsed
            .spans
            .param(node.as_str(), "output")
            .or_else(|| parsed.spans.node(node.as_str())),
        ComposeError::PendingOutput { output } | ComposeError::ReboundOutput { output, .. } => {
            emit_span(parsed, output)
        }
        ComposeError::UnknownScope { scope } => find(source, scope.as_str()),
        ComposeError::DefaultProgram { .. } | ComposeError::Malformed { .. } => None,
    }
}

/// Places a validation finding of the composed plan in the source, through the artifact node each plan node came
/// from (`correspondence`, the edited artifact's). A finding at a node the artifact does not have (outside its scope),
/// or at the plan or a scope as a whole, is not placed.
fn finding_span(
    finding: &Finding, plan: &Plan, correspondence: &Correspondence, parsed: &Parsed, registry: &OperatorRegistry,
) -> Option<Span> {
    let node_span = |node: &saluki_metric_plan::ir::NodeId| {
        correspondence
            .artifact_node(node)
            .and_then(|artifact_node| parsed.spans.node(artifact_node.as_str()))
    };
    match &finding.location {
        Location::Node { id, .. } => {
            let artifact_node = correspondence.artifact_node(id)?;
            // A parameter the operator refuses is placed at the parameter, as the operator names it.
            let parameter = (finding.rule == Rule::ValidParameters)
                .then(|| parsed.artifact.nodes.iter().find(|node| &node.id == artifact_node))
                .flatten()
                .and_then(|node| match registry.configure(node.op.as_str(), &node.params) {
                    Err(ConfigureError::InvalidParams { errors, .. }) => errors
                        .first()
                        .and_then(|error| parsed.spans.param(node.id.as_str(), &error.path)),
                    _ => None,
                });
            parameter.or_else(|| parsed.spans.node(artifact_node.as_str()))
        }
        Location::Edge { index } => {
            let edge = plan.edges.get(*index)?;
            // The artifact edge that entered the same port of the same node, if the node came from the artifact.
            let artifact_edge = parsed
                .artifact
                .edges
                .iter()
                .position(|artifact_edge| match &artifact_edge.to {
                    saluki_metric_plan::ir::ArtifactPortRef::Node { node, port } => {
                        port == &edge.to.port
                            && correspondence
                                .get(node)
                                .is_some_and(|placed| placed.node == edge.to.node)
                    }
                    saluki_metric_plan::ir::ArtifactPortRef::Input(_)
                    | saluki_metric_plan::ir::ArtifactPortRef::Exit(_) => false,
                });
            artifact_edge
                .and_then(|index| parsed.spans.edge(index))
                .or_else(|| node_span(&edge.to.node))
                .or_else(|| node_span(&edge.from.node))
        }
        Location::Output(output) => emit_span(parsed, output),
        Location::Plan | Location::Scope(_) => None,
    }
}

#[cfg(test)]
pub(crate) mod tests;
