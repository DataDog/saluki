//! Tests for formatting, which build trees by hand and compare the exact text.

use saluki_metric_plan::ir::{ArtifactNodeId, OperatorName, OutputId, ParamName, Params, PortName};
use serde_json::{json, Value};

use super::format;
use crate::{
    ast::{Head, Name, OpStage, Pipeline, Program, Ref, Sink, SinkKind, Stage, Statement, Tuple, WhereStage},
    diagnostic::Span,
};

// Builders.

fn name(id: &str) -> Name {
    Name {
        id: ArtifactNodeId::new(id).unwrap(),
        span: Span::default(),
    }
}

fn as_id(id: Option<&str>) -> Option<Name> {
    id.map(name)
}

fn reference(id: &str) -> Ref {
    Ref {
        name: name(id),
        port: None,
        span: Span::default(),
    }
}

fn port_ref(id: &str, port: &str) -> Ref {
    Ref {
        name: name(id),
        port: Some((PortName::new(port).unwrap(), Span::default())),
        span: Span::default(),
    }
}

fn head(id: &str) -> Head {
    Head::Ref(reference(id))
}

fn tuple(items: Vec<Ref>, id: Option<&str>) -> Head {
    Head::Tuple(Tuple {
        items,
        id: as_id(id),
        span: Span::default(),
    })
}

/// An operator stage, with its parameters in the JSON of the IR, the positional one named by `positional`.
fn op(operator: &str, params: Value, positional: Option<&str>, id: Option<&str>) -> Stage {
    let params = params
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| (ParamName::new(key.as_str()).unwrap(), value.clone()))
        .collect::<Params>();
    Stage::Op(OpStage {
        op: OperatorName::new(operator).unwrap(),
        op_span: Span::default(),
        params,
        positional: positional.map(|name| ParamName::new(name).unwrap()),
        param_spans: Vec::new(),
        id: as_id(id),
        span: Span::default(),
    })
}

fn where_stage(predicate: Value, id: Option<&str>) -> Stage {
    Stage::Where(WhereStage {
        predicate,
        param_spans: Vec::new(),
        id: as_id(id),
        span: Span::default(),
    })
}

fn sink(kind: SinkKind, id: Option<&str>) -> Sink {
    Sink {
        kind,
        id: as_id(id),
        span: Span::default(),
    }
}

fn emit(names: &[&str], id: Option<&str>) -> Sink {
    sink(
        SinkKind::Recipients {
            names: names.iter().map(|name| name.to_string()).collect(),
            span: Span::default(),
        },
        id,
    )
}

fn emit_output(output: &str, id: Option<&str>) -> Sink {
    sink(
        SinkKind::Output {
            output: OutputId::new(output).unwrap(),
            span: Span::default(),
        },
        id,
    )
}

fn drop_sink(id: Option<&str>) -> Sink {
    sink(SinkKind::Drop, id)
}

fn pipeline(head: Head, stages: Vec<Stage>, end: Option<Sink>) -> Pipeline {
    Pipeline {
        head,
        stages,
        end,
        span: Span::default(),
    }
}

fn let_(id: &str, pipeline: Pipeline) -> Statement {
    Statement::Let {
        name: name(id),
        pipeline,
        span: Span::default(),
    }
}

fn run(pipeline: Pipeline) -> Statement {
    Statement::Run {
        pipeline,
        span: Span::default(),
    }
}

fn program(statements: Vec<Statement>) -> Program {
    Program {
        head: Span::default(),
        ir_version: None,
        blobs: None,
        statements,
        close: Span::default(),
    }
}

/// Formats one statement and returns its text, without the `policy` wrapper and its indentation.
fn statement_text(statement: Statement, width: usize) -> String {
    let text = format(&program(vec![statement]), width);
    let body = text
        .strip_prefix("policy {\n")
        .and_then(|text| text.strip_suffix("\n}\n"))
        .unwrap();
    body.to_string()
}

/// The text of one bare statement `x | where <predicate>` with a sink, which the predicate tests share.
fn predicate_text(predicate: Value) -> String {
    let statement = run(pipeline(
        head("x"),
        vec![where_stage(predicate, None)],
        Some(drop_sink(None)),
    ));
    let text = statement_text(statement, usize::MAX);
    text.strip_prefix("  x | where ")
        .and_then(|text| text.strip_suffix(" | drop"))
        .unwrap()
        .to_string()
}

// Program sections.

#[test]
fn an_empty_program_is_an_empty_block() {
    assert_eq!(format(&program(vec![]), 120), "policy {\n}\n");
}

