//! A readable surface syntax for policy artifacts.
//!
//! A [policy artifact](saluki_metric_plan::ir::PolicyArtifact) replaces one scope of a metrics plan. Its native form is
//! JSON, which is precise but hard to read and edit. This crate gives it a text form for people: [`parse`] turns text
//! into an artifact, with the [spans](SpanMap) of its parts, and [`print`](fn@print) turns an artifact back into text.
//! The syntax only builds the artifact: whether it is valid is decided by the plan crate's validator, whose findings
//! the span map points back to in the text.
//!
//! The syntax is a thin, easily changed layer. Its grammar has one generic form for every operator, driven by the
//! operator registry's parameter schemas, so a new operator or parameter needs no change here. It is for tools, such
//! as an editor or a demo server, and depends only on the plan crate: no crate that runs plans may depend on it, so
//! that changing it never changes how a plan runs.
//!
//! # A complete example
//!
// The example is kept in Markdown so that documentation checks read it as a code block, not as prose.
#![doc = include_str!("example.md")]
//!
//! # The grammar
//!
//! A source is one `policy` block. Whitespace and line breaks are insignificant, and `#` starts a comment that runs to
//! the end of the line.
//!
//! ```text
//! policy <scope> {
//!   base "<program hash>" [outputs { <output> = "<fingerprint>", ... }]
//!   input.<name> -> <target>
//!   node <id> = <operator>(<parameter>: <value>, ...) { <port> -> <target> ... }
//! }
//! ```
//!
//! - **`base`** records the hash of the program the artifact was derived from and, under `outputs`, the fingerprint
//!   of every output it refers to. It is required.
//! - **Edges** leave a scope input, as `input.metrics -> <target>`, or an output port of a node: inside the node's
//!   block, as `<port> -> <target>`, or as a statement of its own, `<node>.<port> -> <target>`. A node with exactly one
//!   output port may leave the port out: `-> <target>`.
//! - **Targets** are a node, as `<node>` or `<node>.<port>` (the port may be left out when the node has exactly one
//!   input port; a `merge` names its ports, as `joined.0`), one of the scope's exits, as `exit.aggregated`, where the
//!   edge leaves a scope that ends before its paths do (an exit is never an edge's source), or an inline terminal:
//!   `emit <output>` or `drop`. An inline
//!   terminal is a node of its own, with the ID [`derived_terminal_id`](saluki_metric_plan::ir::derived_terminal_id)
//!   gives it, `<parent>_<port>`: `split` port `true` leading to `drop` is the node `<split>_true`. For an edge from a
//!   scope input, the parent is `input`.
//! - **Nodes** are `node <id> = <operator>(<parameter>: <value>, ...)`, for any registered operator, followed by a
//!   block of edges if the node has any. An operator without parameters takes no parentheses. Two forms are shorter:
//!   - an operator whose only parameter is a predicate takes it without its name: `split(<predicate>) { true -> ...
//!     false -> ... }`;
//!   - `tee` without parentheses numbers its ports in the order of its edges: `tee { -> a -> b }` is
//!     `tee(ports: 2) { 0 -> a 1 -> b }`.
//! - **Optional statements**: `ir_version <n>` when the artifact is not written in the current IR version, and
//!   `blobs { "<hash>" = <value>, ... }` for an artifact's blob table.
//!
//! ## Values
//!
//! | Syntax | JSON |
//! |---|---|
//! | `"text"`, with escapes `\"`, `\\`, `\n`, `\r`, `\t`, `\u{1f600}` | a string |
//! | `42`, `-7`, `1.5` | a number |
//! | `10s`, `500ms`, `2m` (units `h`, `m`, `s`, `ms`, `us`, `ns`) | the duration's string, as written |
//! | `true`, `false`, `null` | the same |
//! | `[<value>, ...]` | an array |
//! | `{ <name>: <value>, ... }`, with names bare or quoted | an object |
//! | `config("dogstatsd.metric_filter.values")` | a configuration binding, `{"config": "..."}` |
//! | `blob("sha256:...")` | a blob reference, `{"blob": "..."}` |
//!
//! Values are converted as written; the operator's parameter schema is used only to know where a predicate is
//! expected, and, when printing, to write durations bare and record members in their declared order. Strings cannot
//! contain line breaks, control characters, or invisible formatting characters as they are, only as escapes, and
//! comments cannot contain invisible formatting characters, so the text always shows what it says.
//!
//! ## Predicates
//!
//! Predicates combine leaves with `and`, `or`, `not`, and parentheses; `not` binds tightest and `or` loosest.
//! `all(<predicate>, ...)` and `any(<predicate>, ...)` write junctions of any number of operands.
//!
//! | Leaf | Example |
//! |---|---|
//! | Name, intake name, or host | `name == "s"`, `host starts_with "s"`, `name in ["a"]`, `name prefix_in ["a."]` |
//! | Set match | `intake_name matches(values: config("dogstatsd.metric_filter.values"), prefix: false)` |
//! | Unset host | `host is unset` |
//! | Tag key | `has_tag("env")`, `has_origin_tag("pod")` |
//! | Tag value | `tag("team") == "platform"`, `origin_tag("pod") starts_with "web-"`, and the other string matches |
//! | Kind | `kind in [counter, rate, gauge, set, histogram, distribution]` |
//! | Constant | `true`, `false` |
//! | Flag and empty list | `flag(config("multi_region_failover.enabled"))`, `is_empty(config("..."))`, `is_empty([])` |
//!
//! # Printing
//!
//! [`print`](fn@print) writes nodes in topological order from the scope inputs, each with the edges that leave it, and
//! writes a terminal inline wherever its ID is the derived one. A node whose header is longer than the line width (120
//! columns by default) puts each parameter on its own line, and long predicates, set matches, and lists break the same
//! way. Output is deterministic: it depends only on the artifact's graph.
//!
//! Parsing writes nodes and edges in the same canonical order, whatever order the text gives them in, so the
//! artifact, and its hash, change only when its graph does. The order is a depth-first traversal from the scope
//! inputs, in which a node comes right after the edge that completes its inputs and a node's edges follow its
//! operator's port order: the order lowering writes plans in. As a result:
//!
//! - `parse(print(a))` is `a` for every valid artifact in canonical order, such as every artifact parsed from text and
//!   the exports of the default program's regions. For a valid artifact in another order, as a tool writing JSON
//!   might give it, it is `a` with its nodes and edges reordered, which is a different document with a different hash,
//!   so tools that write artifacts should write them in canonical order. Invalid artifacts print too, and parse back
//!   the same way unless they define a node twice, refer to a node or port they do not have, or have a bare boolean as
//!   a whole predicate, which has no text of its own (where a predicate is expected, `true` is the constant leaf;
//!   inside a junction, a bare boolean is written raw); parsing reports the first two.
//! - `print(parse(s))` is a fixed point: printing what it parses to gives it back.
//!
//! With [`PrintOptions::elide_fingerprints`], the `outputs` of `base` are left out, for display; the text then parses
//! to the artifact without them.
//!
//! # Limits
//!
//! [`parse`] rejects, with a diagnostic and without exhausting the stack, a source longer than [`MAX_SOURCE_SIZE`]
//! bytes, with more than [`MAX_TOKENS`] tokens, or nesting values or predicates more than [`MAX_DEPTH`] levels deep,
//! and an artifact whose JSON is longer than the artifact size limit ([`ARTIFACT_SIZE`] bytes). Its time is
//! proportional to the length of the source, apart from sorting each node's edges into port order.
//!
//! The text of an artifact within the size limit always parses back. Text is usually shorter than JSON (the default
//! program's policy scope prints in about 60% of its compact JSON, since inline terminals take no definitions), and
//! has fewer tokens than its JSON has bytes. Breaking lines can make it longer: printing puts each element of a long
//! list on its own line, indented as deeply as the list is nested. When that would make the text longer than parsing
//! accepts, every value is printed on one line instead, which takes at most six times the bytes of its JSON.
#![deny(warnings)]
#![deny(missing_docs)]

