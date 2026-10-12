//! Compiling surface syntax into a policy artifact, for the editor.
//!
//! Compiling parses the source with `saluki-metric-plan-syntax`, receives the artifact document as ADP does (the plan
//! crate's `receive_artifact`: the artifact limits, what reading a document checks, such as the blob table, and the
//! hash approval binds to), and, when ADP's export bundle and default program are at hand, composes the artifact into
//! the default program, validates the composed plan, and diffs it against the active plan, as ADP will when the
//! artifact arrives (see [`preview`](crate::preview)). With an export but no default program (an ADP that does not
//! serve it), the structural [checks](crate::checks) run against the export's catalog instead. Every problem is a
//! diagnostic with a line, a column, and the span of the source it is about, so the editor can mark it.
//!
//! The editor shows programs printed with [`DISPLAY`]: narrow enough for the editor at 1280 pixels, and without the
//! output fingerprints, which are long digests a reader does not edit. A source that lists no fingerprints gets them
//! back from the export when it is compiled, as the syntax crate's documentation asks of tools that elide them; an
//! output a resources artifact declares that the export does not list yet gets its fingerprint from the Resources tab's
//! draft instead, so that a policy can wait for it (see [`compile_with_resources`]).

use saluki_metric_plan::{
    canonical::PlanHash,
    compose::receive_artifact,
    ir::{OutputId, PolicyArtifact, ReceivedArtifact},
    operators::names,
    validate::Catalog as _,
};
use saluki_metric_plan_syntax::{parse, print, PrintOptions, Span};
use serde::Serialize;
use serde_json::Value;

use crate::{
    checks::{check_against_export, Severity},
    export::ExportBundle,
    preview::{compose_preview, CompileContext, Preview},
    resources::DraftFingerprints,
};

/// How the editor prints programs: lines of up to about 64 characters where the syntax can break them, which the
/// editor shows whole at 1280 pixels, and no output fingerprints.
pub const DISPLAY: PrintOptions = PrintOptions {
    elide_fingerprints: true,
    width: 64,
};

/// How the editor prints a program that lists its output fingerprints, which are kept.
const DISPLAY_WITH_FINGERPRINTS: PrintOptions = PrintOptions {
    elide_fingerprints: false,
    width: 64,
};

/// Where a diagnostic comes from.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// The surface syntax: the text does not parse, or refers to something it does not define.
    Syntax,

    /// The artifact document: receiving it as ADP does (its limits, reading it as a policy artifact) failed.
    Document,

    /// The structural checks against ADP's export bundle, which run when ADP does not serve its default program.
    Check,

    /// Composing the artifact into ADP's default program.
    Composition,

    /// Validating the composed plan.
    Validation,
}

/// A source range as lines and columns, both counted from 1, columns in characters.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct Range {
    /// The line the range starts on.
    pub line: usize,

    /// The column the range starts at.
    pub column: usize,

    /// The line the range ends on.
    pub end_line: usize,

    /// The column just past the range's end.
    pub end_column: usize,

    /// The byte offset of the range's start.
    pub start: usize,

    /// The byte offset just past the range's end.
    pub end: usize,
}

/// One problem, placed in the source.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CompileDiagnostic {
    /// How serious it is.
    pub severity: Severity,

    /// Where it comes from.
    pub origin: Origin,

    /// The validation rule, for a validation finding.
    pub rule: Option<&'static str>,

    /// What is wrong.
    pub message: String,

    /// Where it is. A problem that cannot be placed points at the start of the source.
    pub range: Range,
}

/// What the source was checked against.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CheckContext {
    /// Whether the source was checked against ADP's export bundle: composed and validated, or checked structurally.
    pub available: bool,

    /// Whether the artifact was composed into ADP's default program and the composed plan validated, rather than only
    /// checked structurally.
    pub composed: bool,

    /// Why they did not run, when they did not.
    pub reason: Option<String>,

    /// The default program hash the checks used.
    pub default_program_hash: Option<String>,
}

/// The result of compiling a source.
#[derive(Clone, Debug, Serialize)]
pub struct CompileOutcome {
    /// Whether the source produced an artifact, which may then be published.
    pub ok: bool,

    /// How many diagnostics are errors.
    pub errors: usize,

