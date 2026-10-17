//! Tests of the syntax as a whole: grammar forms, predicates, diagnostics, the default program, and properties.
//!
//! Expected artifacts are written by hand as JSON, independently of the parser, and expected text is written by hand
//! independently of the printer.

use saluki_metric_plan::ir::PolicyArtifact;
use serde_json::Value;

use crate::{parse, parse_with, print, Diagnostic, ParseOptions, Parsed, PrintOptions};

mod errors;
mod fixture;
mod grammar;
mod predicates;
mod properties;

/// The base program hash the hand-written examples use.
const BASE: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// Returns a `post_enrich` policy with the header `base "<BASE>"`, then `statements`, each already indented as the
/// printer indents them and separated by blank lines, as they print.
fn policy(statements: &str) -> String {
    format!("policy post_enrich {{\n  base \"{}\"\n\n{}\n}}\n", BASE, statements)
}

/// Returns the artifact a hand-written JSON document describes, read the way the plan crate reads artifacts.
#[track_caller]
fn artifact(document: Value) -> PolicyArtifact {
    PolicyArtifact::from_value(document).expect("the expected artifact is a valid artifact document")
}

/// Returns the artifact a hand-written JSON document describes, without the checks of reading an artifact document,
/// for artifacts that are deliberately invalid.
#[track_caller]
fn raw_artifact(document: Value) -> PolicyArtifact {
    serde_json::from_value(document).expect("the expected artifact has the artifact's shape")
}

/// Parses `source`, failing with its diagnostics if it does not parse.
#[track_caller]
fn parse_ok(source: &str) -> Parsed {
    match parse(source) {
        Ok(parsed) => parsed,
        Err(diagnostics) => panic!("the source should parse:\n{}\n---\n{}", diagnostics, source),
    }
}

/// Returns the diagnostics of a source that must not parse.
#[track_caller]
fn parse_err(source: &str) -> Vec<Diagnostic> {
    match parse(source) {
        Ok(parsed) => panic!("the source should not parse:\n{}\n---\n{:?}", source, parsed.artifact),
        Err(diagnostics) => diagnostics.into_iter().collect(),
    }
}

/// Prints with the default options.
fn print_default(artifact: &PolicyArtifact) -> String {
    print(artifact, &PrintOptions::default())
}

/// Asserts that `source` parses to `expected` and, being in printed form, prints back unchanged.
#[track_caller]
fn assert_round_trip(source: &str, expected: &PolicyArtifact) {
    let parsed = parse_ok(source);
    assert_eq!(&parsed.artifact, expected, "parsing\n{}", source);
    let printed = print_default(&parsed.artifact);
    assert_eq!(printed, source, "printing gives back the source");
}

/// Returns `artifact` with its nodes and edges in canonical order, the order parsing writes.
fn canonical(artifact: PolicyArtifact) -> PolicyArtifact {
    let shapes = artifact
        .nodes
        .iter()
        .map(|node| crate::graph::NodeShape::of(&node.op, &node.params))
        .collect::<Vec<_>>();
    let order = crate::graph::canonical_order(&artifact.nodes, &artifact.edges, &shapes);
    PolicyArtifact {
        nodes: order.nodes.iter().map(|&i| artifact.nodes[i].clone()).collect(),
        edges: order.edges.iter().map(|&i| artifact.edges[i].clone()).collect(),
        ..artifact
    }
}

/// Asserts that `parsed`, an artifact parsed from the text `printed` of `export`, is the same program as `export` once
/// normalized and in canonical order: printing leaves out the parameters equal to their default, which normalization
/// fills back in, and parsing writes nodes and edges in canonical order, whatever order the export lists them in.
#[track_caller]
fn assert_same_program(parsed: &PolicyArtifact, export: &PolicyArtifact, printed: &str) {
    let normalize = saluki_metric_plan::normalize::normalize_artifact;
    assert_eq!(normalize(export), *export, "the export is in normal form");
    assert_eq!(canonical(normalize(parsed)), canonical(export.clone()), "{}", printed);
}

/// Returns the default program's export, normalized, as a JSON document, in the export's order.
pub(crate) fn default_export_document() -> Value {
    serde_json::from_str(include_str!(
        "../../../saluki-metric-plan/fixtures/export/default-program-metrics.json"
    ))
    .expect("the default program's export reads")
}

/// Returns the default program's export, normalized (see [`default_export_document`]).
pub(crate) fn default_export() -> PolicyArtifact {
    PolicyArtifact::from_value(default_export_document()).expect("the export is an artifact document")
}

/// Returns the source text a span covers.
fn text(source: &str, span: crate::Span) -> &str {
    &source[span.range()]
}

/// The stack that tests of stack use run on: half of what spawned threads get by default, so that what they check keeps
/// a margin on any thread.
///
/// A debug build needs the most. In one, parsing and printing the deepest inputs uses about 700 KB, and generating and
/// checking the property tests' inputs about 600 KB.
const SMALL_STACK: usize = 1024 * 1024;

/// Runs `test` on a thread with a [`SMALL_STACK`], failing if it fails.
fn on_small_stack<F>(test: F)
where
    F: FnOnce() + Send + 'static,
{
    std::thread::Builder::new()
        .stack_size(SMALL_STACK)
        .spawn(test)
        .expect("the test thread starts")
        .join()
        .expect("the test passes");
}
