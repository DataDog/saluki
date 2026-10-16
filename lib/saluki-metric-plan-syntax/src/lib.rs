//! A readable surface syntax for policy artifacts: programs written as streaming queries.
//!
//! A [policy artifact](saluki_metric_plan::ir::PolicyArtifact) is a program for the metrics path. Its native form is
//! JSON, which is precise but hard to read and edit. This crate gives it a text form for people, in the style of Unix
//! pipes and PRQL: [`parse`] turns text into an artifact, with the [spans](SpanMap) of its parts, and
//! [`print`](fn@print) turns an artifact back into the text a person would write for it. The syntax only builds the
//! artifact: whether it is valid is decided by the plan crate's validator, whose findings the span map points back to in
//! the text. The [design document](../../../docs/design/programmable-metrics-pipeline-syntax.md) gives the reasoning.
//!
//! The syntax is a thin, easily changed layer. Every operator is written the same way, driven by the operator
//! registry's parameter schemas, so a new operator or parameter needs no change here. It is for tools, such as an editor
//! or a demo server, and depends only on the plan crate: no crate that runs plans may depend on it, so that changing it
//! never changes how a plan runs.
//!
//! # A complete example
//!
// The example is kept in Markdown so that documentation checks read it as a code block, not as prose.
#![doc = include_str!("example.md")]
//!
//! # The language
//!
//! A source is one `policy` block. Whitespace and line breaks are insignificant, except that an operator's arguments
//! follow its name directly, as `op(...)`, `#` starts a comment that runs to the end of the line, and a stream named
//! `as` starts a statement only at the start of a line (elsewhere, `as |` is an `as` missing its name).
//!
//! ```text
//! policy <scope> {
//!   [base "<program hash>" [outputs { <output> = "<fingerprint>", ... }]]
//!   let <name> = <head> | <stage> | ...           # a named stream
//!   <head> | <stage> | ... | <sink>               # a pipeline that ends in a sink, or in a fork
//! }
//! ```
//!
//! - A **head** is a stream: a scope input (`dogstatsd`), a `let` (`checkout`), one output port of a node with several
//!   (`ts.passthrough`), or a tuple of streams, which merges them (`(a, b, c)`).
//! - A **stage** is any registered operator, `op` or `op(<positional>, <parameter>: <value>, ...)`, or `where
//!   <predicate>`, which keeps what matches. An operator's positional parameter may be written first without its name,
//!   and a parameter with a default may be left out. A word with arguments right after it is always an operator, so an
//!   `emit` whose parameters no sink writes is `emit(output: "o", to: [...])`, and an operator named like a sink,
//!   `where`, `fork`, or `as` is written with parentheses even without arguments, as in `drop()`.
//! - A **sink** ends a pipeline: `emit primary`, `emit [primary, "https://b.example.com"]`, `emit output <output>`,
//!   `drop`, or `exit <name>` where the pipeline leaves a scope that ends early. `fork { <branch> ... }` also ends a
//!   pipeline: it copies the stream to each branch, which is stages then a sink or another fork.
//! - A **stream used more than once** is copied to each use. Copying is how a stream reaches several places, so a sink
//!   in the middle of a pipeline is an error: name the stream with `let` and use it twice.
//! - **`as <id>`** after any stage, tuple, fork, or sink gives its node an ID of its own instead of the one derived from
//!   where it is written ([the derived IDs](../../../docs/design/programmable-metrics-pipeline-syntax.md#node-ids)).
//!   State carries over a change by node ID, so an aggregation's ID is worth keeping.
//! - **`base`** records the hash of the program the artifact was derived from and, under `outputs`, the fingerprint of
//!   every output it refers to. It is required, unless the tool that parses the text gives the base instead
//!   ([`ParseOptions::base`]). `ir_version <n>` and `blobs { "<hash>" = <value>, ... }` are optional headers.
//!
//! The syntax enforces what can be checked where it is written: a `let` is used only after it is defined, so a program
//! cannot loop; every stream and every output port is used; a node with several output ports is always used through one
//! of them; and node IDs are unique.
//!
//! ## The core, and the sugar
//!
//! | Form | What it builds |
//! |---|---|
//! | `e \| op(args)` | a node running `op`, fed by `e` |
//! | `(a, b, ...)` | a `merge`, with `a` at port `0`, `b` at port `1`, and so on |
//! | a stream used `n > 1` times | a `tee(ports: n)`, whose port `i` feeds the `i`th use in the text |
//! | `emit ...`, `drop` | an inline `emit` or `drop` node |
//! | `e \| where p` | `split(p)`, whose `false` port leads to `drop` and whose `true` port continues |
//! | `e \| fork { t1 ... tn }` | `tee(ports: n)`, whose port `i` leads to branch `i` |
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
//! | `config.dogstatsd.metric_filter.values` | a configuration binding, `{"config": "..."}` |
//! | `blob("sha256:...")` | a blob reference, `{"blob": "..."}` |
//!
//! Values are converted as written; the operator's parameter schema is used only to know where a predicate or the
//! positional parameter is expected, and, when printing, to leave out defaults and write record members in their
//! declared order. Strings cannot contain line breaks, control characters, or invisible formatting characters as they
//! are, only as escapes, and comments cannot contain invisible formatting characters, so the text always shows what it
//! says.
//!
//! ## Predicates
//!
//! Predicates combine leaves with `and`, `or`, `not`, and parentheses; `not` binds tightest and `or` loosest.
//!
//! | Leaf | Example |
//! |---|---|
//! | Name, intake name, or host | `name == "s"`, `name != "s"`, `host starts_with "s"`, `name in ["a"]`, `name starts_with ["a.", "b."]` |
//! | A bound set of names or prefixes | `intake_name in config.dogstatsd.metric_filter.values`, `name starts_with config.x` |
//! | Host | `host is unset`, `host is set` |
//! | Tag key | `tags.env is set`, `tags["app.kubernetes.io/name"] is unset`, `origin_tags.pod is set` |
//! | Tag value | `tags.team == "platform"`, `origin_tags.pod starts_with "web-"`, and the other string matches |
//! | Kind | `kind in [counter, rate, gauge, set, histogram, distribution]` |
//! | Constant | `true`, `false` |
//! | Flag and empty list | `config.multi_region_failover.enabled`, `is_empty(config.x)`, `is_empty([])` |
//!
//! Every predicate has exactly one printed spelling. A few shapes of the IR have none of their own, such as a junction
//! of one operand, and print as their JSON, which parses back the same. So does a predicate parameter whose value is not
//! a predicate at all, except a bare `true` or `false`, which can only be read as the constant.
//!
//! # Printing
//!
//! [`print`](fn@print) recognizes sugar in the artifact's graph and writes it wherever parsing it back gives exactly the
//! same graph, and the core form anywhere else. Output is deterministic: it depends only on the graph. Parsing writes
//! nodes and edges in a canonical order, a depth-first traversal from the scope inputs in which a node comes right after
//! the edge that completes its inputs and a node's edges follow its operator's port order, whatever order the text gives
//! them in, so the artifact, and its hash, change only when its graph does. As a result:
//!
//! - `parse(print(a))` is `a` for every well-formed artifact in canonical order, apart from its parameters equal to
//!   their registry default, which printing leaves out. A well-formed artifact is one whose node IDs are unique and
//!   whose edges name nodes and ports that exist, with one edge into each input port and at most one out of each output
//!   port and each scope input, no cycles, every node reached from a scope input, and every output port used (a node
//!   whose operator is not registered, or whose parameters do not configure it, takes the input `in` and whatever output
//!   ports its edges leave, at least one). An artifact in another order, as a tool writing JSON might give it, parses
//!   back with its nodes and edges reordered.
//! - Any other artifact still prints, as well as it can, and its text never parses back to it, since parsing makes only
//!   well-formed artifacts. Parsing reports most problems; the text cannot write an edge into an input port other than
//!   a node's only one (or a merge's numbered ones), out of an exit, into a scope input, or between nodes the artifact
//!   does not have, so printing moves such an edge or leaves it out, and a node that no edge enters reads from the scope
//!   input of its name.
//! - `print(parse(s))` is a fixed point: printing what it parses to gives it back.
//!
//! A `let` is named for the node it ends, except where the node's ID is the name of a scope input the artifact reads,
//! which a reference to the input would then name: such a `let` is named `<id>_let`, and its last stage has `as <id>`.
//!
//! The plan crate's [normalization](saluki_metric_plan::normalize) puts equivalent programs into one form, such as a
//! filter that drops on `true` into one that drops on `false`, so that they print the same way. It also fills in every
//! default, so the text of an artifact in normal form, as the default program and its export are, parses to the same
//! artifact once normalized again.
//!
//! With [`PrintOptions::elide_fingerprints`], the `outputs` of `base` are left out, for display; the text then parses
//! to the artifact without them. With [`PrintOptions::elide_base`], the whole `base` statement is left out, as a tool
//! that keeps the base apart from the text does; the text then parses with the base the tool gives
//! ([`parse_with`]).
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