    /// How many diagnostics are warnings.
    pub warnings: usize,

    /// Every problem, in source order.
    pub diagnostics: Vec<CompileDiagnostic>,

    /// The artifact document.
    pub artifact: Option<Value>,

    /// The hash approval binds to: the plan crate's hash of the artifact document.
    pub artifact_hash: Option<String>,

    /// The artifact printed back in canonical form, as the editor prints it.
    pub canonical_source: Option<String>,

    /// The outputs whose fingerprints the source did not list and the export supplied.
    pub restored_fingerprints: Vec<String>,

    /// The outputs whose fingerprints the source did not list, the export does not declare, and the resources draft
    /// supplied: ADP holds the artifact `pending` until a resources artifact declares them.
    pub draft_fingerprints: Vec<String>,

    /// Why the resources draft supplied no fingerprints, when the source emits to an output only it could declare and
    /// the draft cannot be read or does not validate.
    pub draft_note: Option<String>,

    /// What the source was checked against.
    pub checked_against: CheckContext,

    /// The plan diff preview: what installing the artifact changes.
    pub preview: Preview,
}

/// A compiled artifact, kept for publishing.
#[derive(Clone, Debug)]
pub struct CompiledArtifact {
    /// The artifact document.
    pub document: Value,

    /// Its hash.
    pub hash: PlanHash,
}

/// Maps byte offsets of a source to lines and columns.
pub(crate) struct LineIndex<'a> {
    source: &'a str,
    line_starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    pub(crate) fn new(source: &'a str) -> Self {
        let line_starts = std::iter::once(0)
            .chain(source.match_indices('\n').map(|(at, _)| at + 1))
            .collect();
        Self { source, line_starts }
    }

    /// Returns the line and column of an offset, both from 1, the column in characters.
    fn position(&self, offset: usize) -> (usize, usize) {
        let mut offset = offset.min(self.source.len());
        while !self.source.is_char_boundary(offset) {
            offset -= 1;
        }
        let line = self.line_starts.partition_point(|start| *start <= offset);
        let start = self.line_starts[line - 1];
        (line, self.source[start..offset].chars().count() + 1)
    }

    pub(crate) fn range(&self, span: Option<Span>) -> Range {
        let span = span.unwrap_or_default();
        let (line, column) = self.position(span.start);
        let (end_line, end_column) = self.position(span.end.max(span.start));
        Range {
            line,
            column,
            end_line,
            end_column,
            start: span.start,
            end: span.end.max(span.start),
        }
    }
}

/// The outputs whose fingerprints [`restore_fingerprints`] gave an artifact, by where each came from.
#[derive(Debug, Default)]
struct Restored {
    /// From the export: its base, or its catalog.
    export: Vec<String>,

    /// From the resources draft, for outputs the export does not declare.
    draft: Vec<String>,

    /// Declared outputs (`resources_<name>`) the artifact emits to that neither gave.
    missing_declared: Vec<String>,
}

/// Gives an artifact that lists no output fingerprints the fingerprint of every output it emits to, and returns those
/// outputs, each once, in the order the artifact first emits to them.
///
/// The base of the export lists each output's fingerprint as of the default program; an output it does not list takes
/// the catalog's; a declared output (`resources_<name>`) the catalog does not declare yet takes the resources draft's.
/// An output none knows stays out, and the checks report the emit.
fn restore_fingerprints(artifact: &mut PolicyArtifact, export: &ExportBundle, draft: &DraftFingerprints) -> Restored {
    let mut restored = Restored::default();
    if !artifact.base.outputs.is_empty() {
        return restored;
    }
    let mut emitted = Vec::new();
    for output in artifact
        .nodes
        .iter()
        .filter(|node| node.op.as_str() == names::EMIT)
        .filter_map(|node| node.params.get("output")?.as_str()?.parse::<OutputId>().ok())
    {
        if !emitted.contains(&output) {
            emitted.push(output);
        }
    }
    for output in emitted {
        let exported = export
            .artifact
            .artifact
            .base
            .outputs
            .get(&output)
            .or_else(|| export.catalog.output(&output).map(|declared| &declared.fingerprint));
        if let Some(fingerprint) = exported {
            restored.export.push(output.to_string());
            artifact.base.outputs.insert(output, *fingerprint);
        } else if let Some(fingerprint) = draft.fingerprints.get(&output) {
            restored.draft.push(output.to_string());
            artifact.base.outputs.insert(output, *fingerprint);
        } else if output.is_declared() {
            restored.missing_declared.push(output.to_string());
        }
    }
    restored
}

