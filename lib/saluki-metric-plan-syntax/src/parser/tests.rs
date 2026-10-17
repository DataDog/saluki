//! Tests of the parser: every statement form, stage, sink, value, and predicate, and the error of every syntax rule.

use proptest::prelude::*;
use saluki_metric_plan::predicate::Predicate;
use serde_json::{json, Value};

use super::*;
use crate::lexer::lex;

const ZERO_HASH: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
const ONE_HASH: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

fn parse(source: &str) -> Result<Program, Vec<Error>> {
    let tokens = lex(source).map_err(|error| vec![error])?;
    parse_program(source, &tokens)
}

#[track_caller]
fn parse_ok(source: &str) -> Program {
    match parse(source) {
        Ok(program) => program,
        Err(errors) => panic!(
            "{:?} does not parse: {:?}",
            source,
            errors.iter().map(|error| &error.message).collect::<Vec<_>>()
        ),
    }
}

/// Returns a policy with `body` as its statements.
fn policy(body: &str) -> String {
    format!("policy metrics {{\n{}\n}}\n", body)
}

/// Returns the statements of a policy with `body`.
#[track_caller]
fn statements(body: &str) -> Vec<Statement> {
    parse_ok(&policy(body)).statements
}

/// Returns the text of `source` at `span`.
fn text(source: &str, span: Span) -> &str {
    &source[span.range()]
}

/// Parses a policy with `body`, which must fail, and returns the error's text in the source and its message, with the
/// messages of any notes before it.
#[track_caller]
fn error_in(body: &str) -> (String, String, Vec<String>) {
    let source = policy(body);
    let mut errors = match parse(&source) {
        Ok(_) => panic!("{:?} parses", body),
        Err(errors) => errors,
    };
    let error = errors.pop().unwrap();
    (
        text(&source, error.span).to_string(),
        error.message,
        errors.into_iter().map(|note| note.message).collect(),
    )
}

/// Asserts that a policy with `body` fails at `offending` with a message that contains `message`.
#[track_caller]
fn assert_error(body: &str, offending: &str, message: &str) {
    let (found, found_message, _) = error_in(body);
    assert_eq!(found, offending, "{:?}: {}", body, found_message);
    assert!(
        found_message.contains(message),
        "{:?}: expected the message to contain {:?}, found {:?}",
        body,
        message,
        found_message
    );
}

/// Returns the pipeline of a statement.
fn pipeline_of(statement: &Statement) -> &Pipeline {
    match statement {
        Statement::Let { pipeline, .. } | Statement::Run { pipeline, .. } => pipeline,
    }
}

fn op(stage: &Stage) -> &OpStage {
    match stage {
        Stage::Op(stage) => stage,
        Stage::Where(_) => panic!("expected an operator stage, found `where`"),
    }
}

fn where_(stage: &Stage) -> &WhereStage {
    match stage {
        Stage::Where(stage) => stage,
        Stage::Op(stage) => panic!("expected `where`, found `{}`", stage.op),
    }
}

fn reference(head: &Head) -> &Ref {
    match head {
        Head::Ref(reference) => reference,
        Head::Tuple(_) => panic!("expected a reference, found a tuple"),
    }
}

fn tuple(head: &Head) -> &Tuple {
    match head {
        Head::Tuple(tuple) => tuple,
        Head::Ref(_) => panic!("expected a tuple, found a reference"),
    }
}

/// Returns `name` or `name.port` for a reference.
fn ref_text(reference: &Ref) -> String {
    match &reference.port {
        Some((port, _)) => format!("{}.{}", reference.name.id, port),
        None => reference.name.id.to_string(),
    }
}

fn id_text(id: &Option<Name>) -> Option<String> {
    id.as_ref().map(|name| name.id.to_string())
}

/// Parses `dogstatsd | where <predicate> | drop`, and returns the predicate.
#[track_caller]
fn predicate_json(predicate: &str) -> Value {
    let statements = statements(&format!("  dogstatsd | where {} | drop", predicate));
    where_(&pipeline_of(&statements[0]).stages[0]).predicate.clone()
}

/// Asserts that a policy with predicate `predicate` fails at `offending` with a message that contains `message`.
#[track_caller]
fn assert_predicate_error(predicate: &str, offending: &str, message: &str) {
    assert_error(&format!("  dogstatsd | where {} | drop", predicate), offending, message);
}

// The example program of the design.

const EXAMPLE: &str = r#"policy metrics {
  let dsd_no_agg_split = dogstatsd
    | dogstatsd_mapper(profiles: [], cache_size: 1000) as dsd_enrich
    | namespace_prefix("tenant", exclusions: ["internal.", "system."]) as dsd_prefix_filter
    | where intake_name != "tenant.raw.blocked" as dsd_prefix_filter_blocklist
    | timestamp_split(10s)

  let dsd_agg = dsd_no_agg_split.default
    | tag_projection(rules: [{ metric_name: "checkout.latency", action: "include", tags: ["env"] }], value_rules: [])
      as dsd_tag_filterlist
    | aggregate(window: 10s, flush_interval: 15s)

  let dsd_post_agg_filter = (dsd_agg, dsd_no_agg_split.passthrough)
    | where not (kind in [counter, rate, gauge, set] and intake_name == "tenant.checkout.latency.max")

  dsd_post_agg_filter | emit output dsd_client_telemetry

  let metrics = (dsd_post_agg_filter, otlp, checks, liveness, trace_sampler) as metrics_enrich_in
    | host_tags(config.host.tags, deadline: config.host.tags_deadline) as metrics_enrich_host_tags

  metrics | emit output dd_metrics_encode

  metrics
    | where intake_name == "checkout.requests" or intake_name starts_with "checkout." as metrics_routing_filter_0
    | emit output metrics_routing_encode_0

  metrics | where tags.env == "prod" | emit [primary, "https://b.example.com"]
}
"#;