#[test]
fn the_headers_and_blobs_come_before_the_statements() {
    let mut program = program(vec![run(pipeline(head("x"), vec![], Some(drop_sink(None))))]);
    program.ir_version = Some(2);
    let blob_hash = format!("sha256:{}", "4".repeat(64));
    program.blobs = Some(
        [(blob_hash.clone(), json!({"rules": ["a", "b"]}))]
            .into_iter()
            .collect(),
    );

    let expected = format!(
        r#"policy {{
  ir_version 2

  blobs {{
    "{blob}" = {{ rules: ["a", "b"] }},
  }}

  x | drop
}}
"#,
        blob = blob_hash,
    );
    assert_eq!(format(&program, 120), expected);
}

#[test]
fn statements_are_separated_by_blank_lines() {
    let statements = vec![
        let_("a", pipeline(head("dogstatsd"), vec![], None)),
        let_("b", pipeline(head("a"), vec![], None)),
        run(pipeline(head("b"), vec![], Some(drop_sink(None)))),
    ];
    assert_eq!(
        format(&program(statements), 120),
        "policy {\n  let a = dogstatsd\n\n  let b = a\n\n  b | drop\n}\n"
    );
}

// Statements.

#[test]
fn an_operator_named_like_another_stage_or_as_takes_parentheses() {
    let stages = ["where", "fork", "emit", "drop", "exit", "as", "alpha"]
        .iter()
        .map(|name| op(name, json!({}), None, None))
        .collect();
    let statement = let_("x", pipeline(head("dogstatsd"), stages, None));
    assert_eq!(
        statement_text(statement, usize::MAX),
        "  let x = dogstatsd\n    | where()\n    | fork()\n    | emit()\n    | drop()\n    | exit()\n    | as()\n    | alpha"
    );
}

#[test]
fn a_short_pipeline_is_one_line() {
    let statement = let_(
        "x",
        pipeline(
            head("dogstatsd"),
            vec![op("host_enrichment", json!({}), None, None)],
            None,
        ),
    );
    assert_eq!(statement_text(statement, 120), "  let x = dogstatsd | host_enrichment");

    let statement = run(pipeline(
        port_ref_head("x", "default"),
        vec![op("host_enrichment", json!({}), None, None)],
        Some(emit_output("out", None)),
    ));
    assert_eq!(
        statement_text(statement, 120),
        "  x.default | host_enrichment | emit output out"
    );
}

fn port_ref_head(id: &str, port: &str) -> Head {
    Head::Ref(port_ref(id, port))
}

#[test]
fn a_pipeline_with_three_stages_breaks_even_when_it_fits() {
    let statement = let_(
        "x",
        pipeline(
            head("a"),
            vec![
                op("host_enrichment", json!({}), None, None),
                op("host_enrichment", json!({}), None, None),
                op("host_enrichment", json!({}), None, None),
            ],
            None,
        ),
    );
    assert_eq!(
        statement_text(statement, 120),
        "  let x = a\n    | host_enrichment\n    | host_enrichment\n    | host_enrichment"
    );
}

#[test]
fn a_pipeline_that_does_not_fit_breaks() {
    let statement = run(pipeline(
        head("some_stream"),
        vec![op("host_enrichment", json!({}), None, None)],
        Some(drop_sink(None)),
    ));
    assert_eq!(
        statement_text(statement.clone(), 120),
        "  some_stream | host_enrichment | drop"
    );
    assert_eq!(
        statement_text(statement, 20),
        "  some_stream\n    | host_enrichment\n    | drop"
    );
}

#[test]
fn a_sink_counts_as_a_stage() {
    let statement = run(pipeline(
        head("a"),
        vec![
            op("host_enrichment", json!({}), None, None),
            op("host_enrichment", json!({}), None, None),
        ],
        Some(drop_sink(None)),
    ));
    assert_eq!(
        statement_text(statement, 120),
        "  a\n    | host_enrichment\n    | host_enrichment\n    | drop"
    );
}

// Stages.

#[test]
fn an_operator_without_parameters_has_no_parentheses() {
    let statement = let_(
        "x",
        pipeline(
            head("a"),
            vec![op("host_enrichment", json!({}), None, Some("enrich"))],
            None,
        ),
    );
    assert_eq!(
        statement_text(statement, 120),
        "  let x = a | host_enrichment as enrich"
    );
}

#[test]
fn the_positional_parameter_comes_first_and_the_rest_follow_the_schema() {
    // `deadline` is declared after `tags`, whatever the order of the map.
    let statement = let_(
        "x",
        pipeline(
            head("a"),
            vec![op(
                "host_tags",
                json!({"deadline": "5s", "tags": {"config": "host.tags"}}),
                Some("tags"),
                Some("tagged"),
            )],
            None,
        ),
    );
    assert_eq!(
        statement_text(statement, 120),
        "  let x = a | host_tags(config.host.tags, deadline: 5s) as tagged"
    );
}

