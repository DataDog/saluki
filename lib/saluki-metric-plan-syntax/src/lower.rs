//! Lowering: a [syntax tree](crate::ast) to the artifact it describes, with the spans of its parts.
//!
//! Lowering is the core of the [pipeline syntax](../../../docs/design/programmable-metrics-pipeline-syntax.md): every
//! form becomes nodes and edges of the IR, and every piece of sugar is lowered as the one rewrite into the core that
//! defines it.
//!
//! # What each form becomes
//!
//! - A name that no `let` defines is a scope input, and a reference to it is an edge from `input.<name>`.
//! - `e | op(args)` is a node running `op`, with an edge from `e` into its only input port.
//! - `x.port` is output port `port` of the node that ends `x`, which must have several output ports.
//! - `(a, b, ...)` is a `merge(inputs: n)`, with item `i` at its input port `i`.
//! - A stream used `n > 1` times is followed by a `tee(ports: n)`, whose port `i` feeds the `i`th use in source order:
//!   by the start of the reference's span, and in the order the tree lists references where two start together.
//! - `e | where p` is a `split(predicate: p)` whose `false` port leads to a `drop` terminal; the pipeline continues
//!   from its `true` port.
//! - `emit <recipients>` and `emit output <output>` are `emit` nodes with the `to` or `output` parameter, `drop` is a
//!   `drop` node, and `exit <name>` is an edge to the scope's exit `exit.<name>`, which makes no node.
//!
//! # Node IDs
//!
//! A node written with `as <id>` has that ID. Every other node's ID is derived with the functions of [`ids`], which
//! resugaring compares against, so the two passes agree:
//!
//! - In `let x = s1 | ... | sn`, the last stage's node is `x`, and each earlier stage `si` is
//!   [`ids::stage`]`(x, <operator>, <ordinal>)`. A tuple head is [`ids::merge`]`(x)`, or `x` itself when the pipeline
//!   has no stages.
//! - In a statement without `let`, the anchor is the port the pipeline starts from: [`ids::input_anchor`] for a scope
//!   input, [`ids::port_anchor`] for a `let` (with its port, if written), or [`ids::tee_port`] of the implicit tee and
//!   the use's port when the stream is used more than once. A tuple head's merge is [`ids::merge`]`(<anchor of its
//!   first item>)`, and the ID of the merge, written or derived, is then the anchor. Each stage is
//!   [`ids::stage`]`(anchor, <operator>, <ordinal>)`.
//! - The implicit tee after a stream used more than once is [`ids::fanout`]`(<node>, <port>)`, where the node is the
//!   one that ends the `let` (whatever its ID) and the port is the one the references write, if any. After a scope
//!   input `i`, it is [`ids::fanout`]`(input, i)`, that is `input_<i>_fanout`.
//! - A sink is [`ids::terminal`]`(<parent>, <port>)`, named after the node and output port the edge into it leaves
//!   (`input` and the input's name for a scope input).
//!
//! Ordinals count the stages of one pipeline by operator, from 1, in pipeline order. Every stage counts, including
//! those named with `as` and the last stage of a `let`, so naming one stage never changes the ID of another. A `where`
//! counts as its operator, [`ids::WHERE_OPERATOR`].
//!
//! # Rules
//!
//! Lowering enforces the syntax's rules, and reports every problem it finds together: a name used before its `let`
//! (or by its own `let`), two names that collide (two `let`s, a `let` and an `as`, two `as` clauses, or a derived ID
//! and any other node's ID), a `let` with no stages, an `as` on an `exit`, an unused `let` or output port, a bare
//! reference to a node with several output ports, a reference to a port that does not exist, and a pipeline that
//! continues from a node with several output ports, or none.

use std::{
    collections::{HashMap, HashSet},
    io,
};

use saluki_metric_plan::{
    ir::{
        ArtifactBase, ArtifactEdge, ArtifactNode, ArtifactNodeId, ArtifactPortRef, Blobs, InputName, OperatorName,
        ParamName, Params, PolicyArtifact, PortName, IR_VERSION, SCOPE_INPUT_NODE,
    },
    limits::values::ARTIFACT_SIZE,
    operators::{names, EMIT_TO, FALSE_PORT, TRUE_PORT},
};
use serde_json::Value;

use crate::{
    ast::{Head, Name, OpStage, Pipeline, Program, Ref, Sink, SinkKind, Stage, Statement, Tuple},
    diagnostic::{Error, LineIndex, Span},
    graph::{canonical_order, NodeShape},
    ids::{self, Ordinals},
    spans::SpanMap,
};

