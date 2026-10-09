//! Compiling surface syntax into a policy artifact, for the editor.
//!
//! Compiling parses the source with `saluki-metric-plan-syntax`, reads the artifact document back with the plan crate
//! (which checks what reading a document checks, such as the blob table, and computes the hash approval binds to), and,
//! when ADP's export bundle is at hand, runs the structural [checks](crate::checks) against its catalog. Every problem
//! is a diagnostic with a line, a column, and the span of the source it is about, so the editor can mark it.
//!
//! The editor shows programs printed with [`DISPLAY`]: narrow enough for the editor at 1280 pixels, and without the
//! output fingerprints, which are long digests a reader does not edit. A source that lists no fingerprints gets them
//! back from the export when it is compiled (see [`compile`]), as the syntax crate's documentation asks of tools that
//! elide them.

use saluki_metric_plan::{
    canonical::PlanHash,
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

    /// The artifact document: reading it as a policy artifact failed.
    Document,

    /// The structural checks against ADP's export bundle.
    Check,
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

    /// What is wrong.
    pub message: String,

    /// Where it is. A problem that cannot be placed points at the start of the source.
    pub range: Range,
}

/// Whether the source was checked against ADP's export bundle.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CheckContext {
    /// Whether the structural checks ran.
    pub available: bool,

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

    /// Whether the structural checks ran.
    pub checked_against: CheckContext,

    /// The plan diff preview, which arrives with composition in stage 5.
    pub preview: Preview,
}

/// The plan diff preview of an artifact against the active plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Preview {
    /// Whether a preview was computed.
    pub available: bool,

    /// Why it was not.
    pub reason: String,
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
struct LineIndex<'a> {
    source: &'a str,
    line_starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    fn new(source: &'a str) -> Self {
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

    fn range(&self, span: Option<Span>) -> Range {
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

/// Gives an artifact that lists no output fingerprints the export's fingerprint of every output it emits to, and
/// returns those outputs.
///
/// The base of the export lists each output's fingerprint as of the default program; an output it does not list takes
/// the catalog's. An output neither knows stays out, and the checks report the emit.
fn restore_fingerprints(artifact: &mut PolicyArtifact, export: &ExportBundle) -> Vec<String> {
    if !artifact.base.outputs.is_empty() {
        return Vec::new();
    }
    let emitted = artifact
        .nodes
        .iter()
        .filter(|node| node.op.as_str() == names::EMIT)
        .filter_map(|node| node.params.get("output")?.as_str()?.parse::<OutputId>().ok())
        .collect::<Vec<_>>();
    let mut restored = Vec::new();
    for output in emitted {
        let fingerprint = export
            .artifact
            .artifact
            .base
            .outputs
            .get(&output)
            .or_else(|| export.catalog.output(&output).map(|declared| &declared.fingerprint));
        if let Some(fingerprint) = fingerprint {
            restored.push(output.to_string());
            artifact.base.outputs.insert(output, *fingerprint);
        }
    }
    restored
}

/// Compiles a source, checking it against an export bundle when one is given.
///
/// A source that lists no output fingerprints, as the editor prints programs, gets the export's fingerprint of each
/// output it emits to (see [`CompileOutcome::restored_fingerprints`]); without an export, it keeps none.
///
/// Returns the outcome for the editor and, if the source produced an artifact, the artifact for publishing. Structural
/// findings do not prevent publishing: ADP is the authority, and publishing an artifact the checks flag is how the
/// demo shows ADP's own rejection.
pub fn compile(source: &str, export: Option<&ExportBundle>) -> (CompileOutcome, Option<CompiledArtifact>) {
    let index = LineIndex::new(source);
    let checked_against = match export {
        Some(export) => CheckContext {
            available: true,
            reason: None,
            default_program_hash: Some(export.program_hash().to_string()),
        },
        None => CheckContext {
            available: false,
            reason: Some(
                "ADP's export bundle is not available, so the artifact was parsed but not checked against ADP's \
                 catalog."
                    .to_string(),
            ),
            default_program_hash: None,
        },
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
        checked_against,
        preview: Preview {
            available: false,
            reason: "ADP validates the composed plan when it installs the artifact: composition, validation, and the \
                     plan diff preview arrive with stage 5."
                .to_string(),
        },
    };

    let mut parsed = match parse(source) {
        Ok(parsed) => parsed,
        Err(diagnostics) => {
            outcome.diagnostics = diagnostics
                .iter()
                .map(|diagnostic| CompileDiagnostic {
                    severity: Severity::Error,
                    origin: Origin::Syntax,
                    message: diagnostic.message.clone(),
                    range: index.range(Some(diagnostic.span)),
                })
                .collect();
            outcome.errors = outcome.diagnostics.len();
            return (outcome, None);
        }
    };
    if let Some(export) = export {
        outcome.restored_fingerprints = restore_fingerprints(&mut parsed.artifact, export);
    }

    let document = match serde_json::to_value(&parsed.artifact) {
        Ok(document) => document,
        Err(error) => {
            outcome.diagnostics.push(CompileDiagnostic {
                severity: Severity::Error,
                origin: Origin::Document,
                message: format!("The artifact cannot be serialized: {}", error),
                range: index.range(None),
            });
            outcome.errors = 1;
            return (outcome, None);
        }
    };
    let received = match ReceivedArtifact::from_value(document.clone()) {
        Ok(received) => received,
        Err(error) => {
            outcome.diagnostics.push(CompileDiagnostic {
                severity: Severity::Error,
                origin: Origin::Document,
                message: error.to_string(),
                range: index.range(None),
            });
            outcome.errors = 1;
            outcome.artifact = Some(document);
            return (outcome, None);
        }
    };

    if let Some(export) = export {
        outcome.diagnostics = check_against_export(source, &parsed, export)
            .into_iter()
            .map(|finding| CompileDiagnostic {
                severity: finding.severity,
                origin: Origin::Check,
                message: finding.message,
                range: index.range(finding.span),
            })
            .collect();
        outcome.diagnostics.sort_by_key(|diagnostic| diagnostic.range.start);
    }
    outcome.errors = outcome
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == Severity::Error)
        .count();
    outcome.warnings = outcome.diagnostics.len() - outcome.errors;
    outcome.ok = true;
    outcome.artifact_hash = Some(received.hash.to_string());
    // Printed as it was written: without fingerprints if the export supplied them (they come back on compiling), with
    // them if the source listed them.
    let options = if outcome.restored_fingerprints.is_empty() && !parsed.artifact.base.outputs.is_empty() {
        DISPLAY_WITH_FINGERPRINTS
    } else {
        DISPLAY
    };
    outcome.canonical_source = Some(print(&parsed.artifact, &options));
    outcome.artifact = Some(document.clone());
    (
        outcome,
        Some(CompiledArtifact {
            document,
            hash: received.hash,
        }),
    )
}

/// Prints an export bundle's artifact as source, for the editor's starting point, as [`DISPLAY`] prints.
pub fn default_program_source(export: &ExportBundle) -> String {
    print(&export.artifact.artifact, &DISPLAY)
}

#[cfg(test)]
mod tests;