#[test]
fn a_positional_parameter_the_tree_wrote_by_name_is_written_by_name() {
    let statement = let_(
        "x",
        pipeline(
            head("a"),
            vec![op("timestamp_split", json!({"window": "10s"}), None, None)],
            None,
        ),
    );
    assert_eq!(
        statement_text(statement, 120),
        "  let x = a | timestamp_split(window: 10s)"
    );

    let statement = let_(
        "x",
        pipeline(
            head("a"),
            vec![op("timestamp_split", json!({"window": "10s"}), Some("window"), None)],
            None,
        ),
    );
    assert_eq!(statement_text(statement, 120), "  let x = a | timestamp_split(10s)");
}

#[test]
fn undeclared_parameters_follow_the_declared_ones_sorted() {
    let statement = let_(
        "x",
        pipeline(
            head("a"),
            vec![op(
                "host_tags",
                json!({"zeta": 1, "alpha": true, "tags": ["a"]}),
                None,
                None,
            )],
            None,
        ),
    );
    assert_eq!(
        statement_text(statement, 120),
        "  let x = a | host_tags(tags: [\"a\"], alpha: true, zeta: 1)"
    );
}

#[test]
fn arguments_break_one_per_line_when_the_stage_does_not_fit() {
    let stage = op(
        "namespace_prefix",
        json!({"namespace": "tenant", "exclusions": ["internal.", "system.", "debug."]}),
        Some("namespace"),
        Some("prefixed"),
    );
    let statement = let_("x", pipeline(head("a"), vec![stage], None));
    assert_eq!(
        statement_text(statement.clone(), 120),
        "  let x = a | namespace_prefix(\"tenant\", exclusions: [\"internal.\", \"system.\", \"debug.\"]) as prefixed"
    );
    assert_eq!(
        statement_text(statement, 60),
        r#"  let x = a
    | namespace_prefix(
        "tenant",
        exclusions: ["internal.", "system.", "debug."],
      ) as prefixed"#
    );
}

#[test]
fn a_long_argument_breaks_its_list() {
    let stage = op(
        "namespace_prefix",
        json!({"namespace": "tenant", "exclusions": ["internal.", "system.", "debug."]}),
        Some("namespace"),
        None,
    );
    let statement = let_("x", pipeline(head("a"), vec![stage], None));
    assert_eq!(
        statement_text(statement, 40),
        r#"  let x = a
    | namespace_prefix(
        "tenant",
        exclusions: [
          "internal.",
          "system.",
          "debug.",
        ],
      )"#
    );
}

#[test]
fn a_split_takes_its_predicate_first() {
    let stage = op(
        "split",
        json!({"predicate": {"name": {"eq": "a"}}}),
        Some("predicate"),
        None,
    );
    let statement = let_("x", pipeline(head("a"), vec![stage], None));
    assert_eq!(statement_text(statement, 120), "  let x = a | split(name == \"a\")");
}

#[test]
fn a_where_stage_is_written_with_its_predicate_and_as() {
    let statement = let_(
        "x",
        pipeline(
            head("a"),
            vec![where_stage(json!({"name": {"prefix": "demo."}}), Some("filtered"))],
            None,
        ),
    );
    assert_eq!(
        statement_text(statement, 120),
        "  let x = a | where name starts_with \"demo.\" as filtered"
    );
}

#[test]
fn a_lone_positional_argument_takes_no_comma_when_broken() {
    let predicate = json!({"any": [
        {"intake_name": {"eq": "checkout.requests"}},
        {"intake_name": {"prefix": "checkout."}},
    ]});
    let statement = let_(
        "x",
        pipeline(
            head("a"),
            vec![op("split", json!({"predicate": predicate}), Some("predicate"), None)],
            None,
        ),
    );
    assert_eq!(
        statement_text(statement, 50),
        r#"  let x = a
    | split(
        intake_name == "checkout.requests"
        or intake_name starts_with "checkout."
      )"#
    );
}

#[test]
fn a_long_where_breaks_at_its_junction_under_the_stage() {
    let predicate = json!({"any": [
        {"intake_name": {"eq": "checkout.requests"}},
        {"intake_name": {"prefix": "checkout."}},
        {"host": {"eq": "web-1"}},
    ]});
    let statement = let_(
        "x",
        pipeline(head("a"), vec![where_stage(predicate, Some("routed"))], None),
    );
    assert_eq!(
        statement_text(statement, 60),
        r#"  let x = a
    | where intake_name == "checkout.requests"
        or intake_name starts_with "checkout."
        or host == "web-1"
        as routed"#
    );
}