/// Lowers a program to its artifact, in canonical order, with the span of every node, parameter value, and edge.
///
/// A program without a `base` statement records `default_base`, if given.
///
/// Spans: a node's span is its stage, tuple, or sink; a `where`'s drop terminal has the `where`'s span; an implicit tee
/// has the span of the `let`'s name, or for a scope input, of its first use. An edge's span is that of the element it
/// enters: a stage, a sink, a tuple item, or for the edge into an implicit tee, the tee's.
///
/// # Errors
///
/// Returns every problem found: a missing `base` without a default, a name defined twice or used before it is
/// defined, an unused stream or port, a node with several output ports used without a port, a derived ID that collides
/// with another node's, and any reference to a port a node does not have. If there are none, returns an error if the
/// artifact's JSON is longer than the artifact size limit.
pub(crate) fn lower(
    source: &str, program: Program, default_base: Option<&ArtifactBase>,
) -> Result<(PolicyArtifact, SpanMap), Vec<Error>> {
    let mut lowering = Lowering {
        source,
        lines: LineIndex::new(source),
        entries: Vec::new(),
        by_id: HashMap::new(),
        edges: Vec::new(),
        names: HashMap::new(),
        lets: HashMap::new(),
        uses: HashMap::new(),
        ranks: Vec::new(),
        tees: HashMap::new(),
        seq: 0,
        pipeline_start: 0,
        reported: HashSet::new(),
        errors: Vec::new(),
    };

    let base = match (program.base, default_base) {
        (Some(base), _) => Some(ArtifactBase {
            program_hash: base.program_hash,
            outputs: base.outputs,
        }),
        (None, Some(base)) => Some(base.clone()),
        (None, None) => {
            lowering.errors.push(Error::new(
                program.close,
                "Expected `base \"<program hash>\"` in the policy: an artifact records the hash of the program it was \
                 written against.",
            ));
            None
        }
    };

    lowering.scan(&program.statements);
    // Every port index a stream's copies or a tuple's items take must be a valid port name, so a program wider than
    // that stops here, before any node is made.
    if !lowering.check_widths(&program.statements) {
        return Err(lowering.errors);
    }
    for statement in &program.statements {
        lowering.statement(statement);
    }
    lowering.check_unused(&program.statements);

    let Some(base) = base else {
        return Err(lowering.errors);
    };
    if !lowering.errors.is_empty() {
        return Err(lowering.errors);
    }

    let blobs = match program.blobs {
        None => Blobs::default(),
        Some(blobs) => serde_json::from_value(Value::Object(blobs))
            .map_err(|error| vec![Error::new(program.close, format!("Invalid blob table: {}", error))])?,
    };

    let (nodes, shapes): (Vec<_>, Vec<_>) = lowering
        .entries
        .iter()
        .map(|entry| (entry.node.clone(), entry.shape.clone()))
        .unzip();
    let (edges, edge_spans): (Vec<_>, Vec<_>) = lowering.edges.into_iter().unzip();
    let order = canonical_order(&nodes, &edges, &shapes);

    let mut spans = SpanMap::default();
    for &index in &order.nodes {
        let entry = &lowering.entries[index];
        let id = entry.node.id.as_str();
        spans.insert_node(id, entry.span);
        for (path, span) in &entry.param_spans {
            spans.insert_param(id, path.clone(), *span);
        }
    }
    for &index in &order.edges {
        spans.push_edge(edge_spans[index]);
    }

    let artifact = PolicyArtifact {
        ir_version: program.ir_version.unwrap_or(IR_VERSION),
        scope: program.scope,
        base,
        nodes: order.nodes.iter().map(|&index| nodes[index].clone()).collect(),
        edges: order.edges.iter().map(|&index| edges[index].clone()).collect(),
        blobs,
    };
    check_size(&artifact, program.head)?;
    Ok((artifact, spans))
}

/// Checks that the artifact's JSON, written compactly, is no longer than the artifact size limit, reporting it at
/// `head`, the start of the policy, if it is longer.
fn check_size(artifact: &PolicyArtifact, head: Span) -> Result<(), Vec<Error>> {
    /// Counts the bytes written to it.
    struct Counter(u64);

    impl io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 += bytes.len() as u64;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, artifact).map_err(|error| {
        vec![Error::new(
            head,
            format!("The artifact cannot be written as JSON: {}", error),
        )]
    })?;
    if counter.0 > ARTIFACT_SIZE {
        let message = format!(
            "The artifact is {} bytes as JSON, more than the artifact size limit of {} bytes.",
            counter.0, ARTIFACT_SIZE
        );
        return Err(vec![Error::new(head, message)]);
    }
    Ok(())
}

/// One node of the artifact being built, with where it came from.
struct Entry {
    node: ArtifactNode,
    shape: NodeShape,
    span: Span,
    param_spans: Vec<(String, Span)>,
}

/// What a name the author wrote names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NameKind {
    Let,
    As,
}

/// A `let`, as the scan found it and lowering filled it in.
struct Let {
    /// The position of its statement.
    statement: usize,
    span: Span,
    /// The stream it names, once lowered, unless lowering it failed.
    stream: Option<Stream>,
    /// Whether a reference to it without a port was reported as needing one, which makes its unused ports moot.
    bare_error: bool,
}

/// The stream a pipeline has reached: an output of the node `node`, either its only one, or `port` when the stream
/// is fixed to one port (the `true` port of a `where`).
#[derive(Clone, Debug)]
struct Stream {
    node: usize,
    port: Option<PortName>,
}

/// The output port an edge leaves, with the parent and port that name an inline terminal it enters.
#[derive(Clone, Debug)]
struct Source {
    from: ArtifactPortRef,
    parent: String,
    port: PortName,
}

/// What a reference refers to, which also keys its uses.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum Target {
    Let(String),
    Input(String),
}

/// A stream and port that references use: the target, and the port they write, if any.
type UseKey = (Target, Option<PortName>);

/// A reference, as the scan recorded it.
struct Use {
    seq: usize,
    span: Span,
}

