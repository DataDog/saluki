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
//! The editor shows programs printed with [`DISPLAY`]: at the syntax's own width, so a pipeline's stages and their `as`
//! clauses stay on one line each.
//!
//! The default program is the whole metrics path, so the editor prints it with ADP's notes as comments ([`annotate`]):
//! the note of each node, from lowering, which says what the node stands for, above the stage that runs it, and a line
//! that names the sources the program reads. Comments are not part of the artifact, so they change neither it nor its
//! hash. The printed form writes edges in the syntax's canonical order, the edges from the sources first, in name order,
//! while the export lists them in the default program's order: the compiled document then differs from the export in
//! the order of its edges, and its artifact hash with it, but composition puts edges in the default program's order, so
//! an unmodified program composes to the default program, with its hash.

use saluki_metric_plan::{
    canonical::PlanHash,
    compose::receive_artifact,
    ir::{ArtifactPortRef, OutputId, PolicyArtifact, ReceivedArtifact},
    operators::names,
    validate::{Catalog as _, CatalogRecipient},
};
use saluki_metric_plan_syntax::{parse, print, PrintOptions, Span};
use serde::Serialize;
use serde_json::Value;

use crate::{
    checks::{check_against_export, Severity},
    export::ExportBundle,
    preview::{compose_preview, CompileContext, Preview},
};

/// How the editor prints programs: lines of up to the syntax's default width, 120 characters, where the syntax can
/// break them. A narrower width would break most stages of the default program, whose `as` clauses keep its native
/// node IDs, across lines.
pub const DISPLAY: PrintOptions = PrintOptions { width: 120 };

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

/// How the comment that names the sources, where each source's metrics enter the program, starts.
const INPUTS_SECTION: &str = "Sources, where each one's metrics enter the program:";

/// The policy's tee, which copies the stream every destination reads, `metrics`, to each of its uses: the recipient
/// directory is listed above the first use.
const POLICY_TEE: &str = "metrics_fanout";

