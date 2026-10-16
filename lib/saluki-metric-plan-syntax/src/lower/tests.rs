//! Tests of lowering, on syntax trees built by hand, independently of the parser.
//!
//! Each element a helper builds gets a fresh span, so that spans are distinct and increase in the order the tree is
//! built, which is source order unless a test says otherwise.

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet, HashMap},
};

use saluki_metric_plan::{
    ir::{ArtifactBase, ArtifactNodeId, OperatorName, ParamName, Params, PolicyArtifact, PortName, ScopeName},
    operators::OperatorRegistry,
};
use serde_json::{json, Value};

use super::*;
use crate::ast::{
    Base, Branch, End, Fork, Head, Name, OpStage, Pipeline, Program, Ref, Sink, SinkKind, Stage, Statement, Tuple,
    WhereStage,
};

const BASE: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

thread_local! {
    static NEXT: Cell<usize> = const { Cell::new(0) };
}

/// Returns a fresh span, after every span returned before on this thread.
fn sp() -> Span {
    NEXT.with(|next| {
        let start = next.get();
        next.set(start + 2);
        Span::new(start, start + 1)
    })
}

/// A source long enough for every span the helpers make, for positions in messages.
fn source() -> String {
    " ".repeat(1 << 16)
}

fn id(value: &str) -> ArtifactNodeId {
    ArtifactNodeId::new(value).unwrap()
}

fn name(value: &str) -> Name {
    Name {
        id: id(value),
        span: sp(),
    }
}

fn r(value: &str) -> Ref {
    let name = name(value);
    Ref {
        span: name.span,
        name,
        port: None,
    }
}

fn rp(value: &str, port: &str) -> Ref {
    let name = name(value);
    let port_span = sp();
    Ref {
        span: name.span.to(port_span),
        name,
        port: Some((PortName::new(port).unwrap(), port_span)),
    }
}

fn params(value: Value) -> Params {
    match value {
        Value::Object(map) => map
            .into_iter()
            .map(|(key, value)| (ParamName::new(key).unwrap(), value))
            .collect(),
        _ => panic!("parameters are an object"),
    }
}

/// `<op>(<params>)`, with a span for each parameter.
fn op_with(op: &str, values: Value, as_id: Option<&str>) -> Stage {
    let op_span = sp();
    let params = params(values);
    let param_spans = params.iter().map(|(name, _)| (name.to_string(), sp())).collect();
    Stage::Op(OpStage {
        op: OperatorName::new(op).unwrap(),
        op_span,
        params,
        positional: None,
        param_spans,
        id: as_id.map(name),
        span: op_span.to(sp()),
    })
}

fn op(op: &str) -> Stage {
    op_with(op, json!({}), None)
}

fn op_as(op: &str, as_id: &str) -> Stage {
    op_with(op, json!({}), Some(as_id))
}

fn ts() -> Stage {
    op_with("timestamp_split", json!({"window": "10s"}), None)
}

fn where_with(predicate: Value, as_id: Option<&str>) -> Stage {
    let span = sp();
    Stage::Where(WhereStage {
        predicate,
        param_spans: vec![("predicate".to_string(), sp())],
        id: as_id.map(name),
        span,
    })
}

fn wh(intake_name: &str) -> Stage {
    where_with(json!({"intake_name": {"eq": intake_name}}), None)
}

fn sink(kind: SinkKind, as_id: Option<&str>) -> End {
    End::Sink(Sink {
        kind,
        id: as_id.map(name),
        span: sp(),
    })
}

fn emit_output(output: &str) -> End {
    sink(
        SinkKind::Output {
            output: output.parse().unwrap(),
            span: sp(),
        },
        None,
    )
}

fn drop_() -> End {
    sink(SinkKind::Drop, None)
}

fn fork(as_id: Option<&str>, branches: Vec<Branch>) -> End {
    End::Fork(Fork {
        id: as_id.map(name),
        branches,
        span: sp(),
    })
}

fn branch(stages: Vec<Stage>, end: End) -> Branch {
    Branch {
        stages,
        end,
        span: sp(),
    }
}

fn tuple(items: Vec<Ref>, as_id: Option<&str>) -> Head {
    Head::Tuple(Tuple {
        items,
        id: as_id.map(name),
        span: sp(),
    })
}

fn pipeline(head: Head, stages: Vec<Stage>, end: Option<End>) -> Pipeline {
    Pipeline {
        head,
        stages,
        end,
        span: sp(),
    }
}

fn let_(value: &str, head: Head, stages: Vec<Stage>) -> Statement {
    Statement::Let {
        name: name(value),
        pipeline: pipeline(head, stages, None),
        span: sp(),
    }
}

fn run(head: Head, stages: Vec<Stage>, end: End) -> Statement {
    Statement::Run {
        pipeline: pipeline(head, stages, Some(end)),
        span: sp(),
    }
}

fn reference(value: &str) -> Head {
    Head::Ref(r(value))
}

fn program(statements: Vec<Statement>) -> Program {
    Program {
        head: sp(),
        scope: ScopeName::new("metrics").unwrap(),
        ir_version: None,
        base: Some(Base {
            program_hash: BASE.parse().unwrap(),
            outputs: BTreeMap::new(),
        }),
        blobs: None,
        statements,
        close: sp(),
    }
}

#[track_caller]
fn lower_ok(program: Program) -> (PolicyArtifact, SpanMap) {
    match lower(&source(), program, None) {
        Ok(lowered) => {
            assert_well_formed(&lowered.0);
            lowered
        }
        Err(errors) => panic!("the program should lower: {:#?}", errors),
    }
}

#[track_caller]
fn lower_err(program: Program) -> Vec<Error> {
    match lower(&source(), program, None) {
        Ok((artifact, _)) => panic!("the program should not lower: {:#?}", artifact),
        Err(errors) => errors,
    }
}

/// Asserts that exactly one error contains `message`, and that it is at `span`.
#[track_caller]
fn assert_error(errors: &[Error], span: Span, message: &str) {
    let matching = errors
        .iter()
        .filter(|error| error.message.contains(message))
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "one error says {:?}: {:#?}", message, errors);
    assert_eq!(matching[0].span, span, "the span of {:?}", matching[0].message);
}

fn ids(artifact: &PolicyArtifact) -> Vec<&str> {
    artifact.nodes.iter().map(|node| node.id.as_str()).collect()
}

fn edges(artifact: &PolicyArtifact) -> Vec<String> {
    artifact
        .edges
        .iter()
        .map(|edge| format!("{} -> {}", edge.from, edge.to))
        .collect()
}

fn node<'a>(artifact: &'a PolicyArtifact, value: &str) -> &'a saluki_metric_plan::ir::ArtifactNode {
    artifact
        .nodes
        .iter()
        .find(|node| node.id.as_str() == value)
        .unwrap_or_else(|| panic!("no node `{}` in {:?}", value, ids(artifact)))
}