/// The state of lowering one program.
struct Lowering<'s> {
    source: &'s str,
    lines: LineIndex,
    entries: Vec<Entry>,
    by_id: HashMap<String, usize>,
    edges: Vec<(ArtifactEdge, Span)>,
    /// Every name the author wrote, `let`s and `as` clauses, with its span and kind.
    names: HashMap<String, (Span, NameKind)>,
    lets: HashMap<String, Let>,
    /// Every reference to each stream and port, in source order.
    uses: HashMap<UseKey, Vec<Use>>,
    /// For each reference, by its position in the tree, its key, its rank among the uses of its key in source order,
    /// and how many uses its key has; `None` for a reference that refers to nothing.
    ranks: Vec<Option<(UseKey, usize, usize)>>,
    /// The implicit tee of each stream used more than once, once made.
    tees: HashMap<UseKey, Option<ArtifactNodeId>>,
    /// The position of the next reference, as lowering meets them in the order the scan did.
    seq: usize,
    /// Where the pipeline of the statement being lowered starts, which a suggestion to name part of it quotes.
    pipeline_start: usize,
    /// The names and node IDs already reported as taken twice, which are not reported again.
    reported: HashSet<String>,
    errors: Vec<Error>,
}

impl Lowering<'_> {
    fn position(&self, span: Span) -> String {
        let (line, column) = self.lines.line_column(self.source, span.start);
        format!("line {}, column {}", line, column)
    }

    fn error(&mut self, span: Span, message: impl Into<String>) {
        self.errors.push(Error::new(span, message));
    }

    // The scan: names, and every reference, before any node is made.

    /// Records every name the program defines and every reference it makes, reporting names that collide and
    /// references to a `let` that is not yet defined. The implicit tees and the anchors of reused streams depend on how
    /// many times each stream is used, so this happens before lowering makes any node.
    fn scan(&mut self, statements: &[Statement]) {
        for (index, statement) in statements.iter().enumerate() {
            if let Statement::Let { name, .. } = statement {
                if self.define(name, NameKind::Let) {
                    self.lets.insert(
                        name.id.to_string(),
                        Let {
                            statement: index,
                            span: name.span,
                            stream: None,
                            bare_error: false,
                        },
                    );
                }
            }
            let (pipeline, own) = match statement {
                Statement::Let { name, pipeline, .. } => (pipeline, Some(name.id.as_str())),
                Statement::Run { pipeline, .. } => (pipeline, None),
            };
            self.scan_names(pipeline, own);
        }

        let mut seq = 0;
        let mut ranks = Vec::new();
        for (index, statement) in statements.iter().enumerate() {
            for reference in head_refs(&statement_pipeline(statement).head) {
                let key = self.scan_ref(index, reference);
                if let Some(key) = &key {
                    self.uses.entry(key.clone()).or_default().push(Use {
                        seq,
                        span: reference.span,
                    });
                }
                ranks.push(key);
                seq += 1;
            }
        }
        let mut rank_of = HashMap::new();
        for uses in self.uses.values_mut() {
            // Source order, by span start; the order of the tree breaks ties, as the sort is stable.
            uses.sort_by_key(|reference| reference.span.start);
            for (rank, reference) in uses.iter().enumerate() {
                rank_of.insert(reference.seq, (rank, uses.len()));
            }
        }
        self.ranks = ranks
            .into_iter()
            .enumerate()
            .map(|(seq, key)| key.map(|key| (key, rank_of[&seq].0, rank_of[&seq].1)))
            .collect();
    }

    /// Reports every stream used or tuple merged more than [`MAX_PORTS`] ways, which would need port indices beyond the
    /// largest a port name may have. Returns whether there were none.
    fn check_widths(&mut self, statements: &[Statement]) -> bool {
        let before = self.errors.len();
        let mut too_wide = Vec::new();
        for statement in statements {
            let pipeline = statement_pipeline(statement);
            if let Head::Tuple(tuple) = &pipeline.head {
                if let Some(item) = tuple.items.get(MAX_PORTS) {
                    too_wide.push((item.span, format!("A tuple merges at most {} streams.", MAX_PORTS)));
                }
            }
        }
        for uses in self.uses.values() {
            if let Some(extra) = uses.get(MAX_PORTS) {
                too_wide.push((
                    extra.span,
                    format!(
                        "A stream is used at most {} times; copy it in steps, as in `let t = s | tee(ports: 2)`, and \
                         use each copy fewer times.",
                        MAX_PORTS
                    ),
                ));
            }
        }
        too_wide.sort_by_key(|(span, _)| span.start);
        for (span, message) in too_wide {
            self.error(span, message);
        }
        self.errors.len() == before
    }

    /// Records every `as` clause of a pipeline. `own` is the name of the pipeline's `let`, which the `as` of its last
    /// node may repeat, since it names the same node.
    fn scan_names(&mut self, pipeline: &Pipeline, own: Option<&str>) {
        let last_stage = pipeline.stages.len().checked_sub(1);
        if let Head::Tuple(Tuple { id: Some(id), .. }) = &pipeline.head {
            if !(pipeline.stages.is_empty() && own == Some(id.id.as_str())) {
                self.define(id, NameKind::As);
            }
        }
        for (index, stage) in pipeline.stages.iter().enumerate() {
            if let Some(id) = stage_id(stage) {
                if !(Some(index) == last_stage && own == Some(id.id.as_str())) {
                    self.define(id, NameKind::As);
                }
            }
        }
        let Some(sink) = &pipeline.end else {
            return;
        };
        if let Some(id) = &sink.id {
            if matches!(sink.kind, SinkKind::Exit { .. }) {
                self.error(
                    id.span,
                    "An `exit` leaves the scope and makes no node, so it cannot be named with `as`.",
                );
            } else {
                self.define(id, NameKind::As);
            }
        }
    }

    /// Records a name the author wrote, returning whether it is new. A name already taken is reported here.
    fn define(&mut self, name: &Name, kind: NameKind) -> bool {
        let Some(&(first, first_kind)) = self.names.get(name.id.as_str()) else {
            self.names.insert(name.id.to_string(), (name.span, kind));
            return true;
        };
        let at = self.position(first);
        let message = match (first_kind, kind) {
            (NameKind::Let, NameKind::Let) => format!("`{}` is defined twice; it is first defined at {}.", name.id, at),
            (NameKind::Let, NameKind::As) => format!(
                "`{}` is already the name of the `let` at {}; choose another node ID.",
                name.id, at
            ),
            (NameKind::As, NameKind::Let) => format!(
                "`{}` is already the ID of the node named at {}; choose another name for this `let`.",
                name.id, at
            ),
            (NameKind::As, NameKind::As) => format!(
                "Two nodes are named `{}`; the first is named at {}. Node IDs are unique.",
                name.id, at
            ),
        };
        self.reported.insert(name.id.to_string());
        self.error(name.span, message);
        false
    }

    /// Returns what a reference in statement `statement` refers to, reporting a reference to a `let` that is not yet
    /// defined.
    fn scan_ref(&mut self, statement: usize, reference: &Ref) -> Option<UseKey> {
        let name = reference.name.id.as_str();
        let port = reference.port.as_ref().map(|(port, _)| port.clone());
        match self.lets.get(name) {
            None => Some((Target::Input(name.to_string()), port)),
            Some(definition) if definition.statement < statement => Some((Target::Let(name.to_string()), port)),
            Some(definition) => {
                let message = if definition.statement == statement {
                    format!(
                        "`{}` refers to itself; a `let` is visible only after its statement, and programs cannot loop.",
                        name
                    )
                } else {
                    format!(
                        "`{}` is used before it is defined, at {}; programs cannot loop, so define it first.",
                        name,
                        self.position(definition.span)
                    )
                };
                self.error(reference.name.span, message);
                None
            }
        }
    }

    // Lowering: statements to nodes and edges.

    fn statement(&mut self, statement: &Statement) {
        match statement {
            Statement::Let { name, pipeline, span } => self.lower_let(name, pipeline, *span),
            Statement::Run { pipeline, span } => self.lower_run(pipeline, *span),
        }
    }

    /// Lowers `let x = <pipeline>`.
    fn lower_let(&mut self, name: &Name, pipeline: &Pipeline, span: Span) {
        self.pipeline_start = pipeline.span.start;
        let x = name.id.as_str();
        if let Some(sink) = &pipeline.end {
            self.error(
                sink.span,
                format!(
                    "A `let` names a stream, so its pipeline cannot end in a sink; end the `let` before it, and write \
                     `{} | ...` in a statement of its own.",
                    x
                ),
            );
        }

        let mut stream = match &pipeline.head {
            Head::Ref(reference) => {
                if pipeline.stages.is_empty() {
                    self.error(
                        span,
                        format!(
                            "`let {} = {}` names no node: a `let` names the node its last stage makes. Use `{}` \
                             directly, or add a stage.",
                            x,
                            describe_ref(reference),
                            describe_ref(reference)
                        ),
                    );
                }
                Flow::Source(self.reference(reference).0)
            }
            Head::Tuple(tuple) => {
                let (id, own) = match &tuple.id {
                    Some(id) => (id.id.clone(), None),
                    None if pipeline.stages.is_empty() => (name.id.clone(), Some(x)),
                    None => (ids::merge(x), None),
                };
                let merge = self.tuple(tuple, id, own);
                Flow::Stream(merge.map(|node| Stream { node, port: None }))
            }
        };

        let mut ordinals = Ordinals::default();
        let last = pipeline.stages.len().checked_sub(1);
        for (index, stage) in pipeline.stages.iter().enumerate() {
            let ordinal = ordinals.next(stage_operator(stage));
            let input = self.continue_from(stream, stage_span(stage));
            let (id, own) = match stage_id(stage) {
                Some(id) => (id.id.clone(), None),
                None if Some(index) == last => (name.id.clone(), Some(x)),
                None => (ids::stage(x, stage_operator(stage), ordinal), None),
            };
            stream = Flow::Stream(self.stage(stage, id, own, input));
        }

        let stream = match stream {
            Flow::Stream(stream) => stream,
            Flow::Source(_) => None,
        };
        if let Some(definition) = self.lets.get_mut(x) {
            definition.stream = stream;
        }
    }

    /// Lowers a pipeline that ends in a sink.
    fn lower_run(&mut self, pipeline: &Pipeline, span: Span) {
        self.pipeline_start = pipeline.span.start;
        let (mut flow, anchor) = match &pipeline.head {
            Head::Ref(reference) => {
                let (source, anchor) = self.reference(reference);
                (Flow::Source(source), anchor)
            }
            Head::Tuple(tuple) => {
                let first = tuple
                    .items
                    .first()
                    .map(|item| self.anchor_of(item))
                    // The parser makes no empty tuple; any anchor does for one.
                    .unwrap_or_else(|| ids::port_anchor("tuple", None));
                let id = match &tuple.id {
                    Some(id) => id.id.clone(),
                    None => ids::merge(first.as_str()),
                };
                let merge = self.tuple(tuple, id.clone(), None);
                (Flow::Stream(merge.map(|node| Stream { node, port: None })), id)
            }
        };

        let mut ordinals = Ordinals::default();
        for stage in &pipeline.stages {
            let ordinal = ordinals.next(stage_operator(stage));
            let input = self.continue_from(flow, stage_span(stage));
            let id = match stage_id(stage) {
                Some(id) => id.id.clone(),
                None => ids::stage(anchor.as_str(), stage_operator(stage), ordinal),
            };
            flow = Flow::Stream(self.stage(stage, id, None, input));
        }

        match &pipeline.end {
            Some(sink) => {
                let input = self.continue_from(flow, sink.span);
                self.sink(sink, input);
            }
            None => self.error(
                span,
                "This statement does not end in a sink, so its stream goes nowhere. End it with `| drop`, or name it \
                 with `let` to use it later.",
            ),
        }
    }

    /// Lowers a sink, entered from `input`.
    fn sink(&mut self, sink: &Sink, input: Option<Source>) {
        let (op, params, param_spans) = match &sink.kind {
            SinkKind::Exit { name, .. } => {
                if let Some(input) = input {
                    self.edges.push((
                        ArtifactEdge {
                            from: input.from,
                            to: ArtifactPortRef::Exit(name.clone()),
                        },
                        sink.span,
                    ));
                }
                return;
            }
            SinkKind::Recipients { names, span } => (
                names::EMIT,
                single_param(
                    EMIT_TO,
                    Value::Array(names.iter().cloned().map(Value::String).collect()),
                ),
                vec![(EMIT_TO.to_string(), *span)],
            ),
            SinkKind::Output { output, span } => (
                names::EMIT,
                single_param("output", Value::String(output.to_string())),
                vec![("output".to_string(), *span)],
            ),
            SinkKind::Drop => (names::DROP, Params::new(), Vec::new()),
        };
        let id = match (&sink.id, &input) {
            (Some(id), _) => id.id.clone(),
            (None, Some(input)) => ids::terminal(&input.parent, &input.port),
            // What feeds the sink is in error, which is reported, so the sink has no ID to derive.
            (None, None) => return,
        };
        let node = self.add_node(id, op, params, sink.span, param_spans, sink.id.is_some(), None);
        if let (Some(node), Some(input)) = (node, input) {
            let to = self.input_port(node, sink.span);
            self.connect(input, to, sink.span);
        }
    }

    /// Lowers a stage with the ID `id`, entered from `input`, returning the stream it leaves.
    fn stage(&mut self, stage: &Stage, id: ArtifactNodeId, own: Option<&str>, input: Option<Source>) -> Option<Stream> {
        match stage {
            Stage::Op(OpStage {
                op,
                params,
                param_spans,
                id: written,
                span,
                ..
            }) => {
                let node = self.add_node(
                    id,
                    op.as_str(),
                    params.clone(),
                    *span,
                    param_spans.clone(),
                    written.is_some(),
                    own,
                )?;
                if let Some(input) = input {
                    let to = self.input_port(node, *span);
                    self.connect(input, to, *span);
                }
                Some(Stream { node, port: None })
            }
            Stage::Where(stage) => {
                let param_spans = stage
                    .param_spans
                    .iter()
                    .map(|(path, span)| (predicate_path(path), *span))
                    .collect();
                let params = single_param("predicate", stage.predicate.clone());
                let split = self.add_node(
                    id.clone(),
                    names::SPLIT,
                    params,
                    stage.span,
                    param_spans,
                    stage.id.is_some(),
                    own,
                )?;
                if let Some(input) = input {
                    let to = self.input_port(split, stage.span);
                    self.connect(input, to, stage.span);
                }
                let false_port = port_name(FALSE_PORT);
                let drop_id = ids::terminal(id.as_str(), &false_port);
                if let Some(drop) =
                    self.add_node(drop_id, names::DROP, Params::new(), stage.span, Vec::new(), false, None)
                {
                    let source = Source {
                        from: ArtifactPortRef::Node {
                            node: id.clone(),
                            port: false_port.clone(),
                        },
                        parent: id.to_string(),
                        port: false_port,
                    };
                    let to = self.input_port(drop, stage.span);
                    self.connect(source, to, stage.span);
                }
                Some(Stream {
                    node: split,
                    port: Some(port_name(TRUE_PORT)),
                })
            }
        }
    }

    /// Lowers the merge of a tuple with the ID `id`, and the edges into it.
    fn tuple(&mut self, tuple: &Tuple, id: ArtifactNodeId, own: Option<&str>) -> Option<usize> {
        let sources = tuple
            .items
            .iter()
            .map(|item| (self.reference(item).0, item.span))
            .collect::<Vec<_>>();
        let params = count_param("inputs", tuple.items.len());
        let merge = self.add_node(
            id.clone(),
            names::MERGE,
            params,
            tuple.span,
            Vec::new(),
            tuple.id.is_some(),
            own,
        )?;
        for (index, (source, span)) in sources.into_iter().enumerate() {
            if let Some(source) = source {
                let port = port_name(&index.to_string());
                self.connect(source, Some(ArtifactPortRef::Node { node: id.clone(), port }), span);
            }
        }
        Some(merge)
    }

    /// Returns the anchor of a tuple item without lowering it: what [`Self::reference`] returns, for the item it will
    /// lower next.
    fn anchor_of(&self, reference: &Ref) -> ArtifactNodeId {
        let rank = self.ranks.get(self.seq).cloned().flatten();
        self.anchor(reference, rank.as_ref())
    }

    fn anchor(&self, reference: &Ref, rank: Option<&(UseKey, usize, usize)>) -> ArtifactNodeId {
        let name = reference.name.id.as_str();
        let port = reference.port.as_ref().map(|(port, _)| port);
        if let Some(((target, port), rank, count)) = rank {
            let node = self.target_node(target);
            if *count > 1 {
                return ids::tee_port(fanout_id(target, node.as_deref(), port.as_ref()).as_str(), *rank);
            }
            if let Target::Input(name) = target {
                return ids::input_anchor(name);
            }
            return ids::port_anchor(node.as_deref().unwrap_or(name), port.as_ref());
        }
        ids::port_anchor(name, port)
    }

    /// Returns the ID of the node that ends the `let` a target names, if it is lowered.
    fn target_node(&self, target: &Target) -> Option<String> {
        match target {
            Target::Let(name) => {
                let stream = self.lets.get(name)?.stream.as_ref()?;
                Some(self.entries[stream.node].node.id.to_string())
            }
            Target::Input(_) => None,
        }
    }

    /// Lowers a reference: returns the port it starts from, through the implicit tee when its stream is used more than
    /// once, and its anchor. Reports a reference to a port that does not exist, or one without a port to a node with
    /// several.
    fn reference(&mut self, reference: &Ref) -> (Option<Source>, ArtifactNodeId) {
        let rank = self.ranks.get(self.seq).cloned().flatten();
        self.seq += 1;
        let anchor = self.anchor(reference, rank.as_ref());
        let Some(((target, port), rank, count)) = rank else {
            return (None, anchor);
        };

        let produced = self.produce(&target, port.as_ref(), reference);
        if count <= 1 {
            return (produced, anchor);
        }
        let key = (target.clone(), port.clone());
        let tee = match self.tees.get(&key) {
            Some(tee) => tee.clone(),
            None => {
                let tee = produced.and_then(|produced| self.implicit_tee(&key, produced, count));
                self.tees.insert(key, tee.clone());
                tee
            }
        };
        let source = tee.map(|tee| {
            let port = port_name(&rank.to_string());
            Source {
                from: ArtifactPortRef::Node {
                    node: tee.clone(),
                    port: port.clone(),
                },
                parent: tee.to_string(),
                port,
            }
        });
        (source, anchor)
    }

    /// Makes the implicit tee of a stream used `count` times, fed from `produced`.
    fn implicit_tee(&mut self, key: &UseKey, produced: Source, count: usize) -> Option<ArtifactNodeId> {
        let (target, port) = key;
        let node = self.target_node(target);
        let id = fanout_id(target, node.as_deref(), port.as_ref());
        let span = match target {
            Target::Let(name) => self.lets[name].span,
            Target::Input(_) => self.uses[key][0].span,
        };
        let tee = self.add_node(
            id.clone(),
            names::TEE,
            count_param("ports", count),
            span,
            Vec::new(),
            false,
            None,
        )?;
        let to = self.input_port(tee, span);
        self.connect(produced, to, span);
        Some(id)
    }

    /// Returns the port a reference names on its target, before any implicit tee, reporting a port that does not
    /// exist or is missing.
    fn produce(&mut self, target: &Target, port: Option<&PortName>, reference: &Ref) -> Option<Source> {
        let name = reference.name.id.as_str();
        let port_span = reference.port.as_ref().map(|(_, span)| *span).unwrap_or(reference.span);
        let let_name = match target {
            Target::Input(input) => {
                if port.is_some() {
                    self.error(
                        port_span,
                        format!("`{}` is a scope input, which has no ports; write `{}`.", name, name),
                    );
                    return None;
                }
                let input_name = InputName::new(input.as_str()).ok()?;
                let port = PortName::new(input.as_str()).ok()?;
                return Some(Source {
                    from: ArtifactPortRef::Input(input_name),
                    parent: SCOPE_INPUT_NODE.to_string(),
                    port,
                });
            }
            Target::Let(let_name) => let_name,
        };
        let stream = self.lets.get(let_name)?.stream.clone()?;
        let entry = &self.entries[stream.node];
        let node_id = entry.node.id.clone();
        let resolved = match (&stream.port, port) {
            (Some(fixed), None) => fixed.clone(),
            (Some(_), Some(port)) => {
                let message = format!(
                    "`{}` ends in `where`, so it is one stream, with no port `{}`; write `{}`.",
                    name, port, name
                );
                self.error(port_span, message);
                return None;
            }
            (None, None) => match entry.shape.default_output() {
                Some(port) => port,
                None => {
                    let outputs = entry.shape.outputs().unwrap_or_default().to_vec();
                    let message = if outputs.is_empty() {
                        format!(
                            "`{}` is a `{}`, which has no output ports, so nothing can follow it.",
                            name, entry.node.op
                        )
                    } else {
                        format!(
                            "`{}` is a `{}`, which has several output ports, so pick a port: {}.",
                            name,
                            entry.node.op,
                            alternatives(name, &outputs)
                        )
                    };
                    self.error(reference.span, message);
                    if let Some(definition) = self.lets.get_mut(let_name) {
                        definition.bare_error = true;
                    }
                    return None;
                }
            },
            (None, Some(port)) => {
                if let Some(outputs) = entry.shape.outputs() {
                    let message = match outputs.len() {
                        0 => Some(format!(
                            "`{}` is a `{}`, which has no output ports, so it has no port `{}`.",
                            name, entry.node.op, port
                        )),
                        1 => Some(format!(
                            "`{}` is a `{}`, which has one output port, so write `{}` without a port.",
                            name, entry.node.op, name
                        )),
                        _ if !outputs.contains(port) => Some(format!(
                            "`{}` is a `{}`, which has no output port `{}`; pick one of {}.",
                            name,
                            entry.node.op,
                            port,
                            alternatives(name, outputs)
                        )),
                        _ => None,
                    };
                    if let Some(message) = message {
                        self.error(port_span, message);
                        return None;
                    }
                }
                port.clone()
            }
        };
        Some(Source {
            from: ArtifactPortRef::Node {
                node: node_id.clone(),
                port: resolved.clone(),
            },
            parent: node_id.to_string(),
            port: resolved,
        })
    }

    /// Returns the port a pipeline continues from into the element at `span`, reporting a node with several output
    /// ports, or none, that the pipeline tries to continue from.
    fn continue_from(&mut self, flow: Flow, span: Span) -> Option<Source> {
        let stream = match flow {
            Flow::Source(source) => return source,
            Flow::Stream(stream) => stream?,
        };
        let entry = &self.entries[stream.node];
        let id = entry.node.id.clone();
        let port = match stream.port {
            Some(port) => port,
            None => match entry.shape.default_output() {
                Some(port) => port,
                None => {
                    let outputs = entry.shape.outputs().unwrap_or_default().to_vec();
                    let message = if outputs.is_empty() {
                        format!(
                            "`{}` has no output ports, so the pipeline cannot continue after it.",
                            entry.node.op
                        )
                    } else {
                        let ports = outputs.iter().map(|port| format!("`{}`", port)).collect::<Vec<_>>();
                        let uses = outputs.iter().map(|port| format!("`s.{}`", port)).collect::<Vec<_>>();
                        let written = self
                            .source
                            .get(self.pipeline_start..entry.span.end)
                            .filter(|written| !written.trim().is_empty());
                        let named = match written {
                            Some(written) => format!(
                                "name it with `let s = {}`",
                                written.split_whitespace().collect::<Vec<_>>().join(" ")
                            ),
                            None => "name it with a `let`".to_string(),
                        };
                        format!(
                            "`{}` has {} output ports, {}; {} and use {}.",
                            entry.node.op,
                            count_word(outputs.len()),
                            and_list(&ports),
                            named,
                            and_list(&uses)
                        )
                    };
                    self.error(span, message);
                    return None;
                }
            },
        };
        Some(Source {
            from: ArtifactPortRef::Node {
                node: id.clone(),
                port: port.clone(),
            },
            parent: id.to_string(),
            port,
        })
    }

    /// Returns the input port a pipeline enters node `index` at, reporting a node with several input ports, or none.
    fn input_port(&mut self, index: usize, span: Span) -> Option<ArtifactPortRef> {
        let entry = &self.entries[index];
        match entry.shape.default_input() {
            Some(port) => Some(ArtifactPortRef::Node {
                node: entry.node.id.clone(),
                port,
            }),
            None => {
                let inputs = entry.shape.inputs().unwrap_or_default();
                let message = if inputs.is_empty() {
                    format!("`{}` has no input ports, so no stream can enter it.", entry.node.op)
                } else {
                    format!(
                        "`{}` has {} input ports, so a pipeline cannot enter it; combine streams with a tuple, as in \
                         `(a, b)`.",
                        entry.node.op,
                        inputs.len()
                    )
                };
                self.error(span, message);
                None
            }
        }
    }

    fn connect(&mut self, from: Source, to: Option<ArtifactPortRef>, span: Span) {
        if let Some(to) = to {
            self.edges.push((ArtifactEdge { from: from.from, to }, span));
        }
    }

    /// Adds a node, returning its position, unless its ID is already taken, which is reported.
    ///
    /// `written` is whether the author wrote the ID with `as`; names the author wrote were checked against each other
    /// by the scan, so a derived ID is checked against them here. `own` is the name of the `let` whose last node this
    /// is, which its derived ID may equal.
    #[allow(clippy::too_many_arguments)]
    fn add_node(
        &mut self, id: ArtifactNodeId, op: &str, params: Params, span: Span, param_spans: Vec<(String, Span)>,
        written: bool, own: Option<&str>,
    ) -> Option<usize> {
        let taken = match self.by_id.get(id.as_str()) {
            // Two `as` clauses with one ID, which the scan reported.
            Some(_) if written => return None,
            Some(&existing) => Some(format!("the node at {}", self.position(self.entries[existing].span))),
            None if written || own == Some(id.as_str()) => None,
            None => self.names.get(id.as_str()).map(|&(first, kind)| match kind {
                NameKind::Let => format!("the name of the `let` at {}", self.position(first)),
                NameKind::As => format!("the ID of the node named at {}", self.position(first)),
            }),
        };
        if let Some(taken) = taken {
            // A name taken twice is reported once, where it is first found taken.
            if !self.reported.insert(id.to_string()) {
                return None;
            }
            let message = format!(
                "This `{}` would be the node `{}`, which is already {}. Name one of them with `as`.",
                op, id, taken
            );
            self.error(span, message);
            return None;
        }

        let op = OperatorName::new(op).expect("operator names are identifiers");
        let shape = NodeShape::of(&op, &params);
        let index = self.entries.len();
        self.by_id.insert(id.to_string(), index);
        self.entries.push(Entry {
            node: ArtifactNode { id, op, params },
            shape,
            span,
            param_spans,
        });
        Some(index)
    }

    // The rules checked once everything is lowered.

    /// Reports every `let` that is never used, and every output port of a `let`'s node, among several, that no
    /// reference uses.
    fn check_unused(&mut self, statements: &[Statement]) {
        for (index, statement) in statements.iter().enumerate() {
            let Statement::Let { name, .. } = statement else {
                continue;
            };
            let Some(definition) = self.lets.get(name.id.as_str()) else {
                continue;
            };
            if definition.statement != index {
                // A second `let` of the same name, already reported.
                continue;
            }
            let Some(stream) = &definition.stream else {
                continue;
            };
            let target = Target::Let(name.id.to_string());
            let used = self
                .uses
                .keys()
                .filter(|(used, _)| *used == target)
                .map(|(_, port)| port.clone())
                .collect::<HashSet<_>>();
            let shape = &self.entries[stream.node].shape;
            let outputs = match (&stream.port, shape.outputs()) {
                (None, Some(outputs)) if outputs.len() > 1 => outputs.to_vec(),
                (None, Some([])) => continue,
                _ => {
                    if used.is_empty() {
                        let message = format!(
                            "`{}` is never used; use it, write `{} | drop` to discard it, or remove the `let`.",
                            name.id, name.id
                        );
                        self.error(name.span, message);
                    }
                    continue;
                }
            };
            if definition.bare_error {
                continue;
            }
            for port in outputs {
                if !used.contains(&Some(port.clone())) {
                    let message = format!(
                        "`{}.{}` is never used; write `{}.{} | drop`.",
                        name.id, port, name.id, port
                    );
                    self.error(name.span, message);
                }
            }
        }
    }
}