use saluki_metric_plan::{ir::PolicyArtifact, limits::values::ARTIFACT_SIZE};

mod build;
mod diagnostic;
mod graph;
mod lexer;
mod parser;
mod printer;
mod spans;

pub use self::{
    diagnostic::{Diagnostic, Diagnostics, Span},
    spans::SpanMap,
};

/// The longest a source may be, in bytes: eight times the [artifact size limit](ARTIFACT_SIZE).
///
/// Text is usually shorter than JSON, but an artifact made of strings of characters that are written as escapes, such
/// as U+007F, which takes one byte in JSON and six as `\u{7f}`, prints up to six times longer.
pub const MAX_SOURCE_SIZE: usize = 8 * ARTIFACT_SIZE as usize;

/// The most tokens a source may have: one per byte of the [artifact size limit](ARTIFACT_SIZE).
pub const MAX_TOKENS: usize = lexer::MAX_TOKENS;

/// The deepest that lists, records, predicates, and parenthesized expressions may nest in a source.
///
/// Parsing and printing a source nested this deeply uses about 700 KB of stack in a debug build, and about 220 KB in an
/// optimized one, well within the 2 MiB that spawned threads get by default.
pub const MAX_DEPTH: usize = parser::MAX_DEPTH;

/// A parsed artifact, with the source spans of its parts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Parsed {
    /// The artifact.
    pub artifact: PolicyArtifact,

    /// Where each node, parameter value, and edge of the artifact is in the source.
    pub spans: SpanMap,
}