#[test]
fn the_example_program_parses() {
    let program = parse_ok(EXAMPLE);
    assert_eq!(program.scope.as_str(), "metrics");
    assert_eq!(text(EXAMPLE, program.head), "policy metrics");
    assert_eq!(text(EXAMPLE, program.close), "}");
    assert_eq!(program.close.end, EXAMPLE.len() - 1);
    assert_eq!(program.statements.len(), 8);

    // let dsd_no_agg_split = dogstatsd | ... | timestamp_split(10s)
    let Statement::Let { name, pipeline, span } = &program.statements[0] else {
        panic!("expected a `let`");
    };
    assert_eq!(name.id.as_str(), "dsd_no_agg_split");
    assert_eq!(text(EXAMPLE, name.span), "dsd_no_agg_split");
    assert!(text(EXAMPLE, *span).starts_with("let dsd_no_agg_split = dogstatsd"));
    assert!(text(EXAMPLE, *span).ends_with("timestamp_split(10s)"));
    assert_eq!(pipeline.span.start, text_start(EXAMPLE, "dogstatsd\n"));
    assert_eq!(span.end, pipeline.span.end);
    assert_eq!(ref_text(reference(&pipeline.head)), "dogstatsd");
    assert_eq!(pipeline.end, None);
    let stages = &pipeline.stages;
    assert_eq!(stages.len(), 4);
    assert_eq!(op(&stages[0]).op.as_str(), "dogstatsd_mapper");
    assert_eq!(id_text(&op(&stages[0]).id).as_deref(), Some("dsd_enrich"));
    assert_eq!(
        text(EXAMPLE, op(&stages[0]).span),
        "dogstatsd_mapper(profiles: [], cache_size: 1000) as dsd_enrich"
    );
    let prefix = op(&stages[1]);
    assert_eq!(prefix.positional.as_ref().map(|name| name.as_str()), Some("namespace"));
    assert_eq!(prefix.params.get("namespace"), Some(&json!("tenant")));
    assert_eq!(prefix.params.get("exclusions"), Some(&json!(["internal.", "system."])));
    let blocklist = where_(&stages[2]);
    assert_eq!(
        blocklist.predicate,
        json!({"not": {"intake_name": {"eq": "tenant.raw.blocked"}}})
    );
    assert_eq!(id_text(&blocklist.id).as_deref(), Some("dsd_prefix_filter_blocklist"));
    let split = op(&stages[3]);
    assert_eq!(split.op.as_str(), "timestamp_split");
    assert_eq!(split.params.get("window"), Some(&json!("10s")));
    assert_eq!(split.id, None);

    // let dsd_agg = dsd_no_agg_split.default | tag_projection(...) as dsd_tag_filterlist | aggregate(...)
    let Statement::Let { name, pipeline, .. } = &program.statements[1] else {
        panic!("expected a `let`");
    };
    assert_eq!(name.id.as_str(), "dsd_agg");
    let head = reference(&pipeline.head);
    assert_eq!(ref_text(head), "dsd_no_agg_split.default");
    assert_eq!(text(EXAMPLE, head.span), "dsd_no_agg_split.default");
    assert_eq!(text(EXAMPLE, head.port.as_ref().unwrap().1), "default");
    let projection = op(&pipeline.stages[0]);
    assert_eq!(
        projection.params.get("rules"),
        Some(&json!([{"metric_name": "checkout.latency", "action": "include", "tags": ["env"]}]))
    );
    assert_eq!(id_text(&projection.id).as_deref(), Some("dsd_tag_filterlist"));
    let aggregate = op(&pipeline.stages[1]);
    assert_eq!(aggregate.params.get("window"), Some(&json!("10s")));
    assert_eq!(aggregate.params.get("flush_interval"), Some(&json!("15s")));
    assert_eq!(aggregate.positional, None);

    // let dsd_post_agg_filter = (dsd_agg, dsd_no_agg_split.passthrough) | where not (...)
    let Statement::Let { pipeline, .. } = &program.statements[2] else {
        panic!("expected a `let`");
    };
    let head = tuple(&pipeline.head);
    assert_eq!(
        head.items.iter().map(ref_text).collect::<Vec<_>>(),
        ["dsd_agg", "dsd_no_agg_split.passthrough"]
    );
    assert_eq!(head.id, None);
    assert_eq!(text(EXAMPLE, head.span), "(dsd_agg, dsd_no_agg_split.passthrough)");
    assert_eq!(
        where_(&pipeline.stages[0]).predicate,
        json!({"not": {"all": [
            {"kind_in": ["counter", "rate", "gauge", "set"]},
            {"intake_name": {"eq": "tenant.checkout.latency.max"}},
        ]}})
    );

    // dsd_post_agg_filter | emit output dsd_client_telemetry
    let Statement::Run { pipeline, span } = &program.statements[3] else {
        panic!("expected a bare statement");
    };
    assert_eq!(
        text(EXAMPLE, *span),
        "dsd_post_agg_filter | emit output dsd_client_telemetry"
    );
    let emit = pipeline.end.as_ref().unwrap();
    let SinkKind::Output { output, span } = &emit.kind else {
        panic!("expected `emit output`");
    };
    assert_eq!(output.as_str(), "dsd_client_telemetry");
    assert_eq!(text(EXAMPLE, *span), "dsd_client_telemetry");
    assert_eq!(text(EXAMPLE, emit.span), "emit output dsd_client_telemetry");

    // let metrics = (...) as metrics_enrich_in | host_tags(...) as metrics_enrich_host_tags
    let Statement::Let { name, pipeline, .. } = &program.statements[4] else {
        panic!("expected a `let`");
    };
    assert_eq!(name.id.as_str(), "metrics");
    let head = tuple(&pipeline.head);
    assert_eq!(head.items.len(), 5);
    assert_eq!(id_text(&head.id).as_deref(), Some("metrics_enrich_in"));
    assert_eq!(text(EXAMPLE, head.id.as_ref().unwrap().span), "metrics_enrich_in");
    let host_tags = op(&pipeline.stages[0]);
    assert_eq!(host_tags.positional.as_ref().map(|name| name.as_str()), Some("tags"));
    assert_eq!(host_tags.params.get("tags"), Some(&json!({"config": "host.tags"})));
    assert_eq!(
        host_tags.params.get("deadline"),
        Some(&json!({"config": "host.tags_deadline"}))
    );
    assert_eq!(id_text(&host_tags.id).as_deref(), Some("metrics_enrich_host_tags"));
    assert_eq!(pipeline.end, None);

    // Each use of `metrics` is a statement of its own.
    let Statement::Run { pipeline, span } = &program.statements[5] else {
        panic!("expected a bare statement");
    };
    assert_eq!(ref_text(reference(&pipeline.head)), "metrics");
    assert!(pipeline.stages.is_empty());
    assert_eq!(text(EXAMPLE, *span), "metrics | emit output dd_metrics_encode");
    let Statement::Run { pipeline, span } = &program.statements[6] else {
        panic!("expected a bare statement");
    };
    assert_eq!(
        where_(&pipeline.stages[0]).predicate,
        json!({"any": [
            {"intake_name": {"eq": "checkout.requests"}},
            {"intake_name": {"prefix": "checkout."}},
        ]})
    );
    assert_eq!(
        id_text(&where_(&pipeline.stages[0]).id).as_deref(),
        Some("metrics_routing_filter_0")
    );
    assert!(text(EXAMPLE, *span).ends_with("| emit output metrics_routing_encode_0"));
    let Statement::Run { pipeline, .. } = &program.statements[7] else {
        panic!("expected a bare statement");
    };
    assert_eq!(
        where_(&pipeline.stages[0]).predicate,
        json!({"tag": {"key": "env", "value": {"eq": "prod"}}})
    );
    let SinkKind::Recipients { names, span } = &pipeline.end.as_ref().unwrap().kind else {
        panic!("expected recipients");
    };
    assert_eq!(names, &["primary", "https://b.example.com"]);
    assert_eq!(text(EXAMPLE, *span), "[primary, \"https://b.example.com\"]");
}

