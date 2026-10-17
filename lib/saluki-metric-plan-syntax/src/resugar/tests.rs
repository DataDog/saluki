//! Tests of resugaring: the tree each graph shape gives, written compactly so that a test reads as the program.
//!
//! Lowering is tested on its own; these tests pin the forms resugaring chooses, where it falls back to the core form,
//! where a `let` names a node and where only an `as` can, and the order of statements.

use proptest::prelude::*;
use saluki_metric_plan::ir::{ParamType, PolicyArtifact};
use serde_json::{json, Value};

use super::*;

const BASE: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// Builds an artifact from `(id, op, params)` nodes and `(from, to)` edges, without the checks of reading an artifact
/// document, so that tests can build artifacts that are not well-formed.
fn artifact(nodes: &[(&str, &str, Value)], edges: &[(&str, &str)]) -> PolicyArtifact {
    let nodes = nodes
        .iter()
        .map(|(id, op, params)| json!({"id": id, "op": op, "params": params}))
        .collect::<Vec<_>>();
    let edges = edges
        .iter()
        .map(|(from, to)| json!({"from": from, "to": to}))
        .collect::<Vec<_>>();
    serde_json::from_value(json!({
        "ir_version": IR_VERSION,
        "scope": "metrics",
        "base": {"program_hash": BASE},
        "nodes": nodes,
        "edges": edges,
    }))
    .expect("the test artifact has the artifact's shape")
}

/// Resugars with the default options and writes each statement compactly.
fn statements(artifact: &PolicyArtifact) -> Vec<String> {
    resugar(artifact, &PrintOptions::default())
        .statements
        .iter()
        .map(statement)
        .collect()
}

fn statement(statement: &Statement) -> String {
    match statement {
        Statement::Let { name, pipeline, .. } => format!("let {} = {}", name.id, pipeline_text(pipeline)),
        Statement::Run { pipeline, .. } => pipeline_text(pipeline),
    }
}

fn pipeline_text(pipeline: &Pipeline) -> String {
    let mut parts = vec![match &pipeline.head {
        Head::Ref(head) => ref_text(head),
        Head::Tuple(tuple) => format!(
            "({}){}",
            tuple.items.iter().map(ref_text).collect::<Vec<_>>().join(", "),
            as_text(&tuple.id)
        ),
    }];
    parts.extend(pipeline.stages.iter().map(stage_text));
    if let Some(end) = &pipeline.end {
        parts.push(end_text(end));
    }
    parts.join(" | ")
}

fn ref_text(head: &Ref) -> String {
    match &head.port {
        Some((port, _)) => format!("{}.{}", head.name.id, port),
        None => head.name.id.to_string(),
    }
}

fn as_text(id: &Option<Name>) -> String {
    id.as_ref().map(|id| format!(" as {}", id.id)).unwrap_or_default()
}