/// Asserts that the artifact is a valid artifact document, that every node configures with the native registry (when
/// its parameters need no configuration bindings), and that its graph is well formed: every edge joins ports that
/// exist, every declared input and output port has exactly one edge, and no edge leaves a node it reaches again.
#[track_caller]
fn assert_well_formed(artifact: &PolicyArtifact) {
    let document = serde_json::to_value(artifact).unwrap();
    assert_eq!(&PolicyArtifact::from_value(document).unwrap(), artifact);

    let mut shapes = HashMap::new();
    for node in &artifact.nodes {
        let bound = serde_json::to_string(&node.params).unwrap().contains("\"config\"");
        if !bound {
            OperatorRegistry::native()
                .configure(node.op.as_str(), &node.params)
                .unwrap_or_else(|error| panic!("node `{}` does not configure: {:?}", node.id, error));
        }
        let shape = NodeShape::of(&node.op, &node.params);
        assert!(
            shapes.insert(node.id.to_string(), shape).is_none(),
            "`{}` is unique",
            node.id
        );
    }

    let mut inputs = BTreeMap::<(String, String), usize>::new();
    let mut outputs = BTreeMap::<(String, String), usize>::new();
    let mut successors = HashMap::<String, Vec<String>>::new();
    for edge in &artifact.edges {
        if let ArtifactPortRef::Node { node, port } = &edge.from {
            let shape = &shapes[node.as_str()];
            assert_ne!(
                shape.outputs().map(|ports| ports.contains(port)),
                Some(false),
                "{} -> {}",
                edge.from,
                edge.to
            );
            *outputs.entry((node.to_string(), port.to_string())).or_default() += 1;
        }
        if let ArtifactPortRef::Node { node, port } = &edge.to {
            let shape = &shapes[node.as_str()];
            assert_ne!(
                shape.inputs().map(|ports| ports.contains(port)),
                Some(false),
                "{} -> {}",
                edge.from,
                edge.to
            );
            *inputs.entry((node.to_string(), port.to_string())).or_default() += 1;
            if let ArtifactPortRef::Node { node: from, .. } = &edge.from {
                successors.entry(from.to_string()).or_default().push(node.to_string());
            }
        }
    }
    for (id, shape) in &shapes {
        for port in shape.inputs().unwrap_or_default() {
            let key = (id.clone(), port.to_string());
            assert_eq!(inputs.get(&key), Some(&1), "edges into `{}.{}`", id, port);
        }
        for port in shape.outputs().unwrap_or_default() {
            let key = (id.clone(), port.to_string());
            assert_eq!(outputs.get(&key), Some(&1), "edges from `{}.{}`", id, port);
        }
    }

    // Lowering writes nodes in a topological order, so every edge between nodes goes forward.
    let position = artifact
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.id.to_string(), i))
        .collect::<HashMap<_, _>>();
    for (from, tos) in &successors {
        for to in tos {
            assert!(position[from] < position[to], "`{}` comes before `{}`", from, to);
        }
    }
}

// Forms.

#[test]
fn a_scope_input_feeds_a_stage_and_a_sink() {
    let (artifact, _) = lower_ok(program(vec![run(
        reference("dogstatsd"),
        vec![op("host_enrichment")],
        drop_(),
    )]));
    assert_eq!(
        ids(&artifact),
        ["input_dogstatsd_host_enrichment", "input_dogstatsd_host_enrichment_out"]
    );
    assert_eq!(
        edges(&artifact),
        [
            "input.dogstatsd -> input_dogstatsd_host_enrichment.in",
            "input_dogstatsd_host_enrichment.out -> input_dogstatsd_host_enrichment_out.in",
        ]
    );
    assert_eq!(
        node(&artifact, "input_dogstatsd_host_enrichment_out").op.as_str(),
        "drop"
    );
}

#[test]
fn a_sink_straight_after_a_scope_input_is_named_after_the_input() {
    let (artifact, _) = lower_ok(program(vec![run(reference("otlp"), vec![], emit_output("o"))]));
    assert_eq!(ids(&artifact), ["input_otlp"]);
    assert_eq!(node(&artifact, "input_otlp").params, params(json!({"output": "o"})));
    assert_eq!(edges(&artifact), ["input.otlp -> input_otlp.in"]);
}

#[test]
fn sinks_become_emit_and_drop_nodes_and_exit_edges() {
    let recipients = sink(
        SinkKind::Recipients {
            names: vec!["primary".to_string(), "https://b.example.com".to_string()],
            span: sp(),
        },
        None,
    );
    let exit = sink(
        SinkKind::Exit {
            name: "rest".parse().unwrap(),
            span: sp(),
        },
        None,
    );
    let (artifact, _) = lower_ok(program(vec![
        run(reference("a"), vec![], recipients),
        run(reference("b"), vec![], exit),
        run(reference("c"), vec![], sink(SinkKind::Drop, Some("discard"))),
    ]));
    assert_eq!(
        node(&artifact, "input_a").params,
        params(json!({"to": ["primary", "https://b.example.com"]}))
    );
    assert_eq!(node(&artifact, "discard").op.as_str(), "drop");
    assert_eq!(
        edges(&artifact),
        ["input.a -> input_a.in", "input.b -> exit.rest", "input.c -> discard.in"]
    );
}

#[test]
fn a_let_names_its_last_stage_and_derives_the_earlier_ones() {
    let (artifact, _) = lower_ok(program(vec![
        let_(
            "x",
            reference("a"),
            vec![
                op("host_enrichment"),
                op("host_enrichment"),
                wh("m"),
                op("host_enrichment"),
            ],
        ),
        run(reference("x"), vec![], drop_()),
    ]));
    assert_eq!(
        ids(&artifact),
        [
            "x_host_enrichment",
            "x_host_enrichment_2",
            "x_split",
            "x",
            "x_out",
            "x_split_false"
        ]
    );
}

#[test]
fn every_stage_counts_toward_ordinals_including_named_ones_and_wheres_as_splits() {
    let (artifact, _) = lower_ok(program(vec![
        let_(
            "x",
            reference("a"),
            vec![
                op_as("host_enrichment", "named"),
                op("host_enrichment"),
                wh("m"),
                wh("n"),
                op("host_enrichment"),
            ],
        ),
        let_(
            "y",
            reference("b"),
            vec![
                wh("m"),
                op_with("split", json!({"predicate": {"intake_name": {"eq": "n"}}}), None),
            ],
        ),
        run(reference("x"), vec![], drop_()),
        run(Head::Ref(rp("y", "true")), vec![], drop_()),
        run(Head::Ref(rp("y", "false")), vec![], drop_()),
    ]));
    let ids = ids(&artifact);
    for expected in [
        "named",
        "x_host_enrichment_2",
        "x_split",
        "x_split_false",
        "x_split_2",
        "x_split_2_false",
        "x",
        "y_split",
        "y",
    ] {
        assert!(ids.contains(&expected), "{} in {:?}", expected, ids);
    }
    assert!(!ids.contains(&"x_host_enrichment"), "{:?}", ids);
}