/// Compiles a source without a resources draft, as [`compile_with_resources`] does with an empty one: the demo server
/// always sends the Resources tab's draft, so only tests compile without one.
#[cfg(test)]
pub fn compile(source: &str, context: Option<&CompileContext<'_>>) -> (CompileOutcome, Option<CompiledArtifact>) {
    compile_with_resources(source, context, &DraftFingerprints::default())
}

/// Compiles a source, checking it against what ADP serves when `context` gives it.
///
/// A source that lists no output fingerprints, as the editor prints programs, gets the export's fingerprint of each
/// output it emits to (see [`CompileOutcome::restored_fingerprints`]); without an export, it keeps none. A declared
/// output the export does not declare takes its fingerprint from the resources draft's fingerprints (`draft`), so that
/// an artifact can be published before the resources it emits to: ADP holds it `pending` until a resources artifact
/// declares them (see [`CompileOutcome::draft_fingerprints`]). With the default program too, the artifact is composed
/// into it, the composed plan validated, and the plan diff computed, as ADP will (see [`compose_preview`]); with the
/// export alone, the structural checks run.
///
/// Returns the outcome for the editor and, if the source produced an artifact, the artifact for publishing. Findings
/// do not prevent publishing, not even those ADP is certain to reject: ADP is the authority, and publishing an artifact
/// the editor flags is how the demo shows ADP's own rejection.
pub fn compile_with_resources(
    source: &str, context: Option<&CompileContext<'_>>, draft: &DraftFingerprints,
) -> (CompileOutcome, Option<CompiledArtifact>) {
    let index = LineIndex::new(source);
    let export = context.map(|context| context.export);
    let default_program = context.and_then(|context| context.default_program);
    let checked_against = match (export, default_program) {
        (Some(export), Some(_)) => CheckContext {
            available: true,
            composed: true,
            reason: None,
            default_program_hash: Some(export.program_hash().to_string()),
        },
        (Some(export), None) => CheckContext {
            available: true,
            composed: false,
            reason: Some(
                "ADP does not serve its default program (GET /programmable-metrics/default-program), which composing \
                 the artifact needs, so only the structural checks against its export ran."
                    .to_string(),
            ),
            default_program_hash: Some(export.program_hash().to_string()),
        },
        (None, _) => CheckContext {
            available: false,
            composed: false,
            reason: Some(
                "ADP's export bundle is not available, so the artifact was parsed but not checked against ADP's \
                 catalog."
                    .to_string(),
            ),
            default_program_hash: None,
        },
    };
    let preview = match &checked_against.reason {
        Some(reason) => Preview::unavailable(format!("No plan diff: {}", reason)),
        None => Preview::default(),
    };
    let mut outcome = CompileOutcome {
        ok: false,
        errors: 0,
        warnings: 0,
        diagnostics: Vec::new(),
        artifact: None,
        artifact_hash: None,
        canonical_source: None,
        restored_fingerprints: Vec::new(),
        draft_fingerprints: Vec::new(),
        draft_note: None,
        checked_against,
        preview,
    };

    let mut parsed = match parse(source) {
        Ok(parsed) => parsed,
        Err(diagnostics) => {
            outcome.diagnostics = diagnostics
                .iter()
                .map(|diagnostic| CompileDiagnostic {
                    severity: Severity::Error,
                    origin: Origin::Syntax,
                    rule: None,
                    message: diagnostic.message.clone(),
                    range: index.range(Some(diagnostic.span)),
                })
                .collect();
            outcome.errors = outcome.diagnostics.len();
            outcome.preview = Preview::unavailable("No plan diff: the source does not parse.");
            return (outcome, None);
        }
    };
    if let Some(export) = export {
        let restored = restore_fingerprints(&mut parsed.artifact, export, draft);
        if !restored.missing_declared.is_empty() {
            let outputs = restored
                .missing_declared
                .iter()
                .map(|output| format!("`{}`", output))
                .collect::<Vec<_>>()
                .join(", ");
            outcome.draft_note = Some(match &draft.error {
                Some(error) => format!(
                    "ADP does not declare {} yet, and the resources draft gives no fingerprint for it: {}",
                    outputs, error
                ),
                None => format!(
                    "ADP does not declare {} yet, and the resources draft does not declare it either.",
                    outputs
                ),
            });
        }
        outcome.restored_fingerprints = restored.export;
        outcome.draft_fingerprints = restored.draft;
    }

    let document = match serde_json::to_value(&parsed.artifact) {
        Ok(document) => document,
        Err(error) => {
            outcome.diagnostics.push(CompileDiagnostic {
                severity: Severity::Error,
                origin: Origin::Document,
                rule: None,
                message: format!("The artifact cannot be serialized: {}", error),
                range: index.range(None),
            });
            outcome.errors = 1;
            outcome.preview = Preview::unavailable("No plan diff: there is no artifact document.");
            return (outcome, None);
        }
    };
    // Reading the document gives the hash approval binds to; without one there is nothing to publish.
    let hash = match ReceivedArtifact::from_value(document.clone()) {
        Ok(received) => received.hash,
        Err(error) => {
            outcome.diagnostics.push(CompileDiagnostic {
                severity: Severity::Error,
                origin: Origin::Document,
                rule: None,
                message: error.to_string(),
                range: index.range(None),
            });
            outcome.errors = 1;
            outcome.artifact = Some(document);
            outcome.preview = Preview::unavailable("No plan diff: the document is not a policy artifact.");
            return (outcome, None);
        }
    };

    // ADP receives the document with the artifact limits checked first. One it refuses can still be published, to
    // show the refusal, but there is nothing to compose.
    match (receive_artifact(document.clone()), context) {
        (Err(error), _) => {
            outcome.diagnostics.push(CompileDiagnostic {
                severity: Severity::Error,
                origin: Origin::Document,
                rule: None,
                message: format!("ADP refuses this document on receipt: {}", error),
                range: index.range(None),
            });
            outcome.preview = Preview::unavailable("No plan diff: ADP refuses the artifact on receipt.");
        }
        (Ok(received), Some(context)) => match context.default_program {
            Some(default_program) => {
                let (findings, preview) = compose_preview(source, &parsed, &received, context, default_program, &index);
                outcome.diagnostics = findings
                    .into_iter()
                    .map(|finding| CompileDiagnostic {
                        severity: finding.severity,
                        origin: finding.origin,
                        rule: finding.rule,
                        message: finding.message,
                        range: index.range(finding.span),
                    })
                    .collect();
                outcome.preview = preview;
            }
            None => {
                outcome.diagnostics = check_against_export(source, &parsed, context.export)
                    .into_iter()
                    .map(|finding| CompileDiagnostic {
                        severity: finding.severity,
                        origin: Origin::Check,
                        rule: None,
                        message: finding.message,
                        range: index.range(finding.span),
                    })
                    .collect();
            }
        },
        (Ok(_), None) => {}
    }
    outcome.diagnostics.sort_by_key(|diagnostic| diagnostic.range.start);
    outcome.errors = outcome
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == Severity::Error)
        .count();
    outcome.warnings = outcome.diagnostics.len() - outcome.errors;
    outcome.ok = true;
    outcome.artifact_hash = Some(hash.to_string());
    // Printed as it was written: without fingerprints if the export or the draft supplied them (they come back on
    // compiling), with them if the source listed them.
    let supplied = !outcome.restored_fingerprints.is_empty() || !outcome.draft_fingerprints.is_empty();
    let options = if !supplied && !parsed.artifact.base.outputs.is_empty() {
        DISPLAY_WITH_FINGERPRINTS
    } else {
        DISPLAY
    };
    outcome.canonical_source = Some(print(&parsed.artifact, &options));
    outcome.artifact = Some(document.clone());
    (outcome, Some(CompiledArtifact { document, hash }))
}

/// Prints an export bundle's artifact as source, for the editor's starting point, as [`DISPLAY`] prints.
pub fn default_program_source(export: &ExportBundle) -> String {
    print(&export.artifact.artifact, &DISPLAY)
}

#[cfg(test)]
mod tests;