/// Returns the offset of the first occurrence of `needle` in `source`.
fn text_start(source: &str, needle: &str) -> usize {
    source.find(needle).unwrap()
}

// Headers.

#[test]
fn headers_parse() {
    let source = format!(
        "policy metrics {{\n  ir_version 1\n  base \"{}\" outputs {{ a = \"{}\", b = \"{}\" }}\n  blobs {{ \"{}\" = \
         [1, 2] }}\n  dogstatsd | drop\n}}",
        ZERO_HASH, ONE_HASH, ZERO_HASH, ONE_HASH
    );
    let program = parse_ok(&source);
    assert_eq!(program.ir_version, Some(1));
    let base = program.base.unwrap();
    assert_eq!(base.program_hash.to_string(), ZERO_HASH);
    assert_eq!(
        base.outputs
            .iter()
            .map(|(output, hash)| (output.as_str().to_string(), hash.to_string()))
            .collect::<Vec<_>>(),
        [
            ("a".to_string(), ONE_HASH.to_string()),
            ("b".to_string(), ZERO_HASH.to_string())
        ]
    );
    assert_eq!(program.blobs.unwrap().get(ONE_HASH), Some(&json!([1, 2])));
    assert_eq!(program.statements.len(), 1);

    let program = parse_ok("policy metrics {}");
    assert_eq!((program.ir_version, program.base, program.blobs), (None, None, None));
    assert!(program.statements.is_empty());
}

#[test]
fn header_words_followed_by_a_pipe_or_port_name_streams() {
    let statements = statements("  base | drop\n  blobs.x | drop\n  ir_version | drop\n  let | drop");
    let heads = statements
        .iter()
        .map(|statement| ref_text(reference(&pipeline_of(statement).head)))
        .collect::<Vec<_>>();
    assert_eq!(heads, ["base", "blobs.x", "ir_version", "let"]);
}

#[test]
fn header_errors() {
    assert_error(
        &format!("  base \"{}\"\n  base \"{}\"", ZERO_HASH, ZERO_HASH),
        "base",
        "The policy gives `base` twice.",
    );
    assert_error(
        "  ir_version 1\n  ir_version 1",
        "ir_version",
        "gives `ir_version` twice",
    );
    assert_error(
        &format!("  dogstatsd | drop\n  base \"{}\"", ZERO_HASH),
        "base",
        "`base` comes before the policy's statements",
    );
    assert_error("  ir_version x", "x", "Expected an IR version");
    assert_error("  base \"nope\"", "\"nope\"", "");
}

// Statements and heads.

#[test]
fn every_statement_form_parses() {
    let body = "  let a = dogstatsd\n  let b = a | host_enrichment\n  b | drop\n  s.true | drop\n  (a, s.false) | \
                drop\n  (x,) as joined | drop";
    let statements = statements(body);
    assert_eq!(statements.len(), 6);

    let Statement::Let { name, pipeline, .. } = &statements[0] else {
        panic!("expected a `let`");
    };
    assert_eq!(name.id.as_str(), "a");
    assert!(pipeline.stages.is_empty());
    assert_eq!(pipeline.end, None);

    let Statement::Let { pipeline, .. } = &statements[1] else {
        panic!("expected a `let`");
    };
    let stage = op(&pipeline.stages[0]);
    assert_eq!(stage.op.as_str(), "host_enrichment");
    assert!(stage.params.is_empty());
    assert!(stage.param_spans.is_empty());

    assert!(matches!(&statements[2], Statement::Run { .. }));
    assert_eq!(ref_text(reference(&pipeline_of(&statements[3]).head)), "s.true");
    let head = tuple(&pipeline_of(&statements[4]).head);
    assert_eq!(head.items.iter().map(ref_text).collect::<Vec<_>>(), ["a", "s.false"]);
    let head = tuple(&pipeline_of(&statements[5]).head);
    assert_eq!(head.items.iter().map(ref_text).collect::<Vec<_>>(), ["x"]);
    assert_eq!(id_text(&head.id).as_deref(), Some("joined"));
}

#[test]
fn ports_may_be_indices() {
    let statements = statements("  t.0 | drop\n  t.12 | drop");
    let ports = statements
        .iter()
        .map(|statement| ref_text(reference(&pipeline_of(statement).head)))
        .collect::<Vec<_>>();
    assert_eq!(ports, ["t.0", "t.12"]);
}

#[test]
fn statements_need_no_separators() {
    // A tuple after a stage with no arguments starts the next statement.
    let statements = statements("  let a = dogstatsd | host_enrichment\n  (a, otlp) | drop\n  otlp | drop");
    assert_eq!(statements.len(), 3);
    assert!(matches!(pipeline_of(&statements[1]).head, Head::Tuple(_)));

    // A stream named `and` or `or` after a `where` starts the next statement, rather than continuing the predicate.
    let parsed = self::statements(
        "  let and = dogstatsd | timestamp_split(10s)\n  let or = and.default | where true\n  \
         and.passthrough | where false | drop\n  or | drop",
    );
    assert_eq!(parsed.len(), 4);
    assert_eq!(
        where_(&pipeline_of(&parsed[1]).stages[0]).predicate,
        json!({"const": true})
    );
    assert_eq!(reference(&pipeline_of(&parsed[2]).head).name.id.as_str(), "and");
    assert_eq!(reference(&pipeline_of(&parsed[3]).head).name.id.as_str(), "or");
}

// Stages and arguments.

