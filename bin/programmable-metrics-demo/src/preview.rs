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
//! owner the frame cannot run where it is placed, such as the aggregation moved after enrichment. The preview does
//! not place the plan.

use saluki_metric_plan::{
    canonical::{PlanHash, ResolvedValues},
    compose::{compose_all, demanded_outputs, receive_artifact, Approval, ComposeError, ComposeOutcome},
    install::{
        diff::{NodeChange, OutputChange},
        diff_with_correspondence, Correspondence,
    },
    ir::{
        ArtifactNodeId, OutputBinding, OutputDecl, OutputId, Plan, PortRef, ReceivedArtifact, RecipientName, Selector,
    },
    operators::{names, ConfigureError, OperatorRegistry},
    validate::{Catalog as _, CatalogOutput, Finding, Location, Rule, Severity as ValidationSeverity},
};
use saluki_metric_plan_syntax::{Parsed, Span};
use serde::Serialize;

use crate::{
    checks::Severity,
    compile::{LineIndex, Origin, Range, RecipientLabels},
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

    /// The revision ADP runs it under, which composition records in the plan's provenance.
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
            span: parsed
                .spans
                .param(stand_in.emit.as_str(), "to")
                .or_else(|| parsed.spans.node(stand_in.emit.as_str())),
        })
        .collect::<Vec<_>>();
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
            };
            (&other.artifact, approval)
        })
        .collect::<Vec<_>>();
    let mut artifacts = vec![(received, &approval)];
    artifacts.extend(others.iter().map(|(artifact, approval)| (*artifact, approval)));
    let composed = match compose_all(default_program, &artifacts, &catalog, &registry) {
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
                span: compose_error_span(&error, parsed, source),
            };
            adds.push(finding);
            return (adds, Preview::unavailable(reason));
        }
    };

    // The edited artifact's own composition: its correspondence places findings and diff nodes in its source.
    let (correspondence, stale_base) = match composed.scopes.get(scope) {
        Some(own) => (&own.correspondence, own.stale_base),
        None => (&composed.correspondence, composed.stale_base),
    };
    let mut findings = adds;
    // The artifact ADP runs, composed again in effect, may run degraded: its emits to recipients that are gone dropped.
    if let ComposeOutcome::Degraded(dropped) = &own_outcome(&composed, scope) {
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
            served_default_program,
            PreviewBase {
                kind: BaseKind::DefaultProgram,
                plan_hash: served_default_program
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
                "Composed into ADP's default program{} and validated as ADP does when the artifact arrives; the diff \
                 compares the composed plan with the active plan. The plan is not placed into ADP's regions here.{}",
                with_others, provisional
            ),
            BaseKind::DefaultProgram => format!(
                "Composed into ADP's default program{} and validated as ADP does when the artifact arrives; ADP's \
                 active plan was not available, so the diff compares the composed plan with the default program. The \
                 plan is not placed into ADP's regions here.{}",
                with_others, provisional
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
            fingerprint: self.decl.fingerprint,
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
            // As ADP binds an implied output: its selector its recipients, its fingerprint version 2 over them.
            let binding = template.binding.clone().map(|binding| OutputBinding {
                selector: Selector::Recipients(demand.recipients.clone()),
                ..binding
            });
            let fingerprint = match &binding {
                Some(binding) => {
                    saluki_metric_plan::resources::fingerprint(&binding.host, template.accepts, &binding.selector)
                }
                None => PlanHash::of_value(&serde_json::json!({
                    "stand_in": id.as_str(),
                    "recipients": demand.recipients
                }))
                .ok()?,
            };
            let names = export
                .catalog
                .recipients()
                .filter(|(_, entry)| entry.recipients.iter().any(|id| demand.recipients.contains(id)))
                .map(|(name, _)| name.clone())
                .collect();
            Some(StandIn {
                decl: OutputDecl {
                    id,
                    fingerprint,
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

/// Returns how the artifact of `scope` composed: as written, or degraded.
fn own_outcome(
    composed: &saluki_metric_plan::compose::Composed, scope: &saluki_metric_plan::ir::ScopeName,
) -> ComposeOutcome {
    composed
        .scopes
        .get(scope)
        .map(|own| own.outcome.clone())
        .unwrap_or(ComposeOutcome::Composed)
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
        // A policy binding is placed where the node writes it, as `config("<binding>")`, or at the node.
        ComposeError::PolicyBinding { node, binding, .. } => {
            let within = parsed.spans.node(node.as_str())?;
            source
                .get(within.start..within.end)
                .and_then(|text| find(text, &format!("config(\"{}\")", binding)))
                .map(|found| Span::new(within.start + found.start, within.start + found.end))
                .or(Some(within))
        }
        ComposeError::UnknownRecipient { node, .. }
        | ComposeError::PendingRecipients { node, .. }
        | ComposeError::NoOutputFor { node, .. }
        | ComposeError::RecipientsOnTwoHosts { node, .. } => parsed
            .spans
            .param(node.as_str(), "to")
            .or_else(|| parsed.spans.node(node.as_str())),
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