/// What a pipeline has reached: a port, as a head reference gives, or a node a stage made. `None` inside either is a
/// part in error, which is reported, and from which no edge leaves.
enum Flow {
    Source(Option<Source>),
    Stream(Option<Stream>),
}

/// Returns the ID of the implicit tee after a target's stream: [`ids::fanout`] of the `let`'s node and the port the
/// references write, or of `input` and the input's name.
fn fanout_id(target: &Target, node: Option<&str>, port: Option<&PortName>) -> ArtifactNodeId {
    match target {
        Target::Let(name) => ids::fanout(node.unwrap_or(name), port),
        Target::Input(input) => {
            let input = PortName::new(input.as_str()).expect("scope input names are port names");
            ids::fanout(SCOPE_INPUT_NODE, Some(&input))
        }
    }
}

fn statement_pipeline(statement: &Statement) -> &Pipeline {
    match statement {
        Statement::Let { pipeline, .. } | Statement::Run { pipeline, .. } => pipeline,
    }
}

/// The most ports a node may have: one per port index a port name may take.
const MAX_PORTS: usize = PortName::MAX_INDEX as usize + 1;

/// Returns the references of a pipeline's head, in order. Only heads hold references.
fn head_refs(head: &Head) -> Vec<&Ref> {
    match head {
        Head::Ref(reference) => vec![reference],
        Head::Tuple(tuple) => tuple.items.iter().collect(),
    }
}