#[test]
fn a_where_is_a_split_whose_false_port_is_dropped() {
    let (artifact, spans) = lower_ok(program(vec![run(
        reference("a"),
        vec![where_with(json!({"intake_name": {"eq": "m"}}), None)],
        emit_output("o"),
    )]));
    assert_eq!(
        ids(&artifact),
        ["input_a_split", "input_a_split_true", "input_a_split_false"]
    );
    assert_eq!(
        node(&artifact, "input_a_split").params,
        params(json!({"predicate": {"intake_name": {"eq": "m"}}}))
    );
    assert_eq!(
        edges(&artifact),
        [
            "input.a -> input_a_split.in",
            "input_a_split.true -> input_a_split_true.in",
            "input_a_split.false -> input_a_split_false.in",
        ]
    );
    // The drop terminal and its edge have the `where`'s span; the predicate's span is under the split.
    let split_span = spans.node("input_a_split").unwrap();
    assert_eq!(spans.node("input_a_split_false"), Some(split_span));
    assert_eq!(spans.edge(2), Some(split_span));
    assert!(spans.param("input_a_split", "predicate.intake_name").is_some());
}

#[test]
fn a_tuple_is_a_merge_with_items_in_port_order() {
    let (artifact, spans) = lower_ok(program(vec![
        let_("x", tuple(vec![r("b"), r("a")], None), vec![op("host_enrichment")]),
        let_("y", tuple(vec![r("c"), r("x")], None), vec![]),
        run(reference("y"), vec![], drop_()),
    ]));
    assert_eq!(ids(&artifact), ["x_in", "x", "y", "y_out"]);
    assert_eq!(node(&artifact, "x_in").params, params(json!({"inputs": 2})));
    assert_eq!(node(&artifact, "y").op.as_str(), "merge");
    let edges = edges(&artifact);
    assert!(edges.contains(&"input.b -> x_in.0".to_string()), "{:?}", edges);
    assert!(edges.contains(&"input.a -> x_in.1".to_string()), "{:?}", edges);
    assert!(edges.contains(&"input.c -> y.0".to_string()), "{:?}", edges);
    assert!(edges.contains(&"x.out -> y.1".to_string()), "{:?}", edges);
    // Each edge into a merge has the span of its item.
    let into_x_in_0 = artifact
        .edges
        .iter()
        .position(|edge| edge.to.to_string() == "x_in.0")
        .unwrap();
    let item = spans.edge(into_x_in_0).unwrap();
    assert_eq!(item.end - item.start, 1);
}

#[test]
fn a_bare_tuple_head_is_named_after_its_first_items_anchor_and_anchors_the_stages() {
    let (artifact, _) = lower_ok(program(vec![
        run(tuple(vec![r("a"), r("b")], None), vec![op("host_enrichment")], drop_()),
        run(
            tuple(vec![r("c"), r("d")], Some("m")),
            vec![op("host_enrichment")],
            drop_(),
        ),
    ]));
    assert_eq!(
        ids(&artifact),
        [
            "input_a_in",
            "input_a_in_host_enrichment",
            "input_a_in_host_enrichment_out",
            "m",
            "m_host_enrichment",
            "m_host_enrichment_out"
        ]
    );
}

#[test]
fn a_fork_is_a_tee_whose_ports_start_its_branches() {
    let (artifact, spans) = lower_ok(program(vec![run(
        reference("a"),
        vec![op("host_enrichment")],
        fork(
            None,
            vec![
                branch(vec![], emit_output("o")),
                branch(vec![wh("m"), op("host_enrichment")], drop_()),
                branch(
                    vec![],
                    fork(
                        None,
                        vec![branch(vec![], drop_()), branch(vec![op("host_enrichment")], drop_())],
                    ),
                ),
            ],
        ),
    )]));
    assert_eq!(
        ids(&artifact),
        [
            "input_a_host_enrichment",
            "input_a_fork",
            "input_a_fork_0",
            "input_a_fork_1_split",
            "input_a_fork_1_host_enrichment",
            "input_a_fork_1_host_enrichment_out",
            "input_a_fork_1_split_false",
            "input_a_fork_2_fork",
            "input_a_fork_2_fork_0",
            "input_a_fork_2_fork_1_host_enrichment",
            "input_a_fork_2_fork_1_host_enrichment_out",
        ]
    );
    assert_eq!(node(&artifact, "input_a_fork").params, params(json!({"ports": 3})));
    assert!(edges(&artifact).contains(&"input_a_fork.2 -> input_a_fork_2_fork.in".to_string()));
    // The fork's tee has the fork's span, as does the edge into it.
    let fork_span = spans.node("input_a_fork").unwrap();
    let into_fork = artifact
        .edges
        .iter()
        .position(|edge| edge.to.to_string() == "input_a_fork.in")
        .unwrap();
    assert_eq!(spans.edge(into_fork), Some(fork_span));
}

#[test]
fn a_named_fork_anchors_its_branches_on_its_name() {
    let (artifact, _) = lower_ok(program(vec![run(
        reference("a"),
        vec![],
        fork(
            Some("p"),
            vec![branch(vec![], emit_output("o")), branch(vec![wh("m")], drop_())],
        ),
    )]));
    assert_eq!(
        ids(&artifact),
        ["p", "p_0", "p_1_split", "p_1_split_true", "p_1_split_false"]
    );
    assert_eq!(edges(&artifact)[0], "input.a -> p.in");
}

#[test]
fn ports_of_a_node_with_several_are_picked_by_name_and_anchor_on_the_port() {
    let (artifact, _) = lower_ok(program(vec![
        let_("t", reference("a"), vec![ts()]),
        run(Head::Ref(rp("t", "passthrough")), vec![op("host_enrichment")], drop_()),
        run(Head::Ref(rp("t", "default")), vec![], drop_()),
    ]));
    assert_eq!(
        ids(&artifact),
        [
            "t",
            "t_default",
            "t_passthrough_host_enrichment",
            "t_passthrough_host_enrichment_out"
        ]
    );
    assert_eq!(
        edges(&artifact),
        [
            "input.a -> t.in",
            "t.default -> t_default.in",
            "t.passthrough -> t_passthrough_host_enrichment.in",
            "t_passthrough_host_enrichment.out -> t_passthrough_host_enrichment_out.in",
        ]
    );
}

#[test]
fn a_let_ending_in_where_is_one_stream_from_its_true_port() {
    let (artifact, _) = lower_ok(program(vec![
        let_("f", reference("a"), vec![wh("m")]),
        run(reference("f"), vec![], drop_()),
    ]));
    assert_eq!(ids(&artifact), ["f", "f_true", "f_false"]);
    assert_eq!(edges(&artifact)[1], "f.true -> f_true.in");
}