#[test]
fn positional_parameters_are_recorded() {
    let body =
        "  let s = dogstatsd\n    | namespace_prefix(\"tenant\", exclusions: [])\n    | timestamp_split(10s)\n    | \
                host_tags(config.host.tags, deadline: 1s)\n    | split(name == \"a\")\n  s.true | drop\n  s.false \
                | drop";
    let source = policy(body);
    let program = parse_ok(&source);
    let stages = &pipeline_of(&program.statements[0]).stages;
    let positional = stages
        .iter()
        .map(|stage| {
            let stage = op(stage);
            let name = stage.positional.as_ref().unwrap().as_str();
            (stage.op.as_str(), name, stage.params.get(name).unwrap().clone())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        positional,
        [
            ("namespace_prefix", "namespace", json!("tenant")),
            ("timestamp_split", "window", json!("10s")),
            ("host_tags", "tags", json!({"config": "host.tags"})),
            ("split", "predicate", json!({"name": {"eq": "a"}})),
        ]
    );
    let split = op(&stages[3]);
    assert_eq!(
        split
            .param_spans
            .iter()
            .map(|(path, span)| (path.as_str(), text(&source, *span)))
            .collect::<Vec<_>>(),
        [("predicate", "name == \"a\"")]
    );

    // The positional parameter may also be named.
    let statements = statements("  dogstatsd | timestamp_split(window: 10s) | drop");
    let stage = op(&pipeline_of(&statements[0]).stages[0]);
    assert_eq!(stage.positional, None);
    assert_eq!(stage.params.get("window"), Some(&json!("10s")));
}

#[test]
fn param_spans_cover_every_nested_value() {
    let body = "  dogstatsd | tag_projection(rules: [{ metric_name: \"a\", tags: [\"x\"] }], cache_capacity: 5) | drop";
    let source = policy(body);
    let program = parse_ok(&source);
    let stage = op(&pipeline_of(&program.statements[0]).stages[0]);
    let spans = stage
        .param_spans
        .iter()
        .map(|(path, span)| (path.as_str(), text(&source, *span)))
        .collect::<Vec<_>>();
    assert_eq!(
        spans,
        [
            ("rules[0].metric_name", "\"a\""),
            ("rules[0].tags[0]", "\"x\""),
            ("rules[0].tags", "[\"x\"]"),
            ("rules[0]", "{ metric_name: \"a\", tags: [\"x\"] }"),
            ("rules", "[{ metric_name: \"a\", tags: [\"x\"] }]"),
            ("cache_capacity", "5"),
        ]
    );
    assert_eq!(text(&source, stage.op_span), "tag_projection");
}

#[test]
fn commas_between_named_parameters_may_be_left_out() {
    let statements = statements("  dogstatsd | aggregate(window: 10s flush_interval: 15s,) | drop");
    let stage = op(&pipeline_of(&statements[0]).stages[0]);
    assert_eq!(stage.params.get("flush_interval"), Some(&json!("15s")));
}

#[test]
fn argument_errors() {
    assert_error(
        "  dogstatsd | aggregate(10s) | drop",
        "10s",
        "Expected a parameter, written `<name>: <value>` (`aggregate` has no parameter that may be passed without its \
         name), or `)`; `aggregate` takes: window, flush_interval",
    );
    assert_error(
        "  dogstatsd | timestamp_split(10s, window: 5s) | drop",
        "window",
        "The parameter `window` is given twice.",
    );
    assert_error(
        "  dogstatsd | split(name == \"a\" name == \"b\") | drop",
        "name",
        "Expected `and`, `or`, `,`, or `)` after the predicate",
    );
    assert_error(
        "  dogstatsd | aggregate(window: 10s 5) | drop",
        "5",
        "Expected `,` or `)` after the parameter's value",
    );
    assert_error(
        "  dogstatsd | aggregate (window: 10s) | drop",
        "(",
        "Write the arguments of `aggregate` right after its name, without a space, as in `aggregate(...)`",
    );
}

#[test]
fn unknown_operators_get_a_note() {
    let (offending, message, notes) = error_in("  dogstatsd | agregate(10s) | drop");
    assert_eq!(offending, "10s");
    assert!(
        message.starts_with("Expected a parameter, written `<name>: <value>`, or `)`"),
        "{}",
        message
    );
    assert_eq!(notes.len(), 1);
    assert!(
        notes[0].contains("`agregate` is not a registered operator"),
        "{}",
        notes[0]
    );
    assert!(notes[0].contains("Did you mean `aggregate`?"), "{}", notes[0]);

    // An unknown operator with plain arguments parses.
    let statements = statements("  dogstatsd | frobnicate(level: 3) | drop");
    assert_eq!(
        op(&pipeline_of(&statements[0]).stages[0]).params.get("level"),
        Some(&json!(3))
    );
}

#[test]
fn as_is_accepted_on_every_form() {
    let body =
        "  (a, b) as m\n    | host_enrichment as h\n    | aggregate(window: 10s) as g\n    | where true as w\n    \
                | emit primary as e1\n  s | emit \"https://x.example.com\" as e2\n  s | emit [a, b] as e3\n  s | emit \
                output o as e4\n  s | drop as d\n  s | exit aggregated as x";
    let source = policy(body);
    let program = parse_ok(&source);
    let pipeline = pipeline_of(&program.statements[0]);
    let mut ids = vec![id_text(&tuple(&pipeline.head).id)];
    ids.extend(pipeline.stages.iter().map(|stage| match stage {
        Stage::Op(stage) => id_text(&stage.id),
        Stage::Where(stage) => id_text(&stage.id),
    }));
    let sinks = program
        .statements
        .iter()
        .map(|statement| pipeline_of(statement).end.as_ref().unwrap())
        .collect::<Vec<_>>();
    ids.extend(sinks.iter().map(|sink| id_text(&sink.id)));
    let ids = ids.into_iter().map(Option::unwrap).collect::<Vec<_>>();
    assert_eq!(ids, ["m", "h", "g", "w", "e1", "e2", "e3", "e4", "d", "x"]);

    // Each `as` is part of its form's span, and each ID's span covers the identifier.
    assert_eq!(text(&source, tuple(&pipeline.head).span), "(a, b) as m");
    let host = op(&pipeline.stages[0]);
    assert_eq!(text(&source, host.span), "host_enrichment as h");
    assert_eq!(text(&source, host.id.as_ref().unwrap().span), "h");
    assert_eq!(text(&source, where_(&pipeline.stages[2]).span), "where true as w");
    assert_eq!(text(&source, sinks[0].span), "emit primary as e1");
    assert_eq!(text(&source, sinks[4].span), "drop as d");
}

// Sinks.

#[test]
fn every_sink_parses() {
    let body = "  a | emit primary\n  a | emit \"https://x.example.com\"\n  a | emit [primary, \"https://y\"]\n  a | \
                emit []\n  a | emit output o\n  a | drop\n  a | exit aggregated\n  a | emit [output, as]";
    let source = policy(body);
    let program = parse_ok(&source);
    let sinks = program
        .statements
        .iter()
        .map(|statement| {
            let sink = pipeline_of(statement).end.as_ref().unwrap();
            let described = match &sink.kind {
                SinkKind::Recipients { names, span } => format!("to {:?} at {}", names, text(&source, *span)),
                SinkKind::Output { output, span } => format!("output {} at {}", output, text(&source, *span)),
                SinkKind::Drop => "drop".to_string(),
                SinkKind::Exit { name, span } => format!("exit {} at {}", name, text(&source, *span)),
            };
            (described, text(&source, sink.span).to_string())
        })
        .collect::<Vec<_>>();
    let expected = [
        ("to [\"primary\"] at primary", "emit primary"),
        (
            "to [\"https://x.example.com\"] at \"https://x.example.com\"",
            "emit \"https://x.example.com\"",
        ),
        (
            "to [\"primary\", \"https://y\"] at [primary, \"https://y\"]",
            "emit [primary, \"https://y\"]",
        ),
        ("to [] at []", "emit []"),
        ("output o at o", "emit output o"),
        ("drop", "drop"),
        ("exit aggregated at aggregated", "exit aggregated"),
        ("to [\"output\", \"as\"] at [output, as]", "emit [output, as]"),
    ];
    assert_eq!(
        sinks,
        expected
            .iter()
            .map(|(described, span)| (described.to_string(), span.to_string()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn fork_is_gone() {
    // Wherever `fork` would have been written, the error says what replaced it.
    let gone = "`fork` is gone: name the stream with `let` and use it once per branch.";
    for body in [
        "  dogstatsd | fork {\n    drop\n  }",
        "  dogstatsd | host_enrichment | fork as f { emit primary }",
        "  let a = dogstatsd | fork { drop }",
        "  dogstatsd | host_enrichment fork { drop }",
        "  dogstatsd | drop\n  fork { drop }",
    ] {
        assert_error(body, "fork", gone);
    }

    // A stream may still be named `fork`, and an operator, written with parentheses.
    let statements = statements("  let fork = dogstatsd | fork()\n  fork | drop");
    assert_eq!(op(&pipeline_of(&statements[0]).stages[0]).op.as_str(), "fork");
    assert_eq!(ref_text(reference(&pipeline_of(&statements[1]).head)), "fork");
}

// The syntax rules.

#[test]
fn sinks_end_pipelines() {
    for sink in ["emit primary", "emit output o", "drop", "exit aggregated"] {
        let word = sink.split(' ').next().unwrap();
        assert_error(
            &format!("  dogstatsd | {} | host_enrichment | drop", sink),
            word,
            &format!(
                "`{}` ends a pipeline; to send a copy and keep going, name the stream with `let` and use it twice.",
                word
            ),
        );
    }
}

#[test]
fn a_word_with_arguments_right_after_it_is_an_operator() {
    // The core form of an `emit` whose parameters no sink writes, in a `let`, and operators named like the other
    // stages, or like `as`.
    let body = "  let e = dogstatsd | emit(output: \"o\", to: []) | drop() | where() as w | fork() | exit()\n  \
                e | as() as a | drop";
    let statements = statements(body);
    let ops = pipeline_of(&statements[0])
        .stages
        .iter()
        .map(|stage| op(stage).op.to_string())
        .collect::<Vec<_>>();
    assert_eq!(ops, ["emit", "drop", "where", "fork", "exit"]);
    assert_eq!(
        op(&pipeline_of(&statements[0]).stages[0]).params,
        serde_json::from_value::<Params>(json!({"output": "o", "to": []})).unwrap()
    );
    let stage = op(&pipeline_of(&statements[1]).stages[0]);
    assert_eq!(stage.op.as_str(), "as");
    assert_eq!(id_text(&stage.id).as_deref(), Some("a"));

    // With a space, the word is what it is otherwise.
    assert_error("  dogstatsd | emit (x)", "(", "Expected what `emit` sends to");
}

#[test]
fn a_let_does_not_end() {
    assert_error(
        "  let a = dogstatsd | host_enrichment | emit primary",
        "emit",
        "`emit` ends a pipeline, but `let a` names a stream, which later statements continue: end it in a statement \
         of its own, as in `a | emit ...`, or write this pipeline without `let a =`.",
    );
    assert_error(
        "  let a = dogstatsd | drop",
        "drop",
        "`drop` ends a pipeline, but `let a`",
    );
}

#[test]
fn a_bare_statement_ends() {
    assert_error(
        "  dogstatsd | host_enrichment",
        "dogstatsd | host_enrichment",
        "This pipeline does not end: a statement without `let` ends in a sink (`emit`, `drop`, or `exit`).",
    );
    assert_error("  (a, b) as m", "(a, b) as m", "This pipeline does not end");
}

#[test]
fn structural_errors() {
    assert_error(
        "  dogstatsd | host_enrichment emit primary",
        "emit",
        "Expected `|` before `emit`, which is a stage: it follows a stream and `|`, as in `dogstatsd | emit ...`.",
    );
    assert_error(
        "  let a = dogstatsd where true",
        "where",
        "Expected `|` before `where`, which is a stage",
    );
    assert_error(
        "  emit primary",
        "emit",
        "Expected the start of a pipeline, found `emit`, which is a stage: it follows a stream and `|`",
    );
    assert_error(
        "  dogstatsd -> drop",
        "->",
        "`->` is retired: join a pipeline's stages with `|`",
    );
    assert_error("  node a = tee", "node", "`node <id> = ...` is retired");
    assert_error("  input.dogstatsd | drop", "input.dogstatsd", "`input.` is retired");
    assert_error(
        "  dogstatsd as d | drop",
        "as",
        "`as` names a node, and a stream's name is not one",
    );
    assert_error("  () | drop", "()", "A tuple merges one or more streams");
    assert_error(
        "  (a b) | drop",
        "b",
        "Expected `,` or `)` after the stream, to continue or close the tuple opened at line 2, column 3",
    );
    assert_error("  let = a", "=", "Expected a name for the stream after `let`");
    assert_error("  let a a", "a", "Expected `=` and the stream's pipeline");
    assert_error("  dogstatsd | ", "}", "Expected a stage after `|`");
    assert_error("  dogstatsd | 5", "5", "Expected a stage after `|`");
    assert_error("  dogstatsd | drop as", "}", "Expected a node name after `as`");
    assert_error("  dogstatsd | emit", "}", "Expected what `emit` sends to");
    assert_error(
        "  dogstatsd | emit output",
        "}",
        "Expected an output ID after `emit output`",
    );
    assert_error("  dogstatsd | exit", "}", "Expected a scope exit name after `exit`");
    assert_error("  Dogstatsd | drop", "Dogstatsd", "");
}

#[test]
fn unclosed_policies_point_at_their_opening() {
    let source = "policy metrics {\n  a | drop\n";
    let errors = parse(source).unwrap_err();
    assert_eq!(
        errors.last().unwrap().message,
        "Expected `}` to close the policy opened at line 1, column 16, found the end of the input."
    );

    let errors = parse("policy metrics {} extra").unwrap_err();
    assert!(errors[0]
        .message
        .contains("the end of the input after the policy's closing"));
    let errors = parse("metrics {}").unwrap_err();
    assert!(errors[0].message.contains("Expected `policy`"));
}

// Values.

#[test]
fn values_convert_to_json() {
    let hash = ZERO_HASH;
    let body = format!(
        "  dogstatsd | frobnicate(s: \"x\", n: -2, f: 1.5, d: 10s, t: true, u: false, z: null, l: [1, [2]], r: {{ a: \
         1, \"b c\": 2 }}, c: config.dogstatsd.metric_filter.values, c2: config.group.0123456789abcdef.names, c3: \
         config.a.10s, c4: config.a.0, b: blob(\"{}\")) | drop",
        hash
    );
    let statements = statements(&body);
    let params = &op(&pipeline_of(&statements[0]).stages[0]).params;
    let expected = json!({
        "s": "x", "n": -2, "f": 1.5, "d": "10s", "t": true, "u": false, "z": null, "l": [1, [2]],
        "r": {"a": 1, "b c": 2},
        "c": {"config": "dogstatsd.metric_filter.values"},
        "c2": {"config": "group.0123456789abcdef.names"}, "c3": {"config": "a.10s"}, "c4": {"config": "a.0"},
        "b": {"blob": hash},
    });
    for (name, value) in expected.as_object().unwrap() {
        assert_eq!(params.get(name), Some(value), "{}", name);
    }
}

#[test]
fn value_errors() {
    assert_error(
        "  dogstatsd | host_tags(tags: config(\"host.tags\"), deadline: 1s) | drop",
        "config(\"host.tags\")",
        "`config(\"...\")` is retired: a configuration binding is written `config.` and the binding's name, as in \
         `config.host.tags`.",
    );
    assert_error(
        "  dogstatsd | host_tags(tags: config.Host, deadline: 1s) | drop",
        "config.Host",
        "binding name",
    );
    assert_error(
        "  dogstatsd | host_tags(tags: config., deadline: 1s) | drop",
        ",",
        "Expected the next segment of the binding's name after `.`",
    );
    assert_error(
        "  dogstatsd | frobnicate(a: bare) | drop",
        "bare",
        "Expected a value, found `bare`. Strings are written in double quotes",
    );
    assert_error(
        "  dogstatsd | frobnicate(a: { x: 1, x: 2 }) | drop",
        "x",
        "The member `x` is given twice.",
    );
    assert_error(
        "  dogstatsd | frobnicate(a: [1, 2) | drop",
        ")",
        "`,` or `]` after the list element",
    );
}

// Predicates.

#[test]
fn every_predicate_row_has_its_json() {
    // (predicate, its JSON)
    let cases: Vec<(&str, Value)> = vec![
        // Name, intake name, and host, with every string match.
        (r#"name == "a""#, json!({"name": {"eq": "a"}})),
        (r#"intake_name == "a""#, json!({"intake_name": {"eq": "a"}})),
        (r#"host == "a""#, json!({"host": {"eq": "a"}})),
        (r#"name starts_with "a.""#, json!({"name": {"prefix": "a."}})),
        (r#"name in ["a", "b"]"#, json!({"name": {"in": ["a", "b"]}})),
        (r#"name in []"#, json!({"name": {"in": []}})),
        (
            r#"intake_name starts_with ["a.", "b."]"#,
            json!({"intake_name": {"prefix_in": ["a.", "b."]}}),
        ),
        (
            "name in config.dogstatsd.metric_filter.values",
            json!({"name": {"matches": {"values": {"config": "dogstatsd.metric_filter.values"}, "prefix": false}}}),
        ),
        (
            "host starts_with config.f.values",
            json!({"host": {"matches": {"values": {"config": "f.values"}, "prefix": true}}}),
        ),
        (
            r#"name matches(values: ["a"], prefix: false)"#,
            json!({"name": {"matches": {"values": ["a"], "prefix": false}}}),
        ),
        (
            "intake_name matches(prefix: config.f.prefix, values: config.f.values)",
            json!({"intake_name": {"matches": {"values": {"config": "f.values"}, "prefix": {"config": "f.prefix"}}}}),
        ),
        // Inequality.
        (r#"name != "a""#, json!({"not": {"name": {"eq": "a"}}})),
        (r#"host != "a""#, json!({"not": {"host": {"eq": "a"}}})),
        // Host presence.
        ("host is unset", json!({"host_unset": {}})),
        ("host is set", json!({"not": {"host_unset": {}}})),
        // Tags.
        ("tags.env is set", json!({"has_tag": "env"})),
        ("tags.env is unset", json!({"not": {"has_tag": "env"}})),
        (
            r#"tags["app.kubernetes.io/name"] is set"#,
            json!({"has_tag": "app.kubernetes.io/name"}),
        ),
        (
            r#"tags.team == "platform""#,
            json!({"tag": {"key": "team", "value": {"eq": "platform"}}}),
        ),
        (
            r#"tags.team != "platform""#,
            json!({"not": {"tag": {"key": "team", "value": {"eq": "platform"}}}}),
        ),
        (
            r#"tags["a b"] starts_with ["x", "y"]"#,
            json!({"tag": {"key": "a b", "value": {"prefix_in": ["x", "y"]}}}),
        ),
        (
            "tags.team in config.teams",
            json!({"tag": {"key": "team", "value": {"matches": {"values": {"config": "teams"}, "prefix": false}}}}),
        ),
        // Origin tags.
        ("origin_tags.pod is set", json!({"has_origin_tag": "pod"})),
        ("origin_tags.pod is unset", json!({"not": {"has_origin_tag": "pod"}})),
        (
            r#"origin_tags.pod starts_with "web-""#,
            json!({"origin_tag": {"key": "pod", "value": {"prefix": "web-"}}}),
        ),
        (
            r#"origin_tags["k.8s"] == "x""#,
            json!({"origin_tag": {"key": "k.8s", "value": {"eq": "x"}}}),
        ),
        // Kinds, constants, flags, and empty lists.
        ("kind in [counter, gauge]", json!({"kind_in": ["counter", "gauge"]})),
        ("kind in []", json!({"kind_in": []})),
        ("true", json!({"const": true})),
        ("false", json!({"const": false})),
        (
            "config.multi_region_failover.enabled",
            json!({"flag": {"config": "multi_region_failover.enabled"}}),
        ),
        ("is_empty(config.x)", json!({"is_empty": {"config": "x"}})),
        ("is_empty([])", json!({"is_empty": []})),
        (r#"is_empty(["a"])"#, json!({"is_empty": ["a"]})),
        // Compositions.
        ("not true", json!({"not": {"const": true}})),
        ("not not true", json!({"not": {"not": {"const": true}}})),
        (
            "true and false and true",
            json!({"all": [{"const": true}, {"const": false}, {"const": true}]}),
        ),
        ("true or false", json!({"any": [{"const": true}, {"const": false}]})),
        // Raw values, so that any artifact parses back.
        ("{ x: 1 }", json!({"x": 1})),
        ("null", json!(null)),
        ("{ flag: true }", json!({"flag": true})),
        ("{ all: [] }", json!({"all": []})),
        (
            "{ any: [true, { const: false }] }",
            json!({"any": [true, {"const": false}]}),
        ),
        (
            "{ kind_in: [\"gauge\", \"counter\"] }",
            json!({"kind_in": ["gauge", "counter"]}),
        ),
        ("[1]", json!([1])),
        ("\"s\"", json!("s")),
        ("5", json!(5)),
    ];
    for (predicate, expected) in cases {
        let found = predicate_json(predicate);
        assert_eq!(found, expected, "{}", predicate);
        let raw = predicate.starts_with(['{', '[', '"', '5']) || predicate == "null";
        if !raw {
            let decoded = serde_json::from_value::<Predicate>(found.clone());
            assert!(decoded.is_ok(), "{}: {:?}", predicate, decoded);
            assert_eq!(serde_json::to_value(decoded.unwrap()).unwrap(), found, "{}", predicate);
        }
    }
}

#[test]
fn not_binds_tightest_then_and_then_or() {
    assert_eq!(
        predicate_json("not true and false or true and not false"),
        json!({"any": [
            {"all": [{"not": {"const": true}}, {"const": false}]},
            {"all": [{"const": true}, {"not": {"const": false}}]},
        ]})
    );
    assert_eq!(
        predicate_json(r#"not (name == "a" or host is set)"#),
        json!({"not": {"any": [{"name": {"eq": "a"}}, {"not": {"host_unset": {}}}]}})
    );
    assert_eq!(
        predicate_json(r#"not name != "a""#),
        json!({"not": {"not": {"name": {"eq": "a"}}}})
    );
    // `and` and `or` keep the nesting written: only parentheses nest junctions of one kind.
    assert_eq!(
        predicate_json("true and (false and true)"),
        json!({"all": [{"const": true}, {"all": [{"const": false}, {"const": true}]}]})
    );
    assert_eq!(
        predicate_json("(true or false) or true"),
        json!({"any": [{"any": [{"const": true}, {"const": false}]}, {"const": true}]})
    );
    assert_eq!(
        predicate_json("true or false or true"),
        json!({"any": [{"const": true}, {"const": false}, {"const": true}]})
    );
    assert_eq!(predicate_json("(true)"), json!({"const": true}));
}

#[test]
fn where_spans_start_at_the_predicate() {
    let source = policy("  dogstatsd | where not tags.env is set as w | drop");
    let program = parse_ok(&source);
    let stage = where_(&pipeline_of(&program.statements[0]).stages[0]);
    let spans = stage
        .param_spans
        .iter()
        .map(|(path, span)| (path.as_str(), text(&source, *span)))
        .collect::<Vec<_>>();
    assert_eq!(spans, [("predicate", "not tags.env is set")]);
    assert_eq!(text(&source, stage.span), "where not tags.env is set as w");
}

#[test]
fn retired_predicates_name_their_replacements() {
    // (predicate, offending text, message)
    let cases = [
        (
            "all(true, false)",
            "all",
            "`all(...)` is retired: join the predicates with `and`, as in `a and b and c`.",
        ),
        (
            "any(true, false)",
            "any",
            "`any(...)` is retired: join the predicates with `or`, as in `a or b or c`.",
        ),
        (
            r#"name prefix_in ["a."]"#,
            "prefix_in",
            "`prefix_in` is retired: write `starts_with` and the list",
        ),
        (
            r#"has_tag("env")"#,
            "has_tag",
            "`has_tag(...)` is retired: write `tags.env is set`.",
        ),
        (
            r#"has_tag("app.kubernetes.io/name")"#,
            "has_tag",
            "`has_tag(...)` is retired: write `tags[\"app.kubernetes.io/name\"] is set`.",
        ),
        (
            r#"has_origin_tag("pod")"#,
            "has_origin_tag",
            "`has_origin_tag(...)` is retired: write `origin_tags.pod is set`.",
        ),
        (
            r#"tag("team") == "platform""#,
            "tag",
            "`tag(...)` is retired: write `tags.team` and the match, as in `tags.team == \"platform\"`.",
        ),
        (
            r#"origin_tag("pod") starts_with "web-""#,
            "origin_tag",
            "`origin_tag(...)` is retired: write `origin_tags.pod` and the match",
        ),
        (
            "flag(config.x)",
            "flag",
            "`flag(...)` is retired: write the binding alone, as in `config.multi_region_failover.enabled`",
        ),
        (
            r#"config("a.b")"#,
            r#"config("a.b")"#,
            "`config(\"...\")` is retired: a configuration binding is written `config.` and the binding's name, as in \
             `config.a.b`.",
        ),
        (
            r#"name in config("a.b")"#,
            r#"config("a.b")"#,
            "`config(\"...\")` is retired",
        ),
        (
            r#"name starts_with config("a.b")"#,
            r#"config("a.b")"#,
            "`config(\"...\")` is retired",
        ),
        (
            r#"is_empty(config("a.b"))"#,
            r#"config("a.b")"#,
            "`config(\"...\")` is retired",
        ),
    ];
    for (predicate, offending, message) in cases {
        let source = policy(&format!("  dogstatsd | where {} | drop", predicate));
        let errors = parse(&source).unwrap_err();
        let error = errors.last().unwrap();
        assert_eq!(text(&source, error.span), offending, "{}: {}", predicate, error.message);
        assert!(error.message.contains(message), "{}: {}", predicate, error.message);
    }
}

#[test]
fn predicate_errors() {
    assert_predicate_error(
        "",
        "|",
        "Expected a predicate, such as `name == \"requests\"`, `tags.env is set`",
    );
    assert_predicate_error("name", "|", "Expected a string match: `== \"<s>\"`, `!= \"<s>\"`");
    assert_predicate_error("name == 5", "5", "Expected a string after `==`");
    assert_predicate_error("name starts_with 5", "5", "after `starts_with`");
    assert_predicate_error("name in 5", "5", "after `in`");
    assert_predicate_error("name matches", "|", "Expected `(` after `matches`");
    assert_predicate_error(
        r#"name matches(values: ["a"])"#,
        r#"(values: ["a"])"#,
        "`matches` needs both `values:` and `prefix:`; `prefix:` is missing.",
    );
    assert_predicate_error(
        "name matches(values: [], values: [])",
        "values",
        "`values` is given twice.",
    );
    assert_predicate_error("host is", "|", "Expected `set` or `unset` after `is`");
    assert_predicate_error("tags.env is maybe", "maybe", "Expected `set` or `unset` after `is`");
    assert_predicate_error(
        "tags.app.name is set",
        ".",
        "a key that is not an identifier, such as one with a `.` in it, is quoted, as in \
         `tags[\"app.kubernetes.io/name\"]`",
    );
    assert_predicate_error("tags.5 is set", "5", "Expected a tag key after `tags.`");
    assert_predicate_error("tags.\"a\" is set", "\"a\"", "Expected a tag key after `tags.`");
    assert_predicate_error(
        "tags[env] is set",
        "env",
        "Expected the tag key as a string after `tags[`",
    );
    assert_predicate_error(r#"tags["env" is set"#, "is", "Expected `]` to close the tag key");
    assert_predicate_error(
        "kind counter",
        "counter",
        "Expected `in` and a list of kinds after `kind`",
    );
    assert_predicate_error(
        "kind in [counter, counter]",
        "counter",
        "The kind `counter` is listed twice.",
    );
    assert_predicate_error("kind in [meter]", "meter", "Unknown metric kind `meter`");
    assert_predicate_error("is_empty", "|", "Expected `(` after `is_empty`");
    assert_predicate_error("(true", "|", "Expected `and`, `or`, or `)` to close the parenthesis");
    assert_predicate_error("tags", "tags", "Expected a predicate");
}

// Limits.

#[test]
fn nesting_is_bounded() {
    let depth_error = format!("nests more than {} levels deep", MAX_DEPTH);

    // A predicate takes one level, and each `not` or parenthesis one more.
    let nots = |count: usize| format!("{}true", "not ".repeat(count));
    let parens = |count: usize| format!("{}true{}", "(".repeat(count), ")".repeat(count));
    for predicate in [nots(MAX_DEPTH - 1), parens(MAX_DEPTH - 1)] {
        parse_ok(&policy(&format!("  dogstatsd | where {} | drop", predicate)));
    }
    for predicate in [nots(MAX_DEPTH), parens(MAX_DEPTH)] {
        let (_, message, _) = error_in(&format!("  dogstatsd | where {} | drop", predicate));
        assert!(message.contains(&depth_error), "{}", message);
    }

    // Each list or record takes one level.
    let lists = |count: usize| format!("{}{}", "[".repeat(count), "]".repeat(count));
    parse_ok(&policy(&format!(
        "  dogstatsd | frobnicate(a: {}) | drop",
        lists(MAX_DEPTH)
    )));
    let (offending, message, _) = error_in(&format!("  dogstatsd | frobnicate(a: {}) | drop", lists(MAX_DEPTH + 1)));
    assert_eq!(offending, "[");
    assert!(message.contains(&depth_error), "{}", message);
}

#[test]
fn deep_nesting_does_not_exhaust_the_stack() {
    // Far past the limit, parsing stops at the limit rather than recursing.
    let deep = 100_000;
    for body in [
        format!("  dogstatsd | where {}true | drop", "not ".repeat(deep)),
        format!("  dogstatsd | where {}true | drop", "(".repeat(deep)),
        format!("  dogstatsd | frobnicate(a: {}) | drop", "[".repeat(deep)),
    ] {
        let (_, message, _) = error_in(&body);
        assert!(message.contains("levels deep"), "{}", message);
    }
}

/// The words and symbols that generated sources are made of.
const VOCABULARY: &[&str] = &[
    "policy",
    "metrics",
    "{",
    "}",
    "let",
    "a",
    "=",
    "|",
    "(",
    ")",
    ",",
    ".",
    "true",
    "as",
    "where",
    "fork",
    "emit",
    "output",
    "drop",
    "exit",
    "name",
    "==",
    "!=",
    "\"s\"",
    "[",
    "]",
    "not",
    "and",
    "or",
    "tags",
    "is",
    "set",
    "config",
    "aggregate",
    "window",
    ":",
    "10s",
    "1",
    "split",
    "kind",
    "in",
    "counter",
];

proptest! {
    #[test]
    fn property_test_any_token_sequence_parses_or_reports_an_error_in_the_source(
        words in prop::collection::vec(prop::sample::select(VOCABULARY), 0..60),
    ) {
        let source = format!("policy metrics {{ {} }}", words.join(" "));
        match parse(&source) {
            Ok(_) => {}
            Err(errors) => {
                prop_assert!(!errors.is_empty());
                for error in errors {
                    prop_assert!(error.span.start <= error.span.end && error.span.end <= source.len());
                    prop_assert!(source.is_char_boundary(error.span.start) && source.is_char_boundary(error.span.end));
                }
            }
        }
    }
}