#[test]
fn a_long_list_in_a_where_breaks_its_elements() {
    let predicate = json!({"name": {"in": ["first.name", "second.name", "third.name"]}});
    let statement = let_("x", pipeline(head("a"), vec![where_stage(predicate, None)], None));
    assert_eq!(
        statement_text(statement, 40),
        r#"  let x = a
    | where name in [
        "first.name",
        "second.name",
        "third.name",
      ]"#
    );
}

#[test]
fn a_long_set_match_breaks_its_arguments() {
    let predicate =
        json!({"name": {"matches": {"values": ["first.name", "second.name"], "prefix": {"config": "a.mode"}}}});
    let statement = let_("x", pipeline(head("a"), vec![where_stage(predicate, None)], None));
    assert_eq!(
        statement_text(statement, 50),
        r#"  let x = a
    | where name matches(
        values: ["first.name", "second.name"],
        prefix: config.a.mode,
      )"#
    );
}

// Tuples.

#[test]
fn a_tuple_is_a_head_with_an_optional_id() {
    let statement = let_(
        "x",
        pipeline(
            tuple(
                vec![reference("a"), port_ref("b", "port"), reference("c")],
                Some("joined"),
            ),
            vec![op("host_enrichment", json!({}), None, None)],
            None,
        ),
    );
    assert_eq!(
        statement_text(statement, 120),
        "  let x = (a, b.port, c) as joined | host_enrichment"
    );

    let statement = run(pipeline(
        tuple(vec![reference("a"), reference("b")], None),
        vec![],
        Some(drop_sink(None)),
    ));
    assert_eq!(statement_text(statement, 120), "  (a, b) | drop");
}

#[test]
fn a_tuple_that_does_not_fit_has_one_item_per_line() {
    let statement = let_(
        "x",
        pipeline(
            tuple(
                vec![reference("alpha"), port_ref("beta", "port"), reference("gamma")],
                Some("joined"),
            ),
            vec![op("host_enrichment", json!({}), None, None)],
            None,
        ),
    );
    assert_eq!(
        statement_text(statement, 30),
        "  let x = (\n    alpha,\n    beta.port,\n    gamma,\n  ) as joined\n    | host_enrichment"
    );
}

// Sinks.

#[test]
fn sinks_are_written_with_their_ids() {
    let cases = [
        (emit(&["primary"], None), "  x | emit primary"),
        (emit(&["primary"], Some("s")), "  x | emit primary as s"),
        (
            emit(&["primary", "https://b.example.com"], None),
            "  x | emit [primary, \"https://b.example.com\"]",
        ),
        (
            emit(&["https://b.example.com"], None),
            "  x | emit \"https://b.example.com\"",
        ),
        (emit(&["output"], None), "  x | emit \"output\""),
        (emit(&["a", "b"], Some("s")), "  x | emit [a, b] as s"),
        (emit_output("out", None), "  x | emit output out"),
        (emit_output("out", Some("s")), "  x | emit output out as s"),
        (drop_sink(None), "  x | drop"),
        (drop_sink(Some("s")), "  x | drop as s"),
    ];
    for (end, expected) in cases {
        let statement = run(pipeline(head("x"), vec![], Some(end)));
        assert_eq!(statement_text(statement, 120), expected);
    }
}

// Values.

#[test]
fn values_are_written_as_the_old_printer_wrote_them() {
    let params = json!({
        "tags": {"config": "host.tags"},
        "deadline": "90s",
        "zeta": {"b": [1, 2.5, null, true], "a": "x\ny", "c d": {}, "blob": "sha256:nothash"},
    });
    let statement = let_(
        "x",
        pipeline(head("a"), vec![op("host_tags", params, None, None)], None),
    );
    assert_eq!(
        statement_text(statement, usize::MAX),
        "  let x = a | host_tags(tags: config.host.tags, deadline: 90s, zeta: { a: \"x\\ny\", b: [1, 2.5, null, true], \
         blob: \"sha256:nothash\", \"c d\": {} })"
    );
}

#[test]
fn a_blob_reference_and_a_string_that_is_not_a_duration() {
    let blob = format!("sha256:{}", "a".repeat(64));
    let params = json!({"tags": {"blob": blob}, "deadline": "10 seconds"});
    let statement = let_(
        "x",
        pipeline(head("a"), vec![op("host_tags", params, None, None)], None),
    );
    assert_eq!(
        statement_text(statement, 200),
        format!(
            "  let x = a | host_tags(tags: blob(\"{}\"), deadline: \"10 seconds\")",
            blob
        )
    );
}