#[test]
fn an_as_on_a_lets_last_stage_names_the_node_the_let_refers_to() {
    let (artifact, _) = lower_ok(program(vec![
        let_("x", reference("a"), vec![op_as("host_enrichment", "enrich")]),
        let_("y", reference("b"), vec![op_as("host_enrichment", "y")]),
        run(reference("x"), vec![], drop_()),
        run(reference("y"), vec![], drop_()),
    ]));
    assert_eq!(ids(&artifact).len(), 4, "{:?}", ids(&artifact));
    assert!(ids(&artifact).contains(&"enrich_out"));
    assert!(ids(&artifact).contains(&"y_out"));
}

// Implicit tees.

#[test]
fn a_stream_used_several_times_is_teed_in_source_order() {
    let (artifact, spans) = lower_ok(program(vec![
        let_("x", reference("a"), vec![op("host_enrichment")]),
        run(reference("x"), vec![], emit_output("o")),
        run(reference("x"), vec![op("host_enrichment")], drop_()),
        run(tuple(vec![r("b"), r("x")], None), vec![], drop_()),
    ]));
    assert_eq!(
        ids(&artifact),
        [
            "x",
            "x_fanout",
            "x_fanout_0",
            "x_fanout_1_host_enrichment",
            "x_fanout_1_host_enrichment_out",
            "input_b_in",
            "input_b_in_out",
        ]
    );
    assert_eq!(node(&artifact, "x_fanout").params, params(json!({"ports": 3})));
    let edges = edges(&artifact);
    assert!(edges.contains(&"x.out -> x_fanout.in".to_string()));
    assert!(edges.contains(&"x_fanout.0 -> x_fanout_0.in".to_string()));
    assert!(edges.contains(&"x_fanout.1 -> x_fanout_1_host_enrichment.in".to_string()));
    assert!(edges.contains(&"x_fanout.2 -> input_b_in.1".to_string()));
    // The implicit tee has the span of the `let`'s name.
    assert!(spans.node("x_fanout").is_some());
}

#[test]
fn implicit_tee_ports_follow_span_order_not_tree_order() {
    // The second statement's reference is written first in the source, so it takes port 0.
    let mut statements = vec![
        let_("x", reference("a"), vec![op("host_enrichment")]),
        run(reference("x"), vec![], emit_output("first_in_tree")),
        run(reference("x"), vec![], emit_output("second_in_tree")),
    ];
    let Statement::Run { pipeline, .. } = &mut statements[1] else {
        unreachable!()
    };
    let Head::Ref(reference) = &mut pipeline.head else {
        unreachable!()
    };
    reference.span = Span::new(100_000 - 1, 100_000);
    reference.name.span = reference.span;
    let (artifact, _) = lower(&" ".repeat(100_000), program(statements), None).unwrap();
    assert_eq!(
        node(&artifact, "x_fanout_0").params,
        params(json!({"output": "second_in_tree"}))
    );
    assert_eq!(
        node(&artifact, "x_fanout_1").params,
        params(json!({"output": "first_in_tree"}))
    );
}

#[test]
fn reused_scope_inputs_and_ports_are_teed() {
    let (artifact, spans) = lower_ok(program(vec![
        run(reference("a"), vec![], drop_()),
        run(reference("a"), vec![], drop_()),
        let_("t", reference("b"), vec![ts()]),
        run(Head::Ref(rp("t", "default")), vec![], drop_()),
        run(Head::Ref(rp("t", "passthrough")), vec![], drop_()),
        run(Head::Ref(rp("t", "passthrough")), vec![], drop_()),
    ]));
    assert_eq!(
        ids(&artifact),
        [
            "input_a_fanout",
            "input_a_fanout_0",
            "input_a_fanout_1",
            "t",
            "t_default",
            "t_passthrough_fanout",
            "t_passthrough_fanout_0",
            "t_passthrough_fanout_1",
        ]
    );
    // A reused scope input's tee has the span of its first use.
    assert!(spans.node("input_a_fanout").is_some());
}

#[test]
fn a_reused_where_let_is_teed_under_the_lets_name() {
    let (artifact, _) = lower_ok(program(vec![
        let_("f", reference("a"), vec![wh("m")]),
        run(reference("f"), vec![], drop_()),
        run(reference("f"), vec![], drop_()),
    ]));
    assert!(edges(&artifact).contains(&"f.true -> f_fanout.in".to_string()));
}

// Spans.

#[test]
fn nodes_parameters_and_edges_have_the_spans_of_what_they_were_written_as() {
    let stage = op_with("host_tags", json!({"tags": ["a:b"], "deadline": "1s"}), None);
    let Stage::Op(written) = &stage else { unreachable!() };
    let (stage_span, deadline_span) = (written.span, written.param_spans[0].1);
    let end = emit_output("o");
    let End::Sink(Sink {
        span: sink_span,
        kind: SinkKind::Output { span: output_span, .. },
        ..
    }) = &end
    else {
        unreachable!()
    };
    let (sink_span, output_span) = (*sink_span, *output_span);
    let (artifact, spans) = lower_ok(program(vec![run(reference("a"), vec![stage], end)]));
    assert_eq!(spans.node("input_a_host_tags"), Some(stage_span));
    assert_eq!(spans.node("input_a_host_tags_out"), Some(sink_span));
    assert_eq!(spans.param("input_a_host_tags", "deadline"), Some(deadline_span));
    assert_eq!(spans.param("input_a_host_tags_out", "output"), Some(output_span));
    // Each edge has the span of the element it enters.
    assert_eq!(
        edges(&artifact),
        [
            "input.a -> input_a_host_tags.in",
            "input_a_host_tags.out -> input_a_host_tags_out.in"
        ]
    );
    assert_eq!(spans.edge(0), Some(stage_span));
    assert_eq!(spans.edge(1), Some(sink_span));
}

#[test]
fn base_ir_version_and_blobs_are_recorded() {
    let mut tree = program(vec![run(reference("a"), vec![], drop_())]);
    tree.base = None;
    tree.ir_version = Some(1);
    let default = ArtifactBase {
        program_hash: BASE.parse().unwrap(),
        outputs: BTreeMap::new(),
    };
    let (artifact, _) = lower(&source(), tree.clone(), Some(&default)).unwrap();
    assert_eq!(artifact.base, default);
    assert_eq!(artifact.ir_version, 1);

    let errors = lower(&source(), tree.clone(), None).unwrap_err();
    assert_error(&errors, tree.close, "Expected `base \"<program hash>\"`");
}

// Errors.