use saluki_metric_plan::{
    ir::{ArtifactBase, PolicyArtifact},
    limits::values::ARTIFACT_SIZE,
};

mod ast;
mod diagnostic;
mod format;
mod graph;
mod ids;
mod lexer;
mod lower;
mod parser;
mod resugar;
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

    /// Whether the artifact's base came from the caller ([`ParseOptions::base`]), the source having no `base`
    /// statement, rather than from the source.
    ///
    /// A tool that hides the base when printing the source back ([`PrintOptions::elide_base`]) hides it only then, so
    /// that a base the author wrote stays in the text.
    pub base_given: bool,
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

    /// Whether to leave out the `base` statement, fingerprints included.
    ///
    /// A detached program's base only tells its host whether the program it was written against has changed since:
    /// a tool that keeps it apart from the text, as an editor does with the export it loaded, hides it from a reader,
    /// and gives it back when it parses the text ([`ParseOptions::base`]).
    ///
    /// Defaults to `false`: the base is printed.
    pub elide_base: bool,

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
            elide_base: false,
            width: 120,
        }
    }
}

/// How [`parse_with`] reads a source.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ParseOptions {
    /// The base an artifact whose text has no `base` statement records, as a tool that printed the text without it
    /// ([`PrintOptions::elide_base`]) gives it back.
    ///
    /// A source with a `base` statement records that one. Defaults to `None`: a source must have a `base` statement.
    pub base: Option<ArtifactBase>,
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
/// breaks one of the rules the syntax enforces (a name used before it is defined or defined twice, a stream or output
/// port never used, a node with several output ports used without a port, a port a node does not have, or a derived
/// node ID another node already has), or describes an artifact over the artifact size limit. A syntax error ends
/// parsing, so it is reported alone, after a note naming the operator when it is in the arguments of an operator that
/// is not registered, which are read without a schema; the other problems are all reported together.
pub fn parse(source: &str) -> Result<Parsed, Diagnostics> {
    parse_with(source, &ParseOptions::default())
}

