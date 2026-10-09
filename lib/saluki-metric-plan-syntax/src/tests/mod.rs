//! Tests of the syntax as a whole: grammar forms, predicates, diagnostics, the default program, and properties.
//!
//! Expected artifacts are written by hand as JSON, independently of the parser, and expected text is written by hand
//! independently of the printer.

use saluki_metric_plan::ir::PolicyArtifact;
use serde_json::Value;

use crate::{parse, print, Diagnostic, Parsed, PrintOptions};

mod errors;
mod fixture;
mod grammar;
mod predicates;
mod properties;

/// The base program hash the hand-written examples use.
const BASE: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// Returns a `post_enrich` policy with [`BASE`] and the given items, in printed form.
fn policy(items: &str) -> String {
    format!("policy post_enrich {{\n  base \"{}\"\n\n{}\n}}\n", BASE, items)
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