#[test]
fn the_maximum_width_writes_values_and_predicates_on_one_line() {
    let exclusions = (0..40)
        .map(|i| format!("excluded.namespace.number.{i}."))
        .collect::<Vec<_>>();
    let predicate = json!({"any": [
        {"name": {"in": exclusions}},
        {"name": {"prefix": "x"}},
    ]});
    let statement = let_(
        "x",
        pipeline(
            head("a"),
            vec![
                op(
                    "namespace_prefix",
                    json!({"namespace": "t", "exclusions": exclusions}),
                    Some("namespace"),
                    None,
                ),
                where_stage(predicate, None),
                op("host_enrichment", json!({}), None, None),
            ],
            None,
        ),
    );
    let text = statement_text(statement, usize::MAX);
    // One line per stage and no more.
    assert_eq!(text.lines().count(), 4);
}

// Predicates.

#[test]
fn string_matches_on_names_and_hosts() {
    let cases = [
        (json!({"name": {"eq": "a"}}), r#"name == "a""#),
        (json!({"intake_name": {"eq": "a"}}), r#"intake_name == "a""#),
        (json!({"host": {"eq": "a"}}), r#"host == "a""#),
        (json!({"name": {"prefix": "a."}}), r#"name starts_with "a.""#),
        (json!({"name": {"in": ["a", "b"]}}), r#"name in ["a", "b"]"#),
        (
            json!({"name": {"prefix_in": ["a.", "b."]}}),
            r#"name starts_with ["a.", "b."]"#,
        ),
        (
            json!({"name": {"matches": {"values": {"config": "x.values"}, "prefix": false}}}),
            "name in config.x.values",
        ),
        (
            json!({"name": {"matches": {"values": {"config": "x.values"}, "prefix": true}}}),
            "name starts_with config.x.values",
        ),
        (
            json!({"name": {"matches": {"values": {"config": "x.values"}, "prefix": {"config": "x.mode"}}}}),
            "name matches(values: config.x.values, prefix: config.x.mode)",
        ),
        (
            json!({"name": {"matches": {"values": ["a", "b"], "prefix": false}}}),
            r#"name matches(values: ["a", "b"], prefix: false)"#,
        ),
        (
            json!({"name": {"matches": {"values": ["a"], "prefix": {"config": "x.mode"}}}}),
            r#"name matches(values: ["a"], prefix: config.x.mode)"#,
        ),
        (
            json!({"name": {"matches": {"values": [], "prefix": true}}}),
            "name matches(values: [], prefix: true)",
        ),
    ];
    for (predicate, expected) in cases {
        assert_eq!(predicate_text(predicate), expected);
    }
}

#[test]
fn host_and_tag_leaves() {
    let cases = [
        (json!({"host_unset": {}}), "host is unset"),
        (json!({"not": {"host_unset": {}}}), "host is set"),
        (json!({"has_tag": "env"}), "tags.env is set"),
        (json!({"not": {"has_tag": "env"}}), "tags.env is unset"),
        (
            json!({"has_tag": "app.kubernetes.io/name"}),
            r#"tags["app.kubernetes.io/name"] is set"#,
        ),
        (json!({"not": {"has_tag": "Team"}}), r#"tags["Team"] is unset"#),
        (json!({"has_origin_tag": "pod"}), "origin_tags.pod is set"),
        (json!({"not": {"has_origin_tag": "pod"}}), "origin_tags.pod is unset"),
        (
            json!({"tag": {"key": "team", "value": {"eq": "platform"}}}),
            r#"tags.team == "platform""#,
        ),
        (
            json!({"not": {"tag": {"key": "team", "value": {"eq": "platform"}}}}),
            r#"tags.team != "platform""#,
        ),
        (
            json!({"tag": {"key": "team", "value": {"prefix": "p"}}}),
            r#"tags.team starts_with "p""#,
        ),
        (
            json!({"tag": {"key": "team", "value": {"in": ["a", "b"]}}}),
            r#"tags.team in ["a", "b"]"#,
        ),
        (
            json!({"tag": {"key": "Team", "value": {"prefix_in": ["a", "b"]}}}),
            r#"tags["Team"] starts_with ["a", "b"]"#,
        ),
        (
            json!({"origin_tag": {"key": "pod", "value": {"prefix": "web-"}}}),
            r#"origin_tags.pod starts_with "web-""#,
        ),
        (
            json!({"origin_tag": {"key": "pod", "value": {"eq": "web"}}}),
            r#"origin_tags.pod == "web""#,
        ),
        (
            json!({"not": {"origin_tag": {"key": "pod", "value": {"eq": "web"}}}}),
            r#"origin_tags.pod != "web""#,
        ),
    ];
    for (predicate, expected) in cases {
        assert_eq!(predicate_text(predicate), expected);
    }
}

#[test]
fn kind_constant_flag_and_empty_leaves() {
    let cases = [
        (json!({"kind_in": ["counter", "gauge"]}), "kind in [counter, gauge]"),
        (json!({"const": true}), "true"),
        (json!({"const": false}), "false"),
        (json!({"flag": {"config": "a.enabled"}}), "config.a.enabled"),
        (json!({"is_empty": {"config": "a.list"}}), "is_empty(config.a.list)"),
        (json!({"is_empty": []}), "is_empty([])"),
        (json!({"is_empty": ["a"]}), r#"is_empty(["a"])"#),
    ];
    for (predicate, expected) in cases {
        assert_eq!(predicate_text(predicate), expected);
    }
}

#[test]
fn not_negates_what_has_no_negated_spelling() {
    let cases = [
        (json!({"not": {"name": {"prefix": "a"}}}), r#"not name starts_with "a""#),
        (json!({"not": {"name": {"in": ["a"]}}}), r#"not name in ["a"]"#),
        (json!({"not": {"kind_in": ["counter"]}}), "not kind in [counter]"),
        (
            json!({"not": {"flag": {"config": "a.enabled"}}}),
            "not config.a.enabled",
        ),
        (json!({"not": {"const": true}}), "not true"),
        (json!({"not": {"not": {"name": {"eq": "a"}}}}), r#"not name != "a""#),
        (json!({"not": {"not": {"has_tag": "k"}}}), "not tags.k is unset"),
        (
            json!({"not": {"tag": {"key": "k", "value": {"prefix": "v"}}}}),
            r#"not tags.k starts_with "v""#,
        ),
    ];
    for (predicate, expected) in cases {
        assert_eq!(predicate_text(predicate), expected);
    }
}

#[test]
fn junctions_are_infix_with_parentheses_where_precedence_needs_them() {
    let a = json!({"name": {"eq": "a"}});
    let b = json!({"name": {"eq": "b"}});
    let c = json!({"name": {"eq": "c"}});
    let d = json!({"name": {"eq": "d"}});
    let all = |operands: &[&Value]| json!({"all": operands});
    let any = |operands: &[&Value]| json!({"any": operands});
    let not = |operand: &Value| json!({"not": operand});

    let cases = [
        (all(&[&a, &b, &c]), r#"name == "a" and name == "b" and name == "c""#),
        (any(&[&a, &b, &c]), r#"name == "a" or name == "b" or name == "c""#),
        // `and` binds tighter than `or`, so it needs none under an `or`, but has them for the reader.
        (
            any(&[&all(&[&a, &b]), &c]),
            r#"(name == "a" and name == "b") or name == "c""#,
        ),
        (
            any(&[&a, &all(&[&b, &c])]),
            r#"name == "a" or (name == "b" and name == "c")"#,
        ),
        // An `or` under an `and` does.
        (
            all(&[&any(&[&a, &b]), &c]),
            r#"(name == "a" or name == "b") and name == "c""#,
        ),
        // Nesting of one kind would flatten without them.
        (
            any(&[&any(&[&a, &b]), &c]),
            r#"(name == "a" or name == "b") or name == "c""#,
        ),
        (
            all(&[&a, &all(&[&b, &c])]),
            r#"name == "a" and (name == "b" and name == "c")"#,
        ),
        (
            any(&[&all(&[&a, &b]), &all(&[&c, &d])]),
            r#"(name == "a" and name == "b") or (name == "c" and name == "d")"#,
        ),
        // `not` binds tightest.
        (not(&all(&[&a, &b])), r#"not (name == "a" and name == "b")"#),
        (not(&any(&[&a, &b])), r#"not (name == "a" or name == "b")"#),
        (all(&[&not(&a), &not(&b)]), r#"name != "a" and name != "b""#),
        (all(&[&not(&c), &d]), r#"name != "c" and name == "d""#),
        (not(&not(&all(&[&a, &b]))), r#"not not (name == "a" and name == "b")"#),
    ];
    for (predicate, expected) in cases {
        assert_eq!(predicate_text(predicate), expected);
    }
}

#[test]
fn predicates_with_no_text_of_their_own_are_written_raw() {
    let cases = [
        // `all` and `any` are never written, so a junction of fewer than two operands is a raw value.
        (json!({"all": []}), "{ all: [] }"),
        (json!({"any": []}), "{ any: [] }"),
        (
            json!({"all": [{"name": {"eq": "a"}}]}),
            r#"{ all: [{ name: { eq: "a" } }] }"#,
        ),
        // Where a predicate is expected, `true` and `false` are constants, so a bare boolean is raw inside a junction.
        (
            json!({"all": [true, {"name": {"eq": "a"}}]}),
            r#"{ all: [true, { name: { eq: "a" } }] }"#,
        ),
        (json!({"not": true}), "{ not: true }"),
        // A binding is a value, not a predicate: written `config.x`, it would be the flag leaf, so it is a record, at
        // the top and as an operand.
        (json!({"config": "a.b"}), r#"{ config: "a.b" }"#),
        (
            json!({"any": [{"config": "a.b"}, {"name": {"eq": "a"}}]}),
            r#"{ config: "a.b" } or name == "a""#,
        ),
        // A literal flag is not the constant leaf.
        (json!({"flag": true}), "{ flag: true }"),
        // Leaves that carry resolved values are never written as leaves.
        (
            json!({"flag": {"config": "a.enabled", "resolved": true}}),
            r#"{ flag: { config: "a.enabled", resolved: true } }"#,
        ),
        // Kinds out of declaration order are not how the leaf writes itself.
        (
            json!({"kind_in": ["gauge", "counter"]}),
            r#"{ kind_in: ["gauge", "counter"] }"#,
        ),
    ];
    for (predicate, expected) in cases {
        assert_eq!(predicate_text(predicate), expected);
    }
}

#[test]
fn a_bare_boolean_as_a_whole_predicate_is_written_as_the_constant() {
    assert_eq!(predicate_text(json!(true)), "true");
    assert_eq!(predicate_text(json!(false)), "false");
}

#[test]
fn a_long_predicate_breaks_with_parenthesized_operands() {
    let predicate = json!({"all": [
        {"any": [{"name": {"eq": "first.name"}}, {"name": {"eq": "second.name"}}]},
        {"not": {"any": [{"host": {"eq": "web-1"}}, {"host": {"eq": "web-2"}}]}},
    ]});
    let statement = run(pipeline(
        head("x"),
        vec![where_stage(predicate, None)],
        Some(drop_sink(None)),
    ));
    assert_eq!(
        statement_text(statement.clone(), 120),
        "  x | where (name == \"first.name\" or name == \"second.name\") and not (host == \"web-1\" or host == \"web-2\") | drop"
    );
    assert_eq!(
        statement_text(statement, 70),
        r#"  x
    | where (name == "first.name" or name == "second.name")
        and not (host == "web-1" or host == "web-2")
    | drop"#
    );
}

// The default program.

/// The default program of the metrics scope, with the values of its export, as it prints: a `let` for each stream a
/// node names, and no `as`.
fn default_program() -> Program {
    let fixture = crate::tests::default_export_document();
    let node_params = |id: &str| -> Value {
        let nodes = fixture["nodes"].as_array().unwrap();
        nodes
            .iter()
            .find(|node| node["id"] == id)
            .unwrap_or_else(|| panic!("the export has no node `{id}`"))["params"]
            .clone()
    };
    let predicate = |id: &str| node_params(id)["predicate"].clone();
    // The export is in normal form, so its `split` nodes drop their `false` port, as `where` does.

    let mut statements = vec![
        let_(
            "dsd_enrich",
            pipeline(
                head("dogstatsd"),
                vec![op("dogstatsd_mapper", node_params("dsd_enrich"), None, None)],
                None,
            ),
        ),
        let_(
            "dsd_prefix_filter",
            pipeline(
                head("dsd_enrich"),
                vec![
                    op(
                        "namespace_prefix",
                        node_params("dsd_prefix_filter_namespace_prefix"),
                        Some("namespace"),
                        None,
                    ),
                    where_stage(predicate("dsd_prefix_filter"), None),
                ],
                None,
            ),
        ),
        let_(
            "dsd_no_agg_split",
            pipeline(
                head("dsd_prefix_filter"),
                vec![op(
                    "timestamp_split",
                    node_params("dsd_no_agg_split"),
                    Some("window"),
                    None,
                )],
                None,
            ),
        ),
        let_(
            "dsd_tag_filterlist",
            pipeline(
                port_ref_head("dsd_no_agg_split", "default"),
                vec![op("tag_projection", node_params("dsd_tag_filterlist"), None, None)],
                None,
            ),
        ),
        let_(
            "dsd_agg",
            pipeline(
                head("dsd_tag_filterlist"),
                vec![op("aggregate", node_params("dsd_agg"), None, None)],
                None,
            ),
        ),
        let_(
            "dsd_post_agg_filter",
            pipeline(
                tuple(
                    vec![reference("dsd_agg"), port_ref("dsd_no_agg_split", "passthrough")],
                    None,
                ),
                vec![where_stage(predicate("dsd_post_agg_filter"), None)],
                None,
            ),
        ),
        run(pipeline(
            head("dsd_post_agg_filter"),
            vec![],
            Some(emit_output("dsd_client_telemetry", None)),
        )),
        let_(
            "metrics",
            pipeline(
                tuple(
                    vec![
                        reference("dsd_post_agg_filter"),
                        reference("otlp"),
                        reference("checks"),
                        reference("liveness"),
                        reference("trace_sampler"),
                    ],
                    None,
                ),
                vec![
                    op("host_enrichment", json!({}), None, None),
                    op("host_tags", node_params("metrics"), Some("tags"), None),
                ],
                None,
            ),
        ),
        run(pipeline(
            head("metrics"),
            vec![],
            Some(emit_output("dd_metrics_encode", None)),
        )),
    ];
    for (filter, output) in [
        ("metrics_routing_filter_0", "metrics_routing_encode_0"),
        ("metrics_routing_filter_1", "metrics_routing_encode_1"),
        ("mrf_metrics_gateway", "mrf_metrics_encode"),
        ("af_metrics_gateway", "af_metrics_encode"),
    ] {
        statements.push(let_(
            filter,
            pipeline(head("metrics"), vec![where_stage(predicate(filter), None)], None),
        ));
        statements.push(run(pipeline(head(filter), vec![], Some(emit_output(output, None)))));
    }
    program(statements)
}

#[test]
fn the_default_program_reads_as_a_person_would_write_it() {
    let program = default_program();
    let expected = r#"policy {
  let dsd_enrich = dogstatsd
    | dogstatsd_mapper(
        profiles: [
          {
            name: "checkout",
            prefix: "raw.",
            mappings: [
              {
                match: "raw.requests.*",
                match_type: "wildcard",
                name: "mapped.requests",
                tags: [{ name: "route", value: "$1" }],
              },
            ],
          },
        ],
        cache_size: 1000,
        string_interner_bytes: 65536,
      )

  let dsd_prefix_filter = dsd_enrich
    | namespace_prefix("tenant", exclusions: ["internal.", "system."])
    | where intake_name != "tenant.raw.blocked"

  let dsd_no_agg_split = dsd_prefix_filter | timestamp_split(10s)

  let dsd_tag_filterlist = dsd_no_agg_split.default
    | tag_projection(
        rules: [{ metric_name: "tenant.checkout.latency", action: "exclude", tags: ["user_id"] }],
        value_rules: [
          {
            metric_prefix: "tenant.checkout.",
            tag_name: "env",
            values: ["prod", "staging"],
            on_miss: "replace",
            replacement: "other",
          },
        ],
        cache_capacity: 100000,
      )

  let dsd_agg = dsd_tag_filterlist
    | aggregate(
        window: 10s,
        flush_interval: 15s,
        context_limit: 1000000,
        counter_expiry: 5m,
        flush_open_windows: false,
        histogram_aggregates: ["max", "median", "avg", "count"],
        histogram_percentiles: [95],
        copy_to_distribution: false,
        copy_to_distribution_prefix: "",
      )

  let dsd_post_agg_filter = (dsd_agg, dsd_no_agg_split.passthrough)
    | where not (kind in [counter, rate, gauge, set] and intake_name == "tenant.checkout.latency.max")

  dsd_post_agg_filter | emit output dsd_client_telemetry

  let metrics = (dsd_post_agg_filter, otlp, checks, liveness, trace_sampler)
    | host_enrichment
    | host_tags(config.host.tags, deadline: config.host.tags_deadline)

  metrics | emit output dd_metrics_encode

  let metrics_routing_filter_0 = metrics
    | where intake_name == "checkout.requests" or intake_name starts_with "checkout."

  metrics_routing_filter_0 | emit output metrics_routing_encode_0

  let metrics_routing_filter_1 = metrics | where intake_name starts_with "catalog."

  metrics_routing_filter_1 | emit output metrics_routing_encode_1

  let mrf_metrics_gateway = metrics
    | where config.multi_region_failover.enabled and config.multi_region_failover.metric_mirroring.enabled

  mrf_metrics_gateway | emit output mrf_metrics_encode

  let af_metrics_gateway = metrics
    | where kind in [counter, rate, gauge, set] and config.autoscaling_failover.enabled and name == "requests.count"

  af_metrics_gateway | emit output af_metrics_encode
}
"#;
    let printed = format(&program, 120);
    assert_eq!(printed, expected);
    assert!(printed.lines().all(|line| line.chars().count() <= 120), "{}", printed);
}