#[test]
fn a_let_used_before_it_is_defined_or_by_itself_is_an_error() {
    let early = r("x");
    let early_span = early.name.span;
    let own = r("y");
    let own_span = own.name.span;
    let errors = lower_err(program(vec![
        run(Head::Ref(early), vec![], drop_()),
        let_("x", reference("a"), vec![op("host_enrichment")]),
        let_("y", Head::Ref(own), vec![op("host_enrichment")]),
        run(reference("x"), vec![], drop_()),
    ]));
    assert_error(&errors, early_span, "`x` is used before it is defined");
    assert!(errors
        .iter()
        .filter(|error| error.span == early_span)
        .all(|error| error.message.contains("programs cannot loop")));
    assert_error(&errors, own_span, "`y` refers to itself");
}

#[test]
fn names_that_collide_are_errors() {
    // let and let; let and as; as and let; as and as.
    let first = let_("x", reference("a"), vec![op("host_enrichment")]);
    let second = let_("x", reference("b"), vec![op("host_enrichment")]);
    let Statement::Let { name: second_name, .. } = &second else {
        unreachable!()
    };
    let second_span = second_name.span;
    let as_x = op_as("host_enrichment", "x");
    let Stage::Op(OpStage {
        id: Some(as_x_name), ..
    }) = &as_x
    else {
        unreachable!()
    };
    let as_x_span = as_x_name.span;
    let as_n = op_as("host_enrichment", "n");
    let as_n_again = op_as("host_enrichment", "n");
    let Stage::Op(OpStage {
        id: Some(as_n_again_name),
        ..
    }) = &as_n_again
    else {
        unreachable!()
    };
    let as_n_again_span = as_n_again_name.span;
    let let_n = let_("n", reference("c"), vec![op("host_enrichment")]);
    let Statement::Let { name: let_n_name, .. } = &let_n else {
        unreachable!()
    };
    let let_n_span = let_n_name.span;

    let errors = lower_err(program(vec![
        first,
        second,
        run(reference("x"), vec![as_x], drop_()),
        run(reference("d"), vec![as_n], drop_()),
        run(reference("e"), vec![as_n_again], drop_()),
        let_n,
        run(reference("n"), vec![], drop_()),
    ]));
    assert_error(&errors, second_span, "`x` is defined twice");
    assert_error(&errors, as_x_span, "`x` is already the name of the `let`");
    assert_error(&errors, as_n_again_span, "Two nodes are named `n`");
    assert_error(&errors, let_n_span, "`n` is already the ID of the node");
}

#[test]
fn a_derived_id_that_collides_with_another_node_is_an_error_that_asks_for_as() {
    // `x | host_enrichment | drop` derives `input_x_host_enrichment`, which a later `as` takes.
    let derived = op("host_enrichment");
    let derived_span = stage_span(&derived);
    let other = op("host_enrichment");
    let other_span = stage_span(&other);
    let errors = lower_err(program(vec![
        run(reference("x"), vec![derived], drop_()),
        run(
            reference("y"),
            vec![op_as("host_enrichment", "input_x_host_enrichment")],
            drop_(),
        ),
        // Two derived IDs that collide: `b | host_enrichment` from the scope input `b`, and from a `let` named
        // `input_b`.
        run(reference("b"), vec![op("host_enrichment")], drop_()),
        let_("input_b", reference("c"), vec![op("host_enrichment")]),
        run(reference("input_b"), vec![other], drop_()),
    ]));
    assert_error(
        &errors,
        derived_span,
        "would be the node `input_x_host_enrichment`, which is already the ID of the node named",
    );
    assert_error(
        &errors,
        other_span,
        "would be the node `input_b_host_enrichment`, which is already the node at",
    );
    assert!(
        errors
            .iter()
            .all(|error| !error.message.contains("host_enrichment_out")),
        "{:#?}",
        errors
    );
}

#[test]
fn a_derived_id_may_not_take_a_lets_name() {
    let derived = op("host_enrichment");
    let derived_span = stage_span(&derived);
    let errors = lower_err(program(vec![
        run(reference("a"), vec![derived], drop_()),
        let_("input_a_host_enrichment", reference("b"), vec![op("host_enrichment")]),
        run(reference("input_a_host_enrichment"), vec![], drop_()),
    ]));
    assert_error(&errors, derived_span, "which is already the name of the `let` at");
    assert_error(&errors, derived_span, "Name one of them with `as`");
}

#[test]
fn a_let_without_stages_is_an_error() {
    let statement = let_("x", reference("a"), vec![]);
    let Statement::Let { span, .. } = &statement else {
        unreachable!()
    };
    let span = *span;
    let errors = lower_err(program(vec![statement, run(reference("x"), vec![], drop_())]));
    assert_error(&errors, span, "`let x = a` names no node");
}

#[test]
fn an_as_on_an_exit_is_an_error() {
    let exit = sink(
        SinkKind::Exit {
            name: "rest".parse().unwrap(),
            span: sp(),
        },
        Some("e"),
    );
    let End::Sink(Sink { id: Some(as_id), .. }) = &exit else {
        unreachable!()
    };
    let span = as_id.span;
    let errors = lower_err(program(vec![run(reference("a"), vec![], exit)]));
    assert_error(&errors, span, "An `exit` leaves the scope and makes no node");
}

#[test]
fn unused_lets_and_ports_are_errors_with_a_fix() {
    let unused = let_("u", reference("a"), vec![op("host_enrichment")]);
    let Statement::Let { name: unused_name, .. } = &unused else {
        unreachable!()
    };
    let unused_span = unused_name.span;
    let split = let_("t", reference("b"), vec![ts()]);
    let Statement::Let { name: split_name, .. } = &split else {
        unreachable!()
    };
    let split_span = split_name.span;
    let errors = lower_err(program(vec![
        unused,
        split,
        run(Head::Ref(rp("t", "default")), vec![], drop_()),
    ]));
    assert_eq!(errors.len(), 2, "{:#?}", errors);
    assert_error(&errors, unused_span, "`u` is never used");
    assert_error(
        &errors,
        split_span,
        "`t.passthrough` is never used; write `t.passthrough | drop`",
    );
}

#[test]
fn a_bare_reference_to_a_node_with_several_ports_must_pick_one() {
    let bare = r("t");
    let span = bare.span;
    let errors = lower_err(program(vec![
        let_("t", reference("a"), vec![ts()]),
        run(Head::Ref(bare), vec![], drop_()),
    ]));
    assert_eq!(errors.len(), 1, "{:#?}", errors);
    assert_error(&errors, span, "pick a port: `t.default` or `t.passthrough`");
}

