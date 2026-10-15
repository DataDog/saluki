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
//! `base` statement, whose program hash and output fingerprints are digests a reader does not edit. A source without
//! a base gets the export's program hash when it is compiled, and the fingerprints of the outputs it emits to, as the
//! syntax crate's documentation asks of tools that elide them.
//!
//! The default program is the whole metrics path, so the editor prints it with ADP's notes as comments ([`annotate`]):
//! the note of each node, from lowering, which says what the node stands for, a line where the sources' inputs begin,
//! which ADP does not know. Comments are not part of the artifact, so they change neither it nor its hash. The printed
//! form writes edges in the syntax's canonical order, the edges from the scope's inputs first, in input-name order,
//! while the export lists them in the default program's order: the compiled document then differs from the export in
//! the order of its edges, and its artifact hash with it, but composition puts edges in the default program's order, so
//! an unmodified program composes to the default program, with its hash.

use saluki_metric_plan::{
    canonical::PlanHash,
    compose::receive_artifact,
    ir::{ArtifactBase, OutputId, PolicyArtifact, ReceivedArtifact},
    operators::names,
    validate::{Catalog as _, CatalogRecipient, RecipientSource},
};
use saluki_metric_plan_syntax::{parse_with, print, ParseOptions, PrintOptions, Span};
use serde::Serialize;
use serde_json::Value;

use crate::{
    checks::{check_against_export, Severity},
    export::ExportBundle,
    preview::{compose_preview, CompileContext, Preview},
};

/// How the editor prints programs: lines of up to about 64 characters where the syntax can break them, which the
/// editor shows whole at 1280 pixels, and no `base` statement, which the export gives back.
pub const DISPLAY: PrintOptions = PrintOptions {
    elide_fingerprints: true,
    elide_base: true,
    width: 64,
};

/// How the editor prints a program that lists its output fingerprints, which are kept with its base.
const DISPLAY_WITH_FINGERPRINTS: PrintOptions = PrintOptions {
    elide_fingerprints: false,
    elide_base: false,
    width: 64,
};

/// How the editor prints a program whose source states its base, which is kept.
const DISPLAY_WITH_BASE: PrintOptions = PrintOptions {
    elide_fingerprints: true,
    elide_base: false,
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

/// The labels the demo gives endpoints, by URL, which the editor writes beside the recipients named by their URLs.
#[derive(Clone, Debug, Default)]
pub struct RecipientLabels(Vec<(String, String)>);

impl RecipientLabels {
    /// Returns the labels of `(URL, label)` pairs.
    pub fn new(labels: Vec<(String, String)>) -> Self {
        Self(labels)
    }

    /// Returns the label of the endpoint at `url`, if the demo records one.
    pub fn of(&self, url: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(endpoint, _)| endpoint == url)
            .map(|(_, label)| label.as_str())
    }
}

/// The comment that marks the scope's inputs, where each source's metrics enter the program.
const INPUTS_SECTION: &str = "# Sources: where each one's metrics enter";

/// The node above which the recipient directory is listed: the policy `tee`, where a program routes.
const POLICY_NODE: &str = "metrics_policy";

/// Returns a printed program with ADP's notes as comment lines: a line before the first edge from a scope input, each
/// node's note from the export ([`ExportBundle::annotations`]) above its definition, and the recipient directory above
/// the policy `tee`, each recipient with its endpoint and the label `labels` gives it, said to be not addressed when no
/// `emit` of `artifact` reaches it.
///
/// A program whose nodes ADP did not write, such as one that renames them, gets fewer comments; without an export, only
/// the inputs line. The comments change nothing in the artifact.
pub fn annotate(
    printed: &str, artifact: &PolicyArtifact, export: Option<&ExportBundle>, labels: &RecipientLabels,
) -> String {
    let mut marks = Vec::new();
    if let Some(at) = printed.find("\n  input.") {
        marks.push((at + 1, 0, vec![INPUTS_SECTION.to_string()]));
    }
    let definition = |node: &str| printed.find(&format!("\n  node {} = ", node)).map(|at| at + 1);
    if let Some(export) = export {
        for (node, note) in &export.annotations.nodes {
            if let Some(at) = definition(node.as_str()) {
                marks.push((at, 1, vec![format!("# {}", note)]));
            }
        }
        if let Some(at) = definition(POLICY_NODE) {
            let lines = recipient_lines(artifact, export, labels);
            if !lines.is_empty() {
                marks.push((at, 0, lines));
            }
        }
    }
    marks.sort();
    let mut annotated = String::with_capacity(printed.len() + 64 * marks.len());
    let mut copied = 0;
    for (at, _, lines) in marks {
        annotated.push_str(&printed[copied..at]);
        for line in lines {
            annotated.push_str("  ");
            annotated.push_str(&line);
            annotated.push('\n');
        }
        copied = at;
    }
    annotated.push_str(&printed[copied..]);
    annotated
}