fn stage_id(stage: &Stage) -> Option<&Name> {
    match stage {
        Stage::Op(stage) => stage.id.as_ref(),
        Stage::Where(stage) => stage.id.as_ref(),
    }
}

fn stage_span(stage: &Stage) -> Span {
    match stage {
        Stage::Op(stage) => stage.span,
        Stage::Where(stage) => stage.span,
    }
}

/// Returns the operator a stage runs, whose name its derived ID and ordinal use.
fn stage_operator(stage: &Stage) -> &str {
    match stage {
        Stage::Op(stage) => stage.op.as_str(),
        Stage::Where(_) => ids::WHERE_OPERATOR,
    }
}

fn describe_ref(reference: &Ref) -> String {
    match &reference.port {
        Some((port, _)) => format!("{}.{}", reference.name.id, port),
        None => reference.name.id.to_string(),
    }
}

/// Returns the path of a `where` predicate's span under the `split` node: paths from `predicate` are kept, and a path
/// within the predicate is put under it.
fn predicate_path(path: &str) -> String {
    if path == "predicate" || path.starts_with("predicate.") || path.starts_with("predicate[") {
        path.to_string()
    } else if path.is_empty() {
        "predicate".to_string()
    } else if path.starts_with('[') {
        format!("predicate{}", path)
    } else {
        format!("predicate.{}", path)
    }
}