/// Parses a policy artifact from its surface syntax, as [`parse`] does, with `options`: a source without a `base`
/// statement records the base `options` gives, if it gives one.
///
/// # Errors
///
/// As for [`parse`].
pub fn parse_with(source: &str, options: &ParseOptions) -> Result<Parsed, Diagnostics> {
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
    let program = parser::parse_program(source, &tokens).map_err(|errors| diagnostic::diagnostics(source, errors))?;
    let base_given = program.base.is_none() && options.base.is_some();
    let (artifact, spans) = lower::lower(source, program, options.base.as_ref())
        .map_err(|errors| diagnostic::diagnostics(source, errors))?;
    Ok(Parsed {
        artifact,
        spans,
        base_given,
    })
}

/// Prints a policy artifact in its surface syntax.
///
/// The output of a well-formed artifact parses back to the same artifact, with its nodes and edges in canonical order
/// and without the parameters equal to their default (see [Printing](crate#printing)), unless `options` elides
/// fingerprints or the base. Printing never fails: an artifact that is not well-formed, such as one with an edge to a
/// node it does not have, prints as well as it can, and its text never parses back to it.
pub fn print(artifact: &PolicyArtifact, options: &PrintOptions) -> String {
    let program = resugar::resugar(artifact, options);
    let text = format::format(&program, options.width);
    // Every token takes at least one byte, so only a long text can have too many.
    if text.len() <= MAX_SOURCE_SIZE && (text.len() <= MAX_TOKENS || lexer::lex(&text).is_ok()) {
        return text;
    }
    format::format(&program, usize::MAX)
}

#[cfg(test)]
mod tests;