#[test]
fn a_reference_to_a_port_that_does_not_exist_is_an_error() {
    let missing = rp("t", "nope");
    let missing_span = missing.port.as_ref().unwrap().1;
    let single = rp("x", "out");
    let single_span = single.port.as_ref().unwrap().1;
    let input = rp("a", "p");
    let input_span = input.port.as_ref().unwrap().1;
    let filtered = rp("f", "true");
    let filtered_span = filtered.port.as_ref().unwrap().1;
    let errors = lower_err(program(vec![
        let_("t", reference("b"), vec![ts()]),
        run(Head::Ref(rp("t", "default")), vec![], drop_()),
        run(Head::Ref(rp("t", "passthrough")), vec![], drop_()),
        run(Head::Ref(missing), vec![], drop_()),
        let_("x", reference("c"), vec![op("host_enrichment")]),
        run(Head::Ref(single), vec![], drop_()),
        run(Head::Ref(input), vec![], drop_()),
        let_("f", reference("d"), vec![wh("m")]),
        run(Head::Ref(filtered), vec![], drop_()),
    ]));
    assert_error(
        &errors,
        missing_span,
        "has no output port `nope`; pick one of `t.default` or `t.passthrough`",
    );
    assert_error(
        &errors,
        single_span,
        "which has one output port, so write `x` without a port",
    );
    assert_error(&errors, input_span, "`a` is a scope input, which has no ports");
    assert_error(&errors, filtered_span, "`f` ends in `where`, so it is one stream");
}

#[test]
fn a_pipeline_cannot_continue_from_a_node_with_several_output_ports_or_enter_one_with_several_inputs() {
    let after = op("host_enrichment");
    let after_span = stage_span(&after);
    let merge = op_with("merge", json!({"inputs": 2}), None);
    let merge_span = stage_span(&merge);
    let errors = lower_err(program(vec![
        run(reference("a"), vec![ts(), after], drop_()),
        run(reference("b"), vec![merge], drop_()),
    ]));
    assert_error(
        &errors,
        after_span,
        "`timestamp_split` has two output ports, `default` and `passthrough`; name it with a `let` and use \
         `s.default` and `s.passthrough`.",
    );
    assert_error(
        &errors,
        merge_span,
        "`merge` has 2 input ports, so a pipeline cannot enter it",
    );
}

#[test]
fn every_problem_is_reported_together() {
    let errors = lower_err(program(vec![
        let_("u", reference("a"), vec![op("host_enrichment")]),
        let_("t", reference("b"), vec![ts()]),
        run(reference("t"), vec![], drop_()),
        run(reference("later"), vec![], drop_()),
        let_("later", reference("c"), vec![op("host_enrichment")]),
    ]));
    assert_eq!(errors.len(), 4, "{:#?}", errors);
}

#[test]
fn the_size_limit_is_checked() {
    let huge = json!({"name": {"in": (0..200_000).map(|i| format!("metric.{:07}", i)).collect::<Vec<_>>()}});
    let errors = lower_err(program(vec![run(
        reference("a"),
        vec![where_with(huge, None)],
        drop_(),
    )]));
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0].message.contains("more than the artifact size limit"),
        "{}",
        errors[0].message
    );
}

// The default program.

const DEFAULT_EXPORT: &str = include_str!("../../../saluki-metric-plan/fixtures/export/default-program-metrics.json");

/// Returns the default program's export, which is in normal form: its splits drop their `false` port.
fn normalized_export() -> PolicyArtifact {
    PolicyArtifact::from_json(DEFAULT_EXPORT).unwrap()
}

/// Returns the default program's export as native lowering writes it, before normalization's split polarity rule:
/// the two splits that drop their `false` port turned around, their predicates' `not` removed, their ports swapped,
/// and their drop terminals renamed, in canonical order.
fn native_export() -> PolicyArtifact {
    let mut document: Value = serde_json::from_str(DEFAULT_EXPORT).unwrap();
    let flipped = ["dsd_prefix_filter_blocklist", "dsd_post_agg_filter"];
    for node in document["nodes"].as_array_mut().unwrap() {
        let id = node["id"].as_str().unwrap().to_string();
        if flipped.contains(&id.as_str()) {
            let predicate = node["params"]["predicate"]["not"].take();
            assert!(!predicate.is_null(), "the normalized predicate of `{}` is a `not`", id);
            node["params"]["predicate"] = predicate;
        }
        if let Some(parent) = id.strip_suffix("_false") {
            if flipped.contains(&parent) {
                node["id"] = json!(format!("{}_true", parent));
            }
        }
    }
    for edge in document["edges"].as_array_mut().unwrap() {
        for (end, swap) in [("from", true), ("to", false)] {
            let text = edge[end].as_str().unwrap().to_string();
            let mut text = text;
            for parent in flipped {
                if swap && text == format!("{}.true", parent) {
                    text = format!("{}.false", parent);
                } else if swap && text == format!("{}.false", parent) {
                    text = format!("{}.true", parent);
                } else if text == format!("{}_false.in", parent) {
                    text = format!("{}_true.in", parent);
                }
            }
            edge[end] = json!(text);
        }
    }
    let artifact = PolicyArtifact::from_value(document).unwrap();
    let shapes = artifact
        .nodes
        .iter()
        .map(|node| NodeShape::of(&node.op, &node.params))
        .collect::<Vec<_>>();
    let order = canonical_order(&artifact.nodes, &artifact.edges, &shapes);
    PolicyArtifact {
        nodes: order.nodes.iter().map(|&i| artifact.nodes[i].clone()).collect(),
        edges: order.edges.iter().map(|&i| artifact.edges[i].clone()).collect(),
        ..artifact
    }
}

/// Returns the parameters of the export's node `id`, written as an operator stage's arguments.
fn export_params(export: &PolicyArtifact, value: &str) -> Value {
    serde_json::to_value(&node(export, value).params).unwrap()
}

fn export_stage(export: &PolicyArtifact, op: &str, value: &str) -> Stage {
    op_with(op, export_params(export, value), Some(value))
}

fn export_where(export: &PolicyArtifact, value: &str, as_id: Option<&str>) -> Stage {
    where_with(export_params(export, value)["predicate"].clone(), as_id)
}