thread_local! {
    /// Whether to write parameters and predicates as `...`, for outlines of large programs.
    static BRIEF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn stage_text(stage: &Stage) -> String {
    let brief = BRIEF.with(|brief| brief.get());
    match stage {
        Stage::Op(stage) if brief => {
            let args = if stage.params.is_empty() { "" } else { "(...)" };
            format!("{}{}{}", stage.op, args, as_text(&stage.id))
        }
        Stage::Where(stage) if brief => format!("where ...{}", as_text(&stage.id)),
        Stage::Op(stage) => {
            let mut args = Vec::new();
            if let Some(positional) = &stage.positional {
                args.push(stage.params.get(positional.as_str()).unwrap().to_string());
            }
            for (name, value) in stage.params.iter() {
                if Some(name) != stage.positional.as_ref() {
                    args.push(format!("{}: {}", name, value));
                }
            }
            let args = if args.is_empty() {
                String::new()
            } else {
                format!("({})", args.join(", "))
            };
            format!("{}{}{}", stage.op, args, as_text(&stage.id))
        }
        Stage::Where(stage) => format!("where {}{}", stage.predicate, as_text(&stage.id)),
    }
}

fn end_text(sink: &Sink) -> String {
    let kind = match &sink.kind {
        SinkKind::Recipients { names, .. } => format!("emit [{}]", names.join(", ")),
        SinkKind::Output { output, .. } => format!("emit output {}", output),
        SinkKind::Drop => "drop".to_string(),
        SinkKind::Exit { name, .. } => format!("exit {}", name),
    };
    format!("{}{}", kind, as_text(&sink.id))
}

fn emit(output: &str) -> Value {
    json!({"output": output})
}

fn none() -> Value {
    json!({})
}

fn predicate(name: &str) -> Value {
    json!({"predicate": {"name": {"eq": name}}})
}

const P: &str = r#"{"name":{"eq":"a"}}"#;

// Unregistered operators, such as `alpha`, have unknown ports, which resugaring takes to be `in` and `out` unless
// edges name others: they make plain stages without parameters.

#[test]
fn a_chain_with_derived_ids_has_no_as() {
    let a = artifact(
        &[
            ("input_dogstatsd_alpha", "alpha", none()),
            ("input_dogstatsd_beta", "beta", none()),
            ("input_dogstatsd_alpha_2", "alpha", none()),
            ("input_dogstatsd_alpha_2_out", "emit", emit("o")),
        ],
        &[
            ("input.dogstatsd", "input_dogstatsd_alpha.in"),
            ("input_dogstatsd_alpha.out", "input_dogstatsd_beta.in"),
            ("input_dogstatsd_beta.out", "input_dogstatsd_alpha_2.in"),
            ("input_dogstatsd_alpha_2.out", "input_dogstatsd_alpha_2_out.in"),
        ],
    );
    assert_eq!(statements(&a), ["dogstatsd | alpha | beta | alpha | emit output o"]);
}

#[test]
fn a_node_whose_id_is_not_derived_ends_a_let_and_only_a_sink_takes_an_as() {
    // `first` ends a `let`, so the second `alpha`, which lowering would derive as `input_dogstatsd_alpha_2` after an
    // `alpha as first`, is no longer derived after it either and ends a `let` of its own. The sink's ID cannot be a
    // `let`'s name, so only it takes an `as`.
    let a = artifact(
        &[
            ("first", "alpha", none()),
            ("input_dogstatsd_alpha_2", "alpha", none()),
            ("sink", "drop", none()),
        ],
        &[
            ("input.dogstatsd", "first.in"),
            ("first.out", "input_dogstatsd_alpha_2.in"),
            ("input_dogstatsd_alpha_2.out", "sink.in"),
        ],
    );
    assert_eq!(
        statements(&a),
        [
            "let first = dogstatsd | alpha",
            "let input_dogstatsd_alpha_2 = first | alpha",
            "input_dogstatsd_alpha_2 | drop as sink",
        ]
    );
    assert_lowers_back(&a);
}

#[test]
fn a_sink_on_a_scope_input_derives_from_the_input() {
    let a = artifact(
        &[(
            "input_otlp",
            "emit",
            json!({"to": ["primary", "https://b.example.com"]}),
        )],
        &[("input.otlp", "input_otlp.in")],
    );
    assert_eq!(statements(&a), ["otlp | emit [primary, https://b.example.com]"]);
}

#[test]
fn an_exit_is_a_sink_without_a_node() {
    let a = artifact(
        &[("input_dogstatsd_alpha", "alpha", none())],
        &[
            ("input.dogstatsd", "input_dogstatsd_alpha.in"),
            ("input_dogstatsd_alpha.out", "exit.done"),
        ],
    );
    assert_eq!(statements(&a), ["dogstatsd | alpha | exit done"]);
}

#[test]
fn a_split_whose_false_port_drops_is_where() {
    let a = artifact(
        &[
            ("input_dogstatsd_split", "split", predicate("a")),
            ("input_dogstatsd_split_false", "drop", none()),
            ("input_dogstatsd_split_true", "emit", emit("o")),
        ],
        &[
            ("input.dogstatsd", "input_dogstatsd_split.in"),
            ("input_dogstatsd_split.true", "input_dogstatsd_split_true.in"),
            ("input_dogstatsd_split.false", "input_dogstatsd_split_false.in"),
        ],
    );
    assert_eq!(statements(&a), [format!("dogstatsd | where {} | emit output o", P)]);
}

#[test]
fn a_split_whose_drop_is_not_derived_is_the_core_form() {
    // Where `where` and `split(...)` could both describe the graph, `where` wins only when its drop has the derived
    // ID, here `s_false`; otherwise the split is a `let` whose ports are used separately.
    let a = artifact(
        &[
            ("s", "split", predicate("a")),
            ("dropped", "drop", none()),
            ("s_true", "emit", emit("o")),
        ],
        &[
            ("input.dogstatsd", "s.in"),
            ("s.true", "s_true.in"),
            ("s.false", "dropped.in"),
        ],
    );
    assert_eq!(
        statements(&a),
        [
            format!("let s = dogstatsd | split({})", P),
            "s.true | emit output o".to_string(),
            "s.false | drop as dropped".to_string(),
        ]
    );
}

#[test]
fn a_split_that_drops_on_true_is_the_core_form() {
    // The native polarity: until normalization swaps it, a split dropping matches is not a `where`.
    let a = artifact(
        &[
            ("s", "split", predicate("a")),
            ("s_true", "drop", none()),
            ("s_false", "emit", emit("o")),
        ],
        &[
            ("input.dogstatsd", "s.in"),
            ("s.true", "s_true.in"),
            ("s.false", "s_false.in"),
        ],
    );
    assert_eq!(
        statements(&a),
        [
            format!("let s = dogstatsd | split({})", P),
            "s.true | drop".to_string(),
            "s.false | emit output o".to_string(),
        ]
    );
}

#[test]
fn a_tee_whose_ports_end_in_sinks_is_the_core_form() {
    // Each port of the tee starts a statement, whose stages derive from the port.
    let a = artifact(
        &[
            ("t", "tee", json!({"ports": 3})),
            ("t_0", "emit", emit("a")),
            ("t_1_split", "split", predicate("a")),
            ("t_1_split_false", "drop", none()),
            ("t_1_split_true", "emit", emit("b")),
            ("t_2_split", "split", predicate("a")),
            ("t_2_split_false", "drop", none()),
            ("t_2_split_true", "drop", none()),
        ],
        &[
            ("input.dogstatsd", "t.in"),
            ("t.0", "t_0.in"),
            ("t.1", "t_1_split.in"),
            ("t_1_split.true", "t_1_split_true.in"),
            ("t_1_split.false", "t_1_split_false.in"),
            ("t.2", "t_2_split.in"),
            ("t_2_split.true", "t_2_split_true.in"),
            ("t_2_split.false", "t_2_split_false.in"),
        ],
    );
    assert_eq!(
        statements(&a),
        [
            "let t = dogstatsd | tee(ports: 3)".to_string(),
            "t.0 | emit output a".to_string(),
            format!("t.1 | where {} | emit output b", P),
            format!("t.2 | where {} | drop", P),
        ]
    );
    assert_lowers_back(&a);
}

#[test]
fn a_tee_after_a_tee_is_a_let_of_its_own() {
    let a = artifact(
        &[
            ("policy", "tee", json!({"ports": 2})),
            ("policy_0", "emit", emit("a")),
            ("policy_1_fork", "tee", json!({"ports": 2})),
            ("policy_1_fork_0", "emit", emit("b")),
            ("policy_1_fork_1", "drop", none()),
        ],
        &[
            ("input.dogstatsd", "policy.in"),
            ("policy.0", "policy_0.in"),
            ("policy.1", "policy_1_fork.in"),
            ("policy_1_fork.0", "policy_1_fork_0.in"),
            ("policy_1_fork.1", "policy_1_fork_1.in"),
        ],
    );
    assert_eq!(
        statements(&a),
        [
            "let policy = dogstatsd | tee(ports: 2)",
            "policy.0 | emit output a",
            "let policy_1_fork = policy.1 | tee(ports: 2)",
            "policy_1_fork.0 | emit output b",
            "policy_1_fork.1 | drop",
        ]
    );
}

#[test]
fn a_fanout_tee_whose_uses_all_end_in_sinks_is_reuse() {
    // Reuse is the only way to copy a stream, whatever its copies lead to.
    let a = artifact(
        &[
            ("x", "alpha", none()),
            ("x_fanout", "tee", json!({"ports": 2})),
            ("x_fanout_0", "emit", emit("a")),
            ("x_fanout_1", "emit", emit("b")),
        ],
        &[
            ("input.dogstatsd", "x.in"),
            ("x.out", "x_fanout.in"),
            ("x_fanout.0", "x_fanout_0.in"),
            ("x_fanout.1", "x_fanout_1.in"),
        ],
    );
    assert_eq!(
        statements(&a),
        ["let x = dogstatsd | alpha", "x | emit output a", "x | emit output b"]
    );
}

/// `x` used twice through `x_fanout`: by a sink on port `sink_port`, and by a tuple with `otlp` on the other port.
fn reused(sink_port: usize) -> PolicyArtifact {
    let tuple_port = 1 - sink_port;
    let sink_edge = format!("x_fanout.{}", sink_port);
    let tuple_edge = format!("x_fanout.{}", tuple_port);
    let sink_id = format!("x_fanout_{}", sink_port);
    let sink_in = format!("{}.in", sink_id);
    let merge_id = format!("x_fanout_{}_in", tuple_port);
    let merge_in = format!("{}.0", merge_id);
    let merge_out = format!("{}.out", merge_id);
    artifact(
        &[
            ("x", "alpha", none()),
            ("x_fanout", "tee", json!({"ports": 2})),
            (&sink_id, "emit", emit("a")),
            (&merge_id, "merge", json!({"inputs": 2})),
            ("joined", "beta", none()),
            ("joined_out", "emit", emit("b")),
        ],
        &[
            ("input.dogstatsd", "x.in"),
            ("x.out", "x_fanout.in"),
            (&sink_edge, &sink_in),
            (&tuple_edge, &merge_in),
            ("input.otlp", &format!("{}.1", merge_id)),
            (&merge_out, "joined.in"),
            ("joined.out", "joined_out.in"),
        ],
    )
}

#[test]
fn a_fanout_tee_is_reuse_of_a_let() {
    assert_eq!(
        statements(&reused(0)),
        [
            // The tuple's merge has its derived ID, which the stage after it cannot derive from, so the merge ends a
            // `let`, which an `as` would otherwise have to name.
            "let x = dogstatsd | alpha",
            "x | emit output a",
            "let x_fanout_1_in = (x, otlp)",
            "let joined = x_fanout_1_in | beta",
            "joined | emit output b",
        ]
    );
}

#[test]
fn the_uses_of_an_implicit_tee_come_in_port_order() {
    // The tuple feeds port 0, so it comes first, although the sink's edge comes earlier in canonical order.
    assert_eq!(
        statements(&reused(1)),
        [
            "let x = dogstatsd | alpha",
            "let x_fanout_0_in = (x, otlp)",
            "let joined = x_fanout_0_in | beta",
            "joined | emit output b",
            "x | emit output a",
        ]
    );
}

#[test]
fn an_implicit_tee_whose_uses_cannot_be_in_port_order_is_the_core_form() {
    // Port 0 feeds the tuple's second item and port 1 its first, which no source order gives.
    let a = artifact(
        &[
            ("x", "alpha", none()),
            ("x_fanout", "tee", json!({"ports": 2})),
            ("m", "merge", json!({"inputs": 2})),
            ("m_out", "drop", none()),
        ],
        &[
            ("input.dogstatsd", "x.in"),
            ("x.out", "x_fanout.in"),
            ("x_fanout.0", "m.1"),
            ("x_fanout.1", "m.0"),
            ("m.out", "m_out.in"),
        ],
    );
    assert_eq!(
        statements(&a),
        [
            "let x = dogstatsd | alpha",
            "let x_fanout = x | tee(ports: 2)",
            "let m = (x_fanout.1, x_fanout.0)",
            "m | drop",
        ]
    );
}

#[test]
fn the_implicit_tee_written_in_the_core_form_depends_only_on_the_graph() {
    // The uses of `dogstatsd` cannot come in port order: port 0 and port 2 feed one tuple, and port 1 another. The tee
    // written in the core form to break that is the same whatever order the artifact lists its nodes and edges in, and
    // `otlp` stays reused.
    let nodes = [
        ("input_dogstatsd_fanout", "tee", json!({"ports": 3})),
        ("input_otlp_fanout", "tee", json!({"ports": 2})),
        ("m1", "merge", json!({"inputs": 3})),
        ("m1_out", "drop", none()),
        ("m2", "merge", json!({"inputs": 3})),
        ("m2_out", "drop", none()),
    ];
    let edges = [
        ("input.dogstatsd", "input_dogstatsd_fanout.in"),
        ("input.otlp", "input_otlp_fanout.in"),
        ("input_dogstatsd_fanout.0", "m1.0"),
        ("input_otlp_fanout.0", "m1.1"),
        ("input_dogstatsd_fanout.2", "m1.2"),
        ("input.checks", "m2.0"),
        ("input_otlp_fanout.1", "m2.1"),
        ("input_dogstatsd_fanout.1", "m2.2"),
        ("m1.out", "m1_out.in"),
        ("m2.out", "m2_out.in"),
    ];
    let a = artifact(&nodes, &edges);
    let expected = [
        "let input_dogstatsd_fanout = dogstatsd | tee(ports: 3)",
        "let m1 = (input_dogstatsd_fanout.0, otlp, input_dogstatsd_fanout.2)",
        "m1 | drop",
        "let m2 = (checks, otlp, input_dogstatsd_fanout.1)",
        "m2 | drop",
    ];
    assert_eq!(statements(&a), expected);
    let mut reversed = a.clone();
    reversed.nodes.reverse();
    reversed.edges.reverse();
    assert_eq!(statements(&reversed), expected);
    assert_lowers_back(&a);
}

#[test]
fn a_tee_without_the_derived_id_is_the_core_form() {
    let a = artifact(
        &[
            ("x", "alpha", none()),
            ("copies", "tee", json!({"ports": 2})),
            ("copies_0", "emit", emit("a")),
            ("m", "merge", json!({"inputs": 2})),
            ("m_out", "drop", none()),
        ],
        &[
            ("input.dogstatsd", "x.in"),
            ("x.out", "copies.in"),
            ("copies.0", "copies_0.in"),
            ("copies.1", "m.0"),
            ("input.otlp", "m.1"),
            ("m.out", "m_out.in"),
        ],
    );
    assert_eq!(
        statements(&a),
        [
            "let x = dogstatsd | alpha",
            "let copies = x | tee(ports: 2)",
            "copies.0 | emit output a",
            "let m = (copies.1, otlp)",
            "m | drop",
        ]
    );
}

#[test]
fn a_scope_input_used_twice_is_reused_through_its_fanout() {
    let a = artifact(
        &[
            ("input_otlp_fanout", "tee", json!({"ports": 2})),
            ("input_otlp_fanout_0", "emit", emit("a")),
            ("input_otlp_fanout_1_in", "merge", json!({"inputs": 2})),
            ("input_otlp_fanout_1_in_out", "drop", none()),
        ],
        &[
            ("input.otlp", "input_otlp_fanout.in"),
            ("input_otlp_fanout.0", "input_otlp_fanout_0.in"),
            ("input_otlp_fanout.1", "input_otlp_fanout_1_in.0"),
            ("input.checks", "input_otlp_fanout_1_in.1"),
            ("input_otlp_fanout_1_in.out", "input_otlp_fanout_1_in_out.in"),
        ],
    );
    assert_eq!(statements(&a), ["otlp | emit output a", "(otlp, checks) | drop"]);
}

#[test]
fn a_bare_tuple_anchors_its_stages_on_its_merge() {
    // The merge's ID is not derived, so it ends a `let`, and the stages after it still derive from it.
    let a = artifact(
        &[
            ("joined", "merge", json!({"inputs": 2})),
            ("joined_alpha", "alpha", none()),
            ("joined_alpha_out", "drop", none()),
        ],
        &[
            ("input.otlp", "joined.0"),
            ("input.checks", "joined.1"),
            ("joined.out", "joined_alpha.in"),
            ("joined_alpha.out", "joined_alpha_out.in"),
        ],
    );
    assert_eq!(statements(&a), ["let joined = (otlp, checks)", "joined | alpha | drop"]);
    assert_lowers_back(&a);

    let derived = artifact(
        &[
            ("input_otlp_in", "merge", json!({"inputs": 2})),
            ("input_otlp_in_out", "drop", none()),
        ],
        &[
            ("input.otlp", "input_otlp_in.0"),
            ("input.checks", "input_otlp_in.1"),
            ("input_otlp_in.out", "input_otlp_in_out.in"),
        ],
    );
    assert_eq!(statements(&derived), ["(otlp, checks) | drop"]);
}

#[test]
fn a_tuple_used_twice_is_a_let_of_its_own() {
    let a = artifact(
        &[
            ("m", "merge", json!({"inputs": 2})),
            ("m_fanout", "tee", json!({"ports": 2})),
            ("m_fanout_0", "drop", none()),
            ("n", "merge", json!({"inputs": 2})),
            ("n_out", "drop", none()),
        ],
        &[
            ("input.otlp", "m.0"),
            ("input.checks", "m.1"),
            ("m.out", "m_fanout.in"),
            ("m_fanout.0", "m_fanout_0.in"),
            ("m_fanout.1", "n.0"),
            ("input.liveness", "n.1"),
            ("n.out", "n_out.in"),
        ],
    );
    assert_eq!(
        statements(&a),
        [
            "let m = (otlp, checks)",
            "m | drop",
            "let n = (m, liveness)",
            "n | drop"
        ]
    );
}

#[test]
fn a_named_intermediate_stream_starts_a_let() {
    // `let foo = dogstatsd | alpha | beta` and `foo | gamma | drop`: every ID derives once the chain is split at `foo`.
    let a = artifact(
        &[
            ("foo_alpha", "alpha", none()),
            ("foo", "beta", none()),
            ("foo_gamma", "gamma", none()),
            ("foo_gamma_out", "drop", none()),
        ],
        &[
            ("input.dogstatsd", "foo_alpha.in"),
            ("foo_alpha.out", "foo.in"),
            ("foo.out", "foo_gamma.in"),
            ("foo_gamma.out", "foo_gamma_out.in"),
        ],
    );
    assert_eq!(
        statements(&a),
        ["let foo = dogstatsd | alpha | beta", "foo | gamma | drop"]
    );
}

#[test]
fn a_let_names_each_node_whose_id_is_not_derived() {
    // `as` is the last resort: `a` and `b` each end a `let` named for them, which saves both `as` clauses.
    let a = artifact(
        &[("a", "alpha", none()), ("b", "beta", none()), ("b_out", "drop", none())],
        &[("input.dogstatsd", "a.in"), ("a.out", "b.in"), ("b.out", "b_out.in")],
    );
    assert_eq!(
        statements(&a),
        ["let a = dogstatsd | alpha", "let b = a | beta", "b | drop"]
    );
}

#[test]
fn a_let_is_split_before_a_node_with_a_name_of_its_own() {
    // `x_alpha` derives from `x`, and the tee, which does not, ends a `let` of its own.
    let a = artifact(
        &[
            ("x_alpha", "alpha", none()),
            ("x", "beta", none()),
            ("x_copies", "tee", json!({"ports": 2})),
            ("x_copies_0", "drop", none()),
            ("x_copies_1", "drop", none()),
        ],
        &[
            ("input.dogstatsd", "x_alpha.in"),
            ("x_alpha.out", "x.in"),
            ("x.out", "x_copies.in"),
            ("x_copies.0", "x_copies_0.in"),
            ("x_copies.1", "x_copies_1.in"),
        ],
    );
    assert_eq!(
        statements(&a),
        [
            "let x = dogstatsd | alpha | beta",
            "let x_copies = x | tee(ports: 2)",
            "x_copies.0 | drop",
            "x_copies.1 | drop",
        ]
    );
}

#[test]
fn a_node_with_several_output_ports_is_a_let_with_port_references() {
    // An unregistered operator's ports are those its edges leave.
    let a = artifact(
        &[
            ("m", "gamma", none()),
            ("m_left", "emit", emit("a")),
            ("m_right_alpha", "alpha", none()),
            ("m_right_alpha_out", "drop", none()),
        ],
        &[
            ("input.dogstatsd", "m.in"),
            ("m.left", "m_left.in"),
            ("m.right", "m_right_alpha.in"),
            ("m_right_alpha.out", "m_right_alpha_out.in"),
        ],
    );
    assert_eq!(
        statements(&a),
        [
            "let m = dogstatsd | gamma",
            "m.left | emit output a",
            "m.right | alpha | drop"
        ]
    );
}

#[test]
fn a_let_whose_node_has_the_name_of_a_scope_input_takes_a_name_of_its_own() {
    // `let checks = ...` would make the reference to the scope input `checks` refer to the `let`, so the `let` is
    // named `checks_let` (and `checks_let_2` when that is taken), and its last stage has the node's ID. The node
    // before it, whose ID is not derived, ends a `let` of its own.
    let a = artifact(
        &[
            ("checks", "gamma", none()),
            ("checks_let", "alpha", none()),
            ("checks_left", "drop", none()),
            ("checks_right", "drop", none()),
            ("input_checks", "drop", none()),
        ],
        &[
            ("input.dogstatsd", "checks_let.in"),
            ("checks_let.out", "checks.in"),
            ("checks.left", "checks_left.in"),
            ("checks.right", "checks_right.in"),
            ("input.checks", "input_checks.in"),
        ],
    );
    assert_eq!(
        statements(&a),
        [
            "checks | drop",
            "let checks_let = dogstatsd | alpha",
            "let checks_let_2 = checks_let | gamma as checks",
            "checks_let_2.left | drop",
            "checks_let_2.right | drop",
        ]
    );

    assert_lowers_back(&a);

    // A tuple that is the whole `let`.
    let a = artifact(
        &[
            ("otlp", "merge", json!({"inputs": 2})),
            ("m", "merge", json!({"inputs": 2})),
            ("m_out", "drop", none()),
        ],
        &[
            ("input.otlp", "otlp.0"),
            ("input.checks", "otlp.1"),
            ("otlp.out", "m.0"),
            ("input.liveness", "m.1"),
            ("m.out", "m_out.in"),
        ],
    );
    assert_eq!(
        statements(&a),
        [
            "let otlp_let = (otlp, checks) as otlp",
            "let m = (otlp_let, liveness)",
            "m | drop",
        ]
    );
    assert_lowers_back(&a);
}

/// Asserts that lowering what `artifact` resugars to gives it back, in canonical order.
#[track_caller]
fn assert_lowers_back(artifact: &PolicyArtifact) {
    let program = resugar(artifact, &PrintOptions::default());
    let (lowered, _) = crate::lower::lower("", program, None).expect("the tree lowers");
    assert_eq!(lowered, canonical(artifact.clone()));
}

#[test]
fn a_node_whose_ports_are_unknown_continues_only_from_out() {
    // A pipeline continues from such a node at `out`, as lowering assumes, so its only port, when another, is written
    // as a reference to the port. A `timestamp_split` with a zero window does not configure.
    let a = artifact(
        &[
            ("t", "timestamp_split", json!({"window": "0s"})),
            ("t_default", "drop", none()),
        ],
        &[("input.dogstatsd", "t.in"), ("t.default", "t_default.in")],
    );
    assert_eq!(
        statements(&a),
        [r#"let t = dogstatsd | timestamp_split("0s")"#, "t.default | drop"]
    );

    let a = artifact(
        &[("t", "gamma", none()), ("t_out", "drop", none())],
        &[("input.dogstatsd", "t.in"), ("t.out", "t_out.in")],
    );
    assert_eq!(statements(&a), ["let t = dogstatsd | gamma", "t | drop"]);
}

#[test]
fn a_node_without_outputs_that_is_not_a_sink_ends_a_let() {
    // An `emit` naming both an output and recipients is invalid, and no sink writes it. Its ports are unknown, since
    // its parameters do not configure it, so lowering reports the `let` as unused: the best the syntax can do.
    let a = artifact(
        &[("e", "emit", json!({"output": "o", "to": ["primary"]}))],
        &[("input.dogstatsd", "e.in")],
    );
    assert_eq!(
        statements(&a),
        [r#"let e = dogstatsd | emit(output: "o", to: ["primary"])"#]
    );
}

#[test]
fn positional_parameters_are_marked() {
    let a = artifact(
        &[
            ("ts", "timestamp_split", json!({"window": "10s"})),
            ("ts_default", "drop", none()),
            ("ts_passthrough", "drop", none()),
        ],
        &[
            ("input.dogstatsd", "ts.in"),
            ("ts.default", "ts_default.in"),
            ("ts.passthrough", "ts_passthrough.in"),
        ],
    );
    let program = resugar(&a, &PrintOptions::default());
    let Statement::Let { pipeline, .. } = &program.statements[0] else {
        panic!("a `let` first");
    };
    let Stage::Op(stage) = &pipeline.stages[0] else {
        panic!("an operator stage");
    };
    assert_eq!(stage.positional.as_ref().map(ParamName::as_str), Some("window"));
    assert_eq!(
        statements(&a),
        [
            r#"let ts = dogstatsd | timestamp_split("10s")"#,
            "ts.default | drop",
            "ts.passthrough | drop"
        ]
    );
}

#[test]
fn parameters_equal_to_their_default_are_left_out() {
    static SPECS: &[ParamSpec] = &[
        ParamSpec::required("namespace", ParamType::String).positional(),
        ParamSpec::optional("limit", ParamType::Integer { min: 0, max: 100 }).with_default("10"),
        ParamSpec::optional("names", ParamType::List(&ParamType::String)).with_default(r#"["a"]"#),
    ];
    let params = |value: Value| serde_json::from_value::<Params>(value).unwrap();

    let (written, positional) = written_params(
        Some(SPECS),
        &params(json!({"namespace": "n", "limit": 10, "names": ["b"]})),
    );
    assert_eq!(written, params(json!({"namespace": "n", "names": ["b"]})));
    assert_eq!(positional.as_ref().map(ParamName::as_str), Some("namespace"));

    // Defaults compare as JSON values, and a positional parameter that is not written is not positional.
    let (written, positional) = written_params(Some(SPECS), &params(json!({"limit": 11, "names": ["a"]})));
    assert_eq!(written, params(json!({"limit": 11})));
    assert_eq!(positional, None);

    // An unregistered operator writes every parameter.
    let (written, positional) = written_params(None, &params(json!({"limit": 10})));
    assert_eq!(written, params(json!({"limit": 10})));
    assert_eq!(positional, None);
}

#[test]
fn headers_follow_the_options() {
    let mut a = artifact(&[("input_otlp", "drop", none())], &[("input.otlp", "input_otlp.in")]);
    a.base
        .outputs
        .insert(OutputId::new("o").unwrap(), BASE.parse().unwrap());
    a.ir_version = IR_VERSION + 1;
    a.blobs.insert(json!(["x"])).unwrap();

    let program = resugar(&a, &PrintOptions::default());
    assert_eq!(program.scope.as_str(), "metrics");
    assert_eq!(program.ir_version, Some(IR_VERSION + 1));
    let base = program.base.expect("the base is written");
    assert_eq!(base.program_hash.to_string(), BASE);
    assert_eq!(base.outputs.len(), 1);
    assert_eq!(program.blobs.expect("the blobs are written").len(), 1);

    let options = PrintOptions {
        elide_fingerprints: true,
        ..PrintOptions::default()
    };
    assert!(resugar(&a, &options).base.unwrap().outputs.is_empty());
    let options = PrintOptions {
        elide_base: true,
        ..PrintOptions::default()
    };
    assert_eq!(resugar(&a, &options).base, None);

    a.ir_version = IR_VERSION;
    a.blobs = Default::default();
    let program = resugar(&a, &PrintOptions::default());
    assert_eq!((program.ir_version, program.blobs), (None, None));
}

#[test]
fn the_tree_has_no_spans() {
    let program = resugar(&reused(0), &PrintOptions::default());
    assert_eq!((program.head, program.close), (Span::default(), Span::default()));
    assert!(program.statements.iter().all(|statement| match statement {
        Statement::Let { span, pipeline, .. } | Statement::Run { span, pipeline } =>
            *span == Span::default() && pipeline.span == Span::default(),
    }));
}

#[test]
fn artifacts_that_are_not_well_formed_still_resugar() {
    let cases = [
        // An edge from a node that does not exist, and to one.
        artifact(
            &[("a", "alpha", none())],
            &[("ghost.out", "a.in"), ("a.out", "nowhere.in")],
        ),
        // A cycle.
        artifact(
            &[("a", "merge", json!({"inputs": 2})), ("b", "alpha", none())],
            &[("input.dogstatsd", "a.0"), ("a.out", "b.in"), ("b.out", "a.1")],
        ),
        // A node no edge enters, and nodes without edges.
        artifact(&[("a", "alpha", none()), ("b", "tee", json!({"ports": 2}))], &[]),
        // A port used twice, and a node entered twice.
        artifact(
            &[("a", "alpha", none()), ("b", "drop", none()), ("c", "drop", none())],
            &[
                ("input.dogstatsd", "a.in"),
                ("a.out", "b.in"),
                ("a.out", "c.in"),
                ("input.otlp", "c.in"),
            ],
        ),
        // Duplicate IDs.
        artifact(
            &[("a", "alpha", none()), ("a", "beta", none())],
            &[("input.dogstatsd", "a.in"), ("a.out", "exit.done")],
        ),
        // Unused ports, a merge with a gap, and parameters that do not configure their operator.
        artifact(
            &[
                ("s", "split", none()),
                ("m", "merge", json!({"inputs": 3})),
                ("t", "tee", json!({"ports": "many"})),
            ],
            &[("input.dogstatsd", "s.in"), ("s.true", "m.2"), ("input.otlp", "t.in")],
        ),
    ];
    for case in &cases {
        let program = resugar(case, &PrintOptions::default());
        assert!(!program.statements.is_empty(), "{:?}", case);
    }
}

#[test]
fn a_long_chain_resugars_without_deep_recursion() {
    // Within the artifact size limit.
    let count = 3_000;
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let ids = (0..count).map(|i| format!("n{}", i)).collect::<Vec<_>>();
    for (i, id) in ids.iter().enumerate() {
        nodes.push((id.as_str(), "alpha", none()));
        let from = if i == 0 {
            "input.dogstatsd".to_string()
        } else {
            format!("{}.out", ids[i - 1])
        };
        edges.push((from, format!("{}.in", id)));
    }
    nodes.push(("last", "drop", none()));
    edges.push((format!("{}.out", ids[count - 1]), "last.in".to_string()));
    let edges = edges
        .iter()
        .map(|(from, to)| (from.as_str(), to.as_str()))
        .collect::<Vec<_>>();
    let a = artifact(&nodes, &edges);
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        // Each node, its ID not derived, ends a `let` named for it, which the next statement continues.
        .spawn(move || assert_eq!(statements(&a).len(), count + 1))
        .unwrap()
        .join()
        .unwrap();
}

/// The default program's export, in normal form (its splits drop on `false`), with the IDs native lowering gives it.
fn default_export() -> PolicyArtifact {
    crate::tests::default_export()
}

/// Returns the statements of `artifact` with parameters and predicates written as `...`.
fn outline(artifact: &PolicyArtifact) -> Vec<String> {
    BRIEF.with(|brief| brief.set(true));
    let outline = statements(artifact);
    BRIEF.with(|brief| brief.set(false));
    outline
}

#[test]
fn the_default_program_resugars_into_lets_named_for_its_streams() {
    // In the native polarity, the splits drop on `true`, so they are written in the core form, and the tee after
    // `dsd_post_agg_filter.false` does not have that port's fanout ID, so it is explicit. Every node whose ID its
    // position does not derive ends a `let` named for it; the tuple and the stages after it are a `let` named
    // `metrics`, which derives the merge's ID, `metrics_in`, and the enrichment's, `metrics_host_enrichment`.
    assert_eq!(
        outline(&with_drops_on(
            &default_export(),
            "true",
            Some(&["dsd_prefix_filter_blocklist", "dsd_post_agg_filter"])
        )),
        [
            "let dsd_enrich = dogstatsd | dogstatsd_mapper(...)",
            "let dsd_prefix_filter_blocklist = dsd_enrich | namespace_prefix(...) | split(...)",
            "dsd_prefix_filter_blocklist.true | drop",
            "let dsd_no_agg_split = dsd_prefix_filter_blocklist.false | timestamp_split(...)",
            "let dsd_tag_filterlist = dsd_no_agg_split.default | tag_projection(...)",
            "let dsd_agg = dsd_tag_filterlist | aggregate(...)",
            "let dsd_post_agg_filter = (dsd_agg, dsd_no_agg_split.passthrough) | split(...)",
            "dsd_post_agg_filter.true | drop",
            "let dsd_post_agg_filter_fanout = dsd_post_agg_filter.false | tee(...)",
            "dsd_post_agg_filter_fanout.0 | emit output dsd_client_telemetry",
            "let metrics = (dsd_post_agg_filter_fanout.1, otlp, checks, liveness, trace_sampler) | host_enrichment \
             | host_tags(...)",
            "metrics | emit output dd_metrics_encode",
            "let metrics_routing_filter_0 = metrics | where ...",
            "metrics_routing_filter_0 | emit output metrics_routing_encode_0",
            "let metrics_routing_filter_1 = metrics | where ...",
            "metrics_routing_filter_1 | emit output metrics_routing_encode_1",
            "let mrf_metrics_gateway = metrics | where ...",
            "mrf_metrics_gateway | emit output mrf_metrics_encode",
            "let af_metrics_gateway = metrics | where ...",
            "af_metrics_gateway | emit output af_metrics_encode",
        ]
    );
}

/// Returns `artifact` with every split that drops on its other port and not on `port` swapped, as normalization does
/// for `false`, or only those of `only`: the predicate negated, the ports exchanged, and the drop renamed to
/// `<split>_<port>`, in canonical order.
fn with_drops_on(artifact: &PolicyArtifact, port: &str, only: Option<&[&str]>) -> PolicyArtifact {
    let other = if port == "true" { "false" } else { "true" };
    let mut value = serde_json::to_value(artifact).unwrap();
    let nodes = value["nodes"].as_array().unwrap().clone();
    let edges = value["edges"].as_array().unwrap().clone();
    let op_after = |from: String| {
        let to = edges.iter().find(|edge| edge["from"] == from.as_str())?["to"].as_str()?;
        let target = to.split('.').next()?;
        nodes.iter().find(|node| node["id"] == target)?["op"]
            .as_str()
            .map(str::to_string)
    };
    let swapped = nodes
        .iter()
        .filter(|node| node["op"] == "split")
        .map(|node| node["id"].as_str().unwrap().to_string())
        .filter(|id| only.is_none_or(|only| only.contains(&id.as_str())))
        .filter(|id| {
            op_after(format!("{}.{}", id, other)).as_deref() == Some("drop")
                && op_after(format!("{}.{}", id, port)).as_deref() != Some("drop")
        })
        .collect::<Vec<_>>();
    for id in &swapped {
        for node in value["nodes"].as_array_mut().unwrap() {
            if node["id"] == id.as_str() {
                let predicate = node["params"]["predicate"].clone();
                node["params"]["predicate"] = json!({"not": predicate});
            }
            if node["id"] == format!("{}_{}", id, other).as_str() {
                node["id"] = json!(format!("{}_{}", id, port));
            }
        }
        for edge in value["edges"].as_array_mut().unwrap() {
            let from = edge["from"].as_str().unwrap().to_string();
            let to = edge["to"].as_str().unwrap().to_string();
            if from == format!("{}.{}", id, other) {
                edge["from"] = json!(format!("{}.{}", id, port));
                edge["to"] = json!(to.replace(&format!("{}_{}.", id, other), &format!("{}_{}.", id, port)));
            } else if from == format!("{}.{}", id, port) {
                edge["from"] = json!(format!("{}.{}", id, other));
            }
        }
    }
    canonical(serde_json::from_value::<PolicyArtifact>(value).unwrap())
}

/// Returns `artifact` with its nodes and edges in canonical order.
fn canonical(artifact: PolicyArtifact) -> PolicyArtifact {
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

#[test]
fn the_normalized_default_program_has_the_designed_shape() {
    // With normalization's polarity, the splits are `where`s, and `dsd_post_agg_filter` and `metrics` are reused
    // through their fanouts. No node needs an `as`.
    assert_eq!(
        with_drops_on(&default_export(), "false", None),
        canonical(default_export())
    );
    assert_eq!(
        outline(&default_export()),
        [
            "let dsd_enrich = dogstatsd | dogstatsd_mapper(...)",
            "let dsd_prefix_filter_blocklist = dsd_enrich | namespace_prefix(...) | where ...",
            "let dsd_no_agg_split = dsd_prefix_filter_blocklist | timestamp_split(...)",
            "let dsd_tag_filterlist = dsd_no_agg_split.default | tag_projection(...)",
            "let dsd_agg = dsd_tag_filterlist | aggregate(...)",
            "let dsd_post_agg_filter = (dsd_agg, dsd_no_agg_split.passthrough) | where ...",
            "dsd_post_agg_filter | emit output dsd_client_telemetry",
            "let metrics = (dsd_post_agg_filter, otlp, checks, liveness, trace_sampler) | host_enrichment \
             | host_tags(...)",
            "metrics | emit output dd_metrics_encode",
            "let metrics_routing_filter_0 = metrics | where ...",
            "metrics_routing_filter_0 | emit output metrics_routing_encode_0",
            "let metrics_routing_filter_1 = metrics | where ...",
            "metrics_routing_filter_1 | emit output metrics_routing_encode_1",
            "let mrf_metrics_gateway = metrics | where ...",
            "mrf_metrics_gateway | emit output mrf_metrics_encode",
            "let af_metrics_gateway = metrics | where ...",
            "af_metrics_gateway | emit output af_metrics_encode",
        ]
    );
}

/// Generates small artifacts with arbitrary edges between a few nodes, well-formed or not.
fn arbitrary_artifact() -> impl Strategy<Value = PolicyArtifact> {
    const OPS: &[(&str, &str)] = &[
        ("alpha", "{}"),
        ("drop", "{}"),
        ("emit", r#"{"output": "o"}"#),
        ("split", r#"{"predicate": {"name": {"eq": "a"}}}"#),
        ("tee", r#"{"ports": 2}"#),
        ("tee", r#"{"ports": 3}"#),
        ("merge", r#"{"inputs": 2}"#),
        ("timestamp_split", r#"{"window": "10s"}"#),
    ];
    const IDS: &[&str] = &["a", "b", "c", "a_fanout", "a_split", "b_false", "input_x_fanout", "c_0"];
    const PORTS: &[&str] = &["out", "in", "true", "false", "0", "1", "2", "default", "passthrough"];
    let end = prop_oneof![
        (0..IDS.len(), 0..PORTS.len()).prop_map(|(n, p)| format!("{}.{}", IDS[n], PORTS[p])),
        prop_oneof![Just("input.x"), Just("input.y"), Just("exit.done")].prop_map(str::to_string),
    ];
    (
        proptest::collection::vec((0..IDS.len(), 0..OPS.len()), 0..7),
        proptest::collection::vec((end.clone(), end), 0..12),
    )
        .prop_map(|(nodes, edges)| {
            let nodes = nodes
                .into_iter()
                .map(|(id, op)| (IDS[id], OPS[op].0, serde_json::from_str::<Value>(OPS[op].1).unwrap()))
                .collect::<Vec<_>>();
            let edges = edges
                .iter()
                .map(|(from, to)| (from.as_str(), to.as_str()))
                .collect::<Vec<_>>();
            artifact(&nodes, &edges)
        })
}

proptest! {
    #[test]
    fn property_test_resugaring_is_total(artifact in arbitrary_artifact()) {
        let program = resugar(&artifact, &PrintOptions::default());
        prop_assert_eq!(program.scope, artifact.scope);
    }
}