/// Returns a count in words: `two`, `three`, and so on up to ten, and digits beyond.
fn count_word(count: usize) -> String {
    const WORDS: [&str; 11] = [
        "no", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    ];
    WORDS
        .get(count)
        .map_or_else(|| count.to_string(), |word| word.to_string())
}

/// Joins items with `and`: `a and b`, or `a, b, and c`.
fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [first, second] => format!("{} and {}", first, second),
        [rest @ .., last] => format!("{}, and {}", rest.join(", "), last),
    }
}

/// Lists the ports of `name` to pick from: `x.a` or `x.b`, or `x.a`, `x.b`, or `x.c`.
fn alternatives(name: &str, ports: &[PortName]) -> String {
    let choices = ports
        .iter()
        .map(|port| format!("`{}.{}`", name, port))
        .collect::<Vec<_>>();
    match choices.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [first, second] => format!("{} or {}", first, second),
        [rest @ .., last] => format!("{}, or {}", rest.join(", "), last),
    }
}

/// Returns the port named `name`, an operator's port name or a decimal index.
fn port_name(name: &str) -> PortName {
    PortName::new(name).expect("operator port names and indices are port names")
}

fn single_param(name: &str, value: Value) -> Params {
    std::iter::once((ParamName::new(name).expect("parameter names are identifiers"), value)).collect()
}

fn count_param(name: &str, count: usize) -> Params {
    single_param(name, Value::from(count))
}

#[cfg(test)]
mod tests;