/// The default program as the design document prints it, with the export's parameters:
///
/// ```text
/// let dsd_no_agg_split = dogstatsd
///   | dogstatsd_mapper(...) as dsd_enrich
///   | namespace_prefix(...) as dsd_prefix_filter
///   | where not ... as dsd_prefix_filter_blocklist
///   | timestamp_split(...)
/// let dsd_agg = dsd_no_agg_split.default | tag_projection(...) as dsd_tag_filterlist | aggregate(...)
/// let dsd_post_agg_filter = (dsd_agg, dsd_no_agg_split.passthrough) | where not ...
/// dsd_post_agg_filter | emit output dsd_client_telemetry
/// (dsd_post_agg_filter, otlp, checks, liveness, trace_sampler) as metrics_enrich_in
///   | host_enrichment as metrics_enrich
///   | host_tags(...) as metrics_enrich_host_tags
///   | fork as metrics_policy { emit output dd_metrics_encode; where ... as metrics_routing_filter_0 | emit ...; ... }
/// ```
fn default_program_tree(export: &PolicyArtifact) -> Program {
    let routed =
        |filter: &str, output: &str| branch(vec![export_where(export, filter, Some(filter))], emit_output(output));
    let mut tree = program(vec![
        let_(
            "dsd_no_agg_split",
            reference("dogstatsd"),
            vec![
                export_stage(export, "dogstatsd_mapper", "dsd_enrich"),
                export_stage(export, "namespace_prefix", "dsd_prefix_filter"),
                export_where(
                    export,
                    "dsd_prefix_filter_blocklist",
                    Some("dsd_prefix_filter_blocklist"),
                ),
                op_with("timestamp_split", export_params(export, "dsd_no_agg_split"), None),
            ],
        ),
        let_(
            "dsd_agg",
            Head::Ref(rp("dsd_no_agg_split", "default")),
            vec![
                export_stage(export, "tag_projection", "dsd_tag_filterlist"),
                op_with("aggregate", export_params(export, "dsd_agg"), None),
            ],
        ),
        let_(
            "dsd_post_agg_filter",
            tuple(vec![r("dsd_agg"), rp("dsd_no_agg_split", "passthrough")], None),
            vec![export_where(export, "dsd_post_agg_filter", None)],
        ),
        run(
            reference("dsd_post_agg_filter"),
            vec![],
            emit_output("dsd_client_telemetry"),
        ),
        run(
            tuple(
                vec![
                    r("dsd_post_agg_filter"),
                    r("otlp"),
                    r("checks"),
                    r("liveness"),
                    r("trace_sampler"),
                ],
                Some("metrics_enrich_in"),
            ),
            vec![
                op_as("host_enrichment", "metrics_enrich"),
                export_stage(export, "host_tags", "metrics_enrich_host_tags"),
            ],
            fork(
                Some("metrics_policy"),
                vec![
                    branch(vec![], emit_output("dd_metrics_encode")),
                    routed("metrics_routing_filter_0", "metrics_routing_encode_0"),
                    routed("metrics_routing_filter_1", "metrics_routing_encode_1"),
                    routed("mrf_metrics_gateway", "mrf_metrics_encode"),
                    routed("af_metrics_gateway", "af_metrics_encode"),
                ],
            ),
        ),
    ]);
    tree.base = None;
    tree
}

#[test]
fn the_default_program_lowers_to_its_normalized_export_with_the_native_ids() {
    let export = normalized_export();
    let (artifact, spans) = lower(&source(), default_program_tree(&export), Some(&export.base)).unwrap();
    assert_well_formed(&artifact);

    for native in [
        "dsd_no_agg_split",
        "dsd_prefix_filter_blocklist_false",
        "dsd_agg",
        "dsd_post_agg_filter_in",
        "dsd_post_agg_filter",
        "dsd_post_agg_filter_false",
        "dsd_post_agg_filter_fanout",
        "dsd_post_agg_filter_fanout_0",
        "metrics_policy_0",
        "metrics_routing_filter_0_true",
        "metrics_routing_filter_0_false",
        "af_metrics_gateway_false",
    ] {
        assert!(ids(&artifact).contains(&native), "{} in {:?}", native, ids(&artifact));
    }
    let set = |artifact: &PolicyArtifact| {
        (
            artifact
                .nodes
                .iter()
                .map(|node| serde_json::to_string(node).unwrap())
                .collect::<BTreeSet<_>>(),
            edges(artifact).into_iter().collect::<BTreeSet<_>>(),
        )
    };
    assert_eq!(set(&artifact), set(&export));
    assert_eq!(
        (artifact.scope.clone(), artifact.base.clone()),
        (export.scope.clone(), export.base.clone())
    );

    // Every node and edge has a span.
    for node in &artifact.nodes {
        assert!(spans.node(node.id.as_str()).is_some(), "{}", node.id);
    }
    assert!((0..artifact.edges.len()).all(|i| spans.edge(i).is_some()));
}

/// The default program exactly as native lowering writes it, in the core forms where the export is not in normal
/// form: its two splits whose `true` port is dropped, and the tee after the second, written explicitly.
fn default_program_core_tree(export: &PolicyArtifact) -> Program {
    let split = |value: &str| op_with("split", export_params(export, value), None);
    let routed =
        |filter: &str, output: &str| branch(vec![export_where(export, filter, Some(filter))], emit_output(output));
    let mut tree = program(vec![
        let_(
            "dsd_prefix_filter_blocklist",
            reference("dogstatsd"),
            vec![
                export_stage(export, "dogstatsd_mapper", "dsd_enrich"),
                export_stage(export, "namespace_prefix", "dsd_prefix_filter"),
                split("dsd_prefix_filter_blocklist"),
            ],
        ),
        run(Head::Ref(rp("dsd_prefix_filter_blocklist", "true")), vec![], drop_()),
        let_(
            "dsd_no_agg_split",
            Head::Ref(rp("dsd_prefix_filter_blocklist", "false")),
            vec![op_with(
                "timestamp_split",
                export_params(export, "dsd_no_agg_split"),
                None,
            )],
        ),
        let_(
            "dsd_agg",
            Head::Ref(rp("dsd_no_agg_split", "default")),
            vec![
                export_stage(export, "tag_projection", "dsd_tag_filterlist"),
                op_with("aggregate", export_params(export, "dsd_agg"), None),
            ],
        ),
        let_(
            "dsd_post_agg_filter",
            tuple(vec![r("dsd_agg"), rp("dsd_no_agg_split", "passthrough")], None),
            vec![split("dsd_post_agg_filter")],
        ),
        run(Head::Ref(rp("dsd_post_agg_filter", "true")), vec![], drop_()),
        let_(
            "dsd_post_agg_filter_fanout",
            Head::Ref(rp("dsd_post_agg_filter", "false")),
            vec![op_with("tee", json!({"ports": 2}), None)],
        ),
        run(
            Head::Ref(rp("dsd_post_agg_filter_fanout", "0")),
            vec![],
            emit_output("dsd_client_telemetry"),
        ),
        run(
            tuple(
                vec![
                    rp("dsd_post_agg_filter_fanout", "1"),
                    r("otlp"),
                    r("checks"),
                    r("liveness"),
                    r("trace_sampler"),
                ],
                Some("metrics_enrich_in"),
            ),
            vec![
                op_as("host_enrichment", "metrics_enrich"),
                export_stage(export, "host_tags", "metrics_enrich_host_tags"),
            ],
            fork(
                Some("metrics_policy"),
                vec![
                    branch(vec![], emit_output("dd_metrics_encode")),
                    routed("metrics_routing_filter_0", "metrics_routing_encode_0"),
                    routed("metrics_routing_filter_1", "metrics_routing_encode_1"),
                    routed("mrf_metrics_gateway", "mrf_metrics_encode"),
                    routed("af_metrics_gateway", "af_metrics_encode"),
                ],
            ),
        ),
    ]);
    tree.base = None;
    tree
}