/// How [`print`](fn@print) writes an artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrintOptions {
    /// Whether to leave out the fingerprints of the outputs in `base`.
    ///
    /// Fingerprints are long digests that matter to the installer, not to a reader. Text printed without them parses
    /// to an artifact without them, so a tool that elides them for display restores them before submitting an edited
    /// artifact.
    ///
    /// Defaults to `false`: fingerprints are printed.
    pub elide_fingerprints: bool,

    /// The line width, in characters, beyond which node headers, predicates, and values are broken across lines.
    ///
    /// Width is a target, not a limit: a single string or identifier longer than the width is never broken, and an
    /// artifact whose text breaking lines would make longer than parsing accepts is printed with every value on one
    /// line (see [Limits](crate#limits)).
    ///
    /// Defaults to 120.
    pub width: usize,
}

impl Default for PrintOptions {
    fn default() -> Self {
        Self {
            elide_fingerprints: false,
            width: 120,
        }
    }
}

/// Parses a policy artifact from its surface syntax.
///
/// The artifact's nodes and edges are in canonical order (see [Printing](crate#printing)), so equal graphs give equal
/// artifacts however their text is arranged.
///
/// Parsing only builds the artifact. It does not check what reading an artifact document checks, such as that blobs
/// are stored under their hashes, nor anything validation checks, such as that parameters match their operator's
/// schema: serialize the artifact and read it with
/// [`PolicyArtifact::from_value`](saluki_metric_plan::ir::PolicyArtifact::from_value), then validate the plan it is
/// composed into, and use [`Parsed::spans`] to place the findings in the source.
///
/// # Errors
///
/// Returns diagnostics if the source is over the size or token limits, nests too deeply, does not follow the grammar,
/// refers to something it does not define (an undefined node, a port the node's operator does not have, an edge line
/// without a port on a node with several output ports, or an inline terminal whose derived ID another node already
/// has), or describes an artifact over the artifact size limit. A syntax error ends parsing, so it is reported alone,
/// after a note naming the operator when it is in the arguments of an operator that is not registered, which are read
/// without a schema; reference problems are all reported together.
pub fn parse(source: &str) -> Result<Parsed, Diagnostics> {
    if source.len() > MAX_SOURCE_SIZE {
        let mut start = MAX_SOURCE_SIZE;
        while !source.is_char_boundary(start) {
            start -= 1;
        }
        let error = diagnostic::Error::new(
            Span::new(start, source.len()),
            format!(
                "The source is {} bytes, more than the {} bytes a policy's text may have.",
                source.len(),
                MAX_SOURCE_SIZE
            ),
        );
        return Err(diagnostic::diagnostics(source, vec![error]));
    }
    let tokens = lexer::lex(source).map_err(|error| diagnostic::diagnostics(source, vec![error]))?;
    let policy = parser::parse_policy(source, &tokens).map_err(|errors| diagnostic::diagnostics(source, errors))?;
    let (artifact, spans) = build::build(source, policy).map_err(|errors| diagnostic::diagnostics(source, errors))?;
    Ok(Parsed { artifact, spans })
}

/// Prints a policy artifact in its surface syntax.
///
/// The output parses back to the same artifact, with its nodes and edges in canonical order (see
/// [Printing](crate#printing)), unless `options` elides fingerprints. Printing never fails: an invalid artifact, such
/// as one with an edge to a node it does not have, prints as well as it can, and parsing the text reports the
/// problem.
pub fn print(artifact: &PolicyArtifact, options: &PrintOptions) -> String {
    printer::print(artifact, options)
}

#[cfg(test)]
mod tests;