/// Returns a printed program with ADP's notes as comment lines: what the export leaves out because it does nothing as
/// configured, and the sources the program reads, at the top of the program, wrapped; each node's note from the
/// export ([`ExportBundle::annotations`]) above the line its stage starts on; and the recipient directory above the
/// first branch of the policy, the first use of `metrics` (what port `0` of the policy's tee feeds), above its whole
/// statement, each recipient with its endpoint and the label `labels` gives it, said to be not addressed when no `emit`
/// of `artifact` reaches it.
///
/// The stages are found by parsing `printed` back, through the span of each node, so the comments follow the printed
/// form whatever its layout; a comment is indented as the line it is above. A program whose nodes ADP did not write,
/// such as one that renames them, gets fewer comments; without an export, only the inputs line. The comments change
/// nothing in the artifact.
pub fn annotate(
    printed: &str, artifact: &PolicyArtifact, export: Option<&ExportBundle>, labels: &RecipientLabels,
) -> String {
    // Each mark is an offset, its order among the marks at that offset, its indentation, and its lines.
    let mut marks: Vec<(usize, i32, String, Vec<String>)> = Vec::new();
    if let Some(at) = printed.find(" {\n").map(|at| at + " {\n".len()) {
        if let Some(omitted) = export.and_then(|export| export.annotations.omitted.as_deref()) {
            let mut lines = wrapped(omitted, COMMENT_WIDTH);
            lines.push(String::new());
            // Before anything else at the top.
            marks.push((at, -2, "  ".to_string(), lines));
        }
        let mut lines = Vec::new();
        let inputs = artifact
            .edges
            .iter()
            .filter_map(|edge| match &edge.from {
                ArtifactPortRef::Input(input) => Some(input.as_str()),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        if !inputs.is_empty() {
            let names = inputs.into_iter().collect::<Vec<_>>().join(", ");
            lines.extend(wrapped(&format!("{} {}.", INPUTS_SECTION, names), COMMENT_WIDTH));
            lines.push(String::new());
        }
        // Before the first statement, after the headers.
        marks.push((statements_start(printed, at), -1, "  ".to_string(), lines));
    }
    // The line each node's stage starts on, from the spans of the printed text parsed back.
    let reparsed = parse(printed).ok();
    // A node a `let` names ends that `let`'s pipeline: its note goes above the `let`, which is where a reader looks
    // for it, rather than above its stage.
    let line_of = |node: &str| {
        let span = reparsed.as_ref()?.spans.node(node)?;
        let named = printed
            .match_indices(&format!("\n  let {} = ", node))
            .map(|(at, _)| at)
            .find(|at| *at < span.start);
        let start = match named {
            Some(at) => at + 1,
            None => printed[..span.start.min(printed.len())]
                .rfind('\n')
                .map_or(0, |at| at + 1),
        };
        let indent = printed[start..].chars().take_while(|c| *c == ' ').collect::<String>();
        Some((start, indent))
    };
    if let Some(export) = export {
        for (node, note) in &export.annotations.nodes {
            if let Some((at, indent)) = line_of(node.as_str()) {
                marks.push((at, 1, indent, vec![format!("# {}", note)]));
            }
        }
        // The first branch of the policy: what port `0` of the policy's tee feeds, or the tee itself.
        let first_branch = artifact
            .edges
            .iter()
            .find(|edge| {
                matches!(&edge.from, ArtifactPortRef::Node { node, port }
                    if node.as_str() == POLICY_TEE && port.as_str() == "0")
            })
            .and_then(|edge| match &edge.to {
                ArtifactPortRef::Node { node, .. } => line_of(node.as_str()),
                _ => None,
            })
            .or_else(|| line_of(POLICY_TEE))
            // A bare use can run over several lines, so the directory goes above its statement, not its stage.
            .map(|(at, _)| statement_line(printed, at));
        if let Some((at, indent)) = first_branch {
            let lines = recipient_lines(artifact, export, labels);
            if !lines.is_empty() {
                marks.push((at, 0, indent, lines));
            }
        }
    }
    marks.sort();
    let mut annotated = String::with_capacity(printed.len() + 64 * marks.len());
    let mut copied = 0;
    for (at, _, indent, lines) in marks {
        annotated.push_str(&printed[copied..at]);
        for line in lines {
            if !line.is_empty() {
                annotated.push_str(&indent);
                annotated.push_str(&line);
            }
            annotated.push('\n');
        }
        copied = at;
    }
    annotated.push_str(&printed[copied..]);
    annotated
}

/// Returns the offset and indentation of the line the statement containing the line at `at` starts on: the nearest
/// line at or above it indented as a statement of the program, two spaces, and not a continuation (`| ...` or a closing
/// bracket).
fn statement_line(printed: &str, at: usize) -> (usize, String) {
    let mut start = at;
    loop {
        let line = printed[start..].lines().next().unwrap_or("");
        let statement =
            line.starts_with("  ") && !line[2..].starts_with(' ') && !line[2..].starts_with(['|', ')', ']', '}', '#']);
        if statement || start == 0 {
            return (start, "  ".to_string());
        }
        start = printed[..start - 1].rfind('\n').map_or(0, |at| at + 1);
    }
}

/// Returns where the statements of a printed program start: the first line from `at`, just past the program's opening
/// brace, that is neither blank nor part of a header (`ir_version` or `blobs`).
fn statements_start(printed: &str, at: usize) -> usize {
    let mut offset = at;
    let mut in_block = false;
    for line in printed[at..].split_inclusive('\n') {
        let code = line.trim();
        let header = if in_block {
            in_block = code != "}";
            true
        } else if code.starts_with("blobs {") {
            in_block = !code.ends_with('}');
            true
        } else {
            code.is_empty() || code.starts_with("ir_version ")
        };
        if !header {
            break;
        }
        offset += line.len();
    }
    offset
}

/// The widest comment line the notes at the top of a program wrap to, their indentation and `# ` included: narrower
/// than the printed program's width, [`DISPLAY`]'s, since prose reads better in shorter lines than code.
const COMMENT_WIDTH: usize = 100;

/// Returns `note` as comment lines of at most `width` characters once [`annotate`] indents them, broken at spaces, the
/// lines after the first indented a little further.
fn wrapped(note: &str, width: usize) -> Vec<String> {
    // `annotate` indents every comment line by two spaces.
    let room = width.saturating_sub(2);
    let mut lines = Vec::new();
    let mut line = String::from("#");
    let mut started = false;
    for word in note.split_whitespace() {
        if started && line.chars().count() + 1 + word.chars().count() > room {
            lines.push(std::mem::replace(&mut line, String::from("#  ")));
        }
        line.push(' ');
        line.push_str(word);
        started = true;
    }
    lines.push(line);
    lines
}

/// Returns the comment lines that list the export's recipient directory: a heading, then one row per recipient, in
/// aligned columns: its name as a program writes it (quoted when it is an endpoint's URL), the demo's label of its
/// endpoint, its endpoint when the name is not the endpoint, and notes: whether no `emit` of `artifact` reaches it (by
/// its name, or through an output that delivers to it). The primary and
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

/// Compiles a source, checking it against what ADP serves when `context` gives it.
///
/// With the export and the default program, the artifact is composed into the default program, the composed plan validated, and the plan diff computed,
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
            default_program_hash: Some(export.program_hash.to_string()),
        },
        (Some(export), None) => CheckContext {
            available: true,
            composed: false,
            reason: Some(
                "ADP does not serve its default program (GET /programmable-metrics/default-program), which composing \
                 the artifact needs, so only the structural checks against its export ran."
                    .to_string(),
            ),
            default_program_hash: Some(export.program_hash.to_string()),
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
        checked_against,
        preview,
    };

    let parsed = match parse(source) {
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
                let (findings, preview) = compose_preview(&parsed, &received, context, default_program, &index);
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
    let labels = context.map(|context| context.labels).cloned().unwrap_or_default();
    outcome.canonical_source = Some(annotate(
        &print(&parsed.artifact, &DISPLAY),
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