#[test]
fn the_default_programs_core_form_lowers_to_its_export_exactly_and_composes_into_a_valid_plan() {
    use saluki_metric_plan::{
        compose::{compose, receive_artifact, Approval},
        ir::Plan,
        testing::{default_program_catalog, DEFAULT_PROGRAM_JSON},
    };

    let export = native_export();
    let (artifact, _) = lower(&source(), default_program_core_tree(&export), Some(&export.base)).unwrap();
    assert_well_formed(&artifact);
    // The same nodes, in the same order, and the same edges. Canonical order takes the edges from scope inputs by
    // input name, where native lowering writes them as it meets them.
    assert_eq!(artifact.nodes, export.nodes);
    let sorted = |artifact: &PolicyArtifact| edges(artifact).into_iter().collect::<BTreeSet<_>>();
    assert_eq!(sorted(&artifact), sorted(&export));
    assert_eq!(artifact.edges.len(), export.edges.len());
    assert_eq!(
        (&artifact.scope, &artifact.base, artifact.ir_version),
        (&export.scope, &export.base, export.ir_version)
    );

    let plan = Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap();
    let received = receive_artifact(serde_json::to_value(&artifact).unwrap()).unwrap();
    let approval = Approval {
        revision: 1,
        approved_against: None,
        active_plan_hash: None,
        in_effect: false,
    };
    let composed = compose(
        &plan,
        &received,
        &approval,
        &default_program_catalog(),
        &OperatorRegistry::native(),
    )
    .unwrap();
    assert!(composed.report.is_valid(), "{}", composed.report);
    assert!(composed.report.findings().is_empty(), "{}", composed.report);
}

#[test]
fn the_default_program_without_as_clauses_derives_ids_from_lets_and_anchors() {
    let export = normalized_export();
    let mut tree = default_program_tree(&export);
    // Strip every `as` clause.
    fn strip_stages(stages: &mut [Stage]) {
        for stage in stages {
            match stage {
                Stage::Op(stage) => stage.id = None,
                Stage::Where(stage) => stage.id = None,
            }
        }
    }
    fn strip_end(end: &mut End) {
        if let End::Fork(fork) = end {
            fork.id = None;
            for branch in &mut fork.branches {
                strip_stages(&mut branch.stages);
                strip_end(&mut branch.end);
            }
        }
    }
    for statement in &mut tree.statements {
        let pipeline = match statement {
            Statement::Let { pipeline, .. } | Statement::Run { pipeline, .. } => pipeline,
        };
        if let Head::Tuple(tuple) = &mut pipeline.head {
            tuple.id = None;
        }
        strip_stages(&mut pipeline.stages);
        if let Some(end) = &mut pipeline.end {
            strip_end(end);
        }
    }
    let (artifact, _) = lower(&source(), tree, Some(&export.base)).unwrap();
    assert_well_formed(&artifact);
    let ids = ids(&artifact);
    for derived in [
        "dsd_no_agg_split_dogstatsd_mapper",
        "dsd_no_agg_split_namespace_prefix",
        "dsd_no_agg_split_split",
        "dsd_no_agg_split_split_false",
        "dsd_no_agg_split",
        "dsd_agg_tag_projection",
        "dsd_agg",
        "dsd_post_agg_filter_in",
        "dsd_post_agg_filter_fanout",
        "dsd_post_agg_filter_fanout_0",
        "dsd_post_agg_filter_fanout_1_in",
        "dsd_post_agg_filter_fanout_1_in_host_enrichment",
        "dsd_post_agg_filter_fanout_1_in_host_tags",
        "dsd_post_agg_filter_fanout_1_in_fork",
        "dsd_post_agg_filter_fanout_1_in_fork_0",
        "dsd_post_agg_filter_fanout_1_in_fork_1_split",
        "dsd_post_agg_filter_fanout_1_in_fork_1_split_true",
        "dsd_post_agg_filter_fanout_1_in_fork_4_split_false",
    ] {
        assert!(ids.contains(&derived), "{} in {:?}", derived, ids);
    }
}

#[test]
fn a_let_with_a_sink_and_a_statement_without_one_are_errors() {
    let ended = Statement::Let {
        name: name("x"),
        pipeline: pipeline(reference("a"), vec![op("host_enrichment")], Some(drop_())),
        span: sp(),
    };
    let Statement::Let {
        pipeline: ended_pipeline,
        ..
    } = &ended
    else {
        unreachable!()
    };
    let End::Sink(Sink { span: sink_span, .. }) = ended_pipeline.end.as_ref().unwrap() else {
        unreachable!()
    };
    let sink_span = *sink_span;
    let open = Statement::Run {
        pipeline: pipeline(reference("b"), vec![op("host_enrichment")], None),
        span: sp(),
    };
    let Statement::Run { span: open_span, .. } = &open else {
        unreachable!()
    };
    let open_span = *open_span;
    let errors = lower_err(program(vec![ended, run(reference("x"), vec![], drop_()), open]));
    assert_error(
        &errors,
        sink_span,
        "A `let` names a stream, so its pipeline cannot end in a sink",
    );
    assert_error(&errors, open_span, "This statement does not end in a sink or `fork`");
}

#[test]
fn an_unknown_operator_is_assumed_to_have_one_input_and_one_output() {
    let (artifact, _) = lower(
        &source(),
        program(vec![run(reference("a"), vec![op("frobnicate")], drop_())]),
        None,
    )
    .unwrap();
    assert_eq!(
        edges(&artifact),
        [
            "input.a -> input_a_frobnicate.in",
            "input_a_frobnicate.out -> input_a_frobnicate_out.in"
        ]
    );
}

proptest::proptest! {
    /// However the references to a stream are laid out in the source, the implicit tee's port `i` feeds the `i`th of
    /// them in source order.
    #[test]
    fn property_test_implicit_tee_ports_follow_source_order(
        starts in proptest::strategy::Strategy::prop_shuffle(
            proptest::sample::subsequence((1..200usize).map(|i| i * 10).collect::<Vec<_>>(), 2..8)
        )
    ) {
        let mut statements = vec![let_("x", reference("a"), vec![op("host_enrichment")])];
        for (i, start) in starts.iter().enumerate() {
            let mut head = r("x");
            head.span = Span::new(*start, start + 1);
            head.name.span = head.span;
            statements.push(run(Head::Ref(head), vec![], emit_output(&format!("o{}", i))));
        }
        let (artifact, _) = lower(&source(), program(statements), None).unwrap();
        let mut by_start = starts.iter().enumerate().collect::<Vec<_>>();
        by_start.sort_by_key(|(_, start)| **start);
        for (port, (written, _)) in by_start.into_iter().enumerate() {
            let sink = node(&artifact, &format!("x_fanout_{}", port));
            proptest::prop_assert_eq!(&sink.params, &params(json!({"output": format!("o{}", written)})));
        }
    }
}