/// Returns the comment lines that list the export's recipient directory: a heading, then one row per recipient, in
/// aligned columns: its name as a program writes it (quoted when it is an endpoint's URL), the demo's label of its
/// endpoint, its endpoint when the name is not the endpoint, and notes: whether no `emit` of `artifact` reaches it (by
/// its name, or through an output that delivers to it), and whether a resources artifact declares it. The primary and
/// failover recipients come first, then the others by label.
fn recipient_lines(artifact: &PolicyArtifact, export: &ExportBundle, labels: &RecipientLabels) -> Vec<String> {
    let emits = artifact
        .nodes
        .iter()
        .filter(|node| node.op.as_str() == names::EMIT)
        .collect::<Vec<_>>();
    let named = emits
        .iter()
        .filter_map(|node| node.params.get("to")?.as_array())
        .flatten()
        .filter_map(|name| name.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let through_outputs = emits
        .iter()
        .filter_map(|node| node.params.get("output")?.as_str()?.parse::<OutputId>().ok())
        .filter_map(|output| export.catalog.output(&output))
        .flat_map(|output| output.recipients.iter())
        .collect::<std::collections::BTreeSet<_>>();
    let reached = |name: &str, entry: &CatalogRecipient| {
        named.contains(name)
            || entry
                .recipients
                .iter()
                .any(|recipient| through_outputs.contains(recipient))
    };
    let mut rows = export
        .catalog
        .recipients()
        .map(|(name, entry)| {
            let written = if saluki_metric_plan::ir::is_identifier(name.as_str()) {
                name.to_string()
            } else {
                serde_json::to_string(name.as_str()).unwrap_or_else(|_| format!("\"{}\"", name))
            };
            let label = labels.of(&entry.endpoint).unwrap_or("").to_string();
            let endpoint = if name.as_str() == entry.endpoint {
                String::new()
            } else {
                entry.endpoint.clone()
            };
            let mut notes = Vec::new();
            if entry.source == RecipientSource::Resources {
                notes.push("resources");
            }
            if !reached(name.as_str(), entry) {
                notes.push("not addressed");
            }
            let rank = match name.as_str() {
                "primary" => 0,
                "mrf" => 1,
                "cluster_agent" => 2,
                _ => 3,
            };
            (rank, label, written, endpoint, notes)
        })
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return Vec::new();
    }
    rows.sort_by(|a, b| (a.0, &a.1, &a.2).cmp(&(b.0, &b.1, &b.2)));
    let name_width = rows.iter().map(|row| row.2.chars().count()).max().unwrap_or(0);
    let label_width = rows.iter().map(|row| row.1.chars().count()).max().unwrap_or(0);
    let mut lines = vec!["# Recipients:".to_string()];
    for (_, label, written, endpoint, notes) in rows {
        let mut line = format!("#   {:<name_width$}", written);
        if label_width > 0 {
            line.push_str(&format!("  {:<label_width$}", label));
        }
        if !endpoint.is_empty() {
            line.push_str(&format!("  {}", endpoint));
        }
        if !notes.is_empty() {
            line.push_str(&format!("  ({})", notes.join("; ")));
        }
        lines.push(line.trim_end().to_string());
    }
    lines
}

/// Gives an artifact that lists no output fingerprints the fingerprint of every output it emits to by ID, and returns
/// those outputs, each once, in the order the artifact first emits to them.
///
/// The base of the export lists each output's fingerprint as of the default program; an output it does not list takes
/// the catalog's. An output neither knows stays out, and composition reports the emit. An `emit` addressed to
/// recipients records no fingerprint: the names are the reference.
fn restore_fingerprints(artifact: &mut PolicyArtifact, export: &ExportBundle) -> Vec<String> {
    let mut restored = Vec::new();
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
            restored.push(output.to_string());
            artifact.base.outputs.insert(output, *fingerprint);
        }
    }
    restored
}

/// Compiles a source, checking it against what ADP serves when `context` gives it.
///
/// A source that lists no output fingerprints, as the editor prints programs, gets the export's fingerprint of each
/// output it emits to by ID (see [`CompileOutcome::restored_fingerprints`]); without an export, it keeps none. With
/// the default program too, the artifact is composed into it, the composed plan validated, and the plan diff computed,
/// as ADP will (see [`compose_preview`]); with the export alone, the structural checks run.
///
/// Returns the outcome for the editor and, if the source produced an artifact, the artifact for publishing. Findings
/// do not prevent publishing, not even those ADP is certain to reject: ADP is the authority, and publishing an artifact
/// the editor flags is how the demo shows ADP's own rejection.
pub fn compile(source: &str, context: Option<&CompileContext<'_>>) -> (CompileOutcome, Option<CompiledArtifact>) {
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
        checked_against,
        preview,
    };

    // A source printed without its base, as the editor prints programs, records the export's program hash; the
    // fingerprints are restored below.
    let options = ParseOptions {
        base: export.map(|export| ArtifactBase {
            program_hash: *export.program_hash(),
            outputs: Default::default(),
        }),
    };
    let mut parsed = match parse_with(source, &options) {
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
        outcome.restored_fingerprints = restore_fingerprints(&mut parsed.artifact, export);
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
    // Printed as it was written: without its base if the export gave it (it comes back on compiling), and with the base
    // the source states otherwise, its fingerprints too if it lists them.
    let supplied = !outcome.restored_fingerprints.is_empty();
    let options = if parsed.base_given {
        DISPLAY
    } else if !supplied && !parsed.artifact.base.outputs.is_empty() {
        DISPLAY_WITH_FINGERPRINTS
    } else {
        DISPLAY_WITH_BASE
    };
    let labels = context.map(|context| context.labels).cloned().unwrap_or_default();
    outcome.canonical_source = Some(annotate(
        &print(&parsed.artifact, &options),
        &parsed.artifact,
        export,
        &labels,
    ));
    outcome.artifact = Some(document.clone());
    (outcome, Some(CompiledArtifact { document, hash }))
}

/// Prints an export bundle's artifact as source, for the editor's starting point, as [`DISPLAY`] prints, with ADP's
/// notes as comments and the demo's labels of the recipients ([`annotate`]).
pub fn default_program_source(export: &ExportBundle, labels: &RecipientLabels) -> String {
    let artifact = &export.artifact.artifact;
    annotate(&print(artifact, &DISPLAY), artifact, Some(export), labels)
}

#[cfg(test)]
mod tests;
