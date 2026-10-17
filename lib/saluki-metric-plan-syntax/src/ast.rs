//! The syntax tree of a program, shared by the four passes: the parser makes one from text, lowering turns one into an
//! artifact, resugaring makes one from an artifact, and formatting writes one as text.
//!
//! The tree records what is written, not what it means: an `as` clause the author wrote, a parameter passed
//! positionally, a sink, but no derived node IDs, implicit tees, or resolved ports, which lowering works out. Parameter
//! values and predicates are already in the JSON shapes of the IR, as the parser has always made them.
//!
//! Spans locate each part in the source the parser read. A tree that resugaring made has no source, and every span in
//! it is [`Span::default`]; formatting never reads spans.

use std::collections::BTreeMap;

use saluki_metric_plan::{
    canonical::PlanHash,
    ir::{ArtifactNodeId, ExitName, OperatorName, OutputId, ParamName, Params, PortName, ScopeName},
};
use serde_json::{Map, Value};

use crate::diagnostic::Span;

/// A whole program: `policy <scope> { <header>* <statement>* }`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Program {
    /// `policy <scope>`.
    pub(crate) head: Span,
    pub(crate) scope: ScopeName,
    /// `ir_version <n>`, when written.
    pub(crate) ir_version: Option<u32>,
    /// `base "<hash>" [outputs { ... }]`, when written.
    pub(crate) base: Option<Base>,
    /// `blobs { "<hash>" = <value>, ... }`, when written.
    pub(crate) blobs: Option<Map<String, Value>>,
    pub(crate) statements: Vec<Statement>,
    /// The closing brace.
    pub(crate) close: Span,
}

/// The `base` statement.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Base {
    pub(crate) program_hash: PlanHash,
    pub(crate) outputs: BTreeMap<OutputId, PlanHash>,
}

/// A statement: a named stream, or a pipeline that ends in a sink.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Statement {
    /// `let <name> = <pipeline>`, where the pipeline has no end.
    Let { name: Name, pipeline: Pipeline, span: Span },
    /// `<pipeline>`, where the pipeline ends in a sink.
    Run { pipeline: Pipeline, span: Span },
}

/// A name the author wrote: a `let`'s name, an `as` clause's node ID, or a reference.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Name {
    pub(crate) id: ArtifactNodeId,
    pub(crate) span: Span,
}

/// `<head> | <stage> | ... [| <sink>]`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Pipeline {
    pub(crate) head: Head,
    pub(crate) stages: Vec<Stage>,
    pub(crate) end: Option<Sink>,
    pub(crate) span: Span,
}

/// Where a pipeline starts.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Head {
    /// A `let` or a scope input, perhaps with a port.
    Ref(Ref),
    /// `(<ref>, ...)`: a merge.
    Tuple(Tuple),
}

/// `<name>` or `<name>.<port>`: a `let`, or a scope input, which the name alone cannot tell apart until lowering looks
/// it up.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Ref {
    pub(crate) name: Name,
    pub(crate) port: Option<(PortName, Span)>,
    pub(crate) span: Span,
}

/// `(<ref>, ...) [as <id>]`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Tuple {
    pub(crate) items: Vec<Ref>,
    pub(crate) id: Option<Name>,
    pub(crate) span: Span,
}

/// One stage of a pipeline, between its head and its sink.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Stage {
    /// `<operator>[(<args>)] [as <id>]`.
    Op(OpStage),
    /// `where <predicate> [as <id>]`.
    Where(WhereStage),
}

/// `<operator>[(<args>)] [as <id>]`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OpStage {
    pub(crate) op: OperatorName,
    pub(crate) op_span: Span,
    /// Every parameter written, the positional one included, by name.
    pub(crate) params: Params,
    /// The parameter written first without its name, if one was. Its value is in `params`.
    pub(crate) positional: Option<ParamName>,
    /// The span of every parameter value, and of every value nested in one, by parameter path, as
    /// [`SpanMap::param`](crate::SpanMap::param) takes them.
    pub(crate) param_spans: Vec<(String, Span)>,
    pub(crate) id: Option<Name>,
    pub(crate) span: Span,
}

/// `where <predicate> [as <id>]`: a `split` on the predicate whose `false` port leads to `drop`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WhereStage {
    /// The predicate, in the JSON shape of the `split` operator's `predicate` parameter.
    pub(crate) predicate: Value,
    /// The span of the predicate, and of values nested in it, by parameter path from `predicate`.
    pub(crate) param_spans: Vec<(String, Span)>,
    pub(crate) id: Option<Name>,
    pub(crate) span: Span,
}

/// An inline terminal: `emit ...`, `drop`, or `exit <name>`, with an optional `as`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Sink {
    pub(crate) kind: SinkKind,
    pub(crate) id: Option<Name>,
    pub(crate) span: Span,
}

/// What a sink does.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SinkKind {
    /// `emit <recipient>` or `emit [<recipient>, ...]`: each name an identifier, such as `primary`, or a quoted string,
    /// such as an endpoint's URL. The `emit` operator's `to` parameter. Formatting writes one recipient without
    /// brackets.
    Recipients { names: Vec<String>, span: Span },
    /// `emit output <output>`: the `emit` operator's `output` parameter.
    Output { output: OutputId, span: Span },
    /// `drop`.
    Drop,
    /// `exit <name>`: an edge to one of the scope's exits, which makes no node.
    Exit { name: ExitName, span: Span },
}
