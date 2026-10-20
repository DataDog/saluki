//! Every grammar form, parsed against hand-written artifacts and printed against hand-written text.

use saluki_metric_plan::{
    ir::{Blobs, PolicyArtifact},
    operators::{ConfigureError, OperatorRegistry},
};
use serde_json::{json, Value};

use super::*;

/// Returns a blob table document holding one blob.
fn blob_table(hash: &str, blob: Value) -> Value {
    let mut table = serde_json::Map::new();
    table.insert(hash.to_string(), blob);
    Value::Object(table)
}

/// The routing example: a source used twice, a `where`, and inline terminals.
fn routing_source() -> String {
    r#"policy {
  metrics | emit output dd_metrics_encode

  let metrics_routing_filter_0 = metrics | where intake_name starts_with ["demo.checkout."]

  metrics_routing_filter_0 | emit output metrics_routing_encode_0
}
"#
    .to_string()
}

fn routing_artifact() -> PolicyArtifact {
    artifact(json!({
        "ir_version": 1,
        "nodes": [
            {"id": "input_metrics_fanout", "op": "tee", "params": {"ports": 2}},
            {"id": "input_metrics_fanout_0", "op": "emit", "params": {"output": "dd_metrics_encode"}},
            {"id": "metrics_routing_filter_0", "op": "split",
             "params": {"predicate": {"intake_name": {"prefix_in": ["demo.checkout."]}}}},
            {"id": "metrics_routing_filter_0_true", "op": "emit", "params": {"output": "metrics_routing_encode_0"}},
            {"id": "metrics_routing_filter_0_false", "op": "drop"},
        ],
        "edges": [
            {"from": "input.metrics", "to": "input_metrics_fanout.in"},
            {"from": "input_metrics_fanout.0", "to": "input_metrics_fanout_0.in"},
            {"from": "input_metrics_fanout.1", "to": "metrics_routing_filter_0.in"},
            {"from": "metrics_routing_filter_0.true", "to": "metrics_routing_filter_0_true.in"},
            {"from": "metrics_routing_filter_0.false", "to": "metrics_routing_filter_0_false.in"},
        ],
    }))
}

#[test]
fn an_empty_policy_is_a_program() {
    let source = "policy {\n}\n";
    let expected = artifact(json!({
        "ir_version": 1,
        "nodes": [],
        "edges": [],
    }));
    assert_round_trip(source, &expected);
}

#[test]
fn sugar_and_inline_terminals_build_the_routing_example() {
    assert_round_trip(&routing_source(), &routing_artifact());
}

#[test]
fn the_core_forms_build_the_same_artifact_as_the_sugar() {
    // The routing example in the core: an explicit `tee` and `split`, ports picked by `.0`, `.true`, and `.false`, a
    // `drop` written out, and the statements in another order, some of them on one line.
    let source = r#"policy {
  # The routing example in the core forms, in another order, on fewer lines.
  let input_metrics_fanout = metrics | tee(ports: 2)
  let metrics_routing_filter_0 = input_metrics_fanout.1 | split(predicate: intake_name starts_with ["demo.checkout."])
  metrics_routing_filter_0.false | drop
  metrics_routing_filter_0.true | emit output metrics_routing_encode_0
  input_metrics_fanout.0 | emit output dd_metrics_encode
}"#
    .to_string();
    let parsed = parse_ok(&source);
    assert_eq!(parsed.artifact, routing_artifact());
    // The sugar is used where it applies, and the terminals named by the derived-ID rule print inline.
    assert_eq!(print_default(&parsed.artifact), routing_source());

    // Reuse: a stream used twice is followed by a `tee` named `<stream>_fanout`, whose ports feed the uses in order.
    let reuse = r#"policy {
  let enriched = metrics | host_enrichment

  enriched | emit output dd_metrics_encode

  enriched | emit output dsd_client_telemetry
}
"#
    .to_string();
    let explicit = r#"policy {
  let enriched = metrics | host_enrichment
  let enriched_fanout = enriched | tee(ports: 2)

  enriched_fanout.0 | emit output dd_metrics_encode
  enriched_fanout.1 | emit output dsd_client_telemetry
}
"#
    .to_string();
    let expected = artifact(json!({
        "ir_version": 1,
        "nodes": [
            {"id": "enriched", "op": "host_enrichment"},
            {"id": "enriched_fanout", "op": "tee", "params": {"ports": 2}},
            {"id": "enriched_fanout_0", "op": "emit", "params": {"output": "dd_metrics_encode"}},
            {"id": "enriched_fanout_1", "op": "emit", "params": {"output": "dsd_client_telemetry"}},
        ],
        "edges": [
            {"from": "input.metrics", "to": "enriched.in"},
            {"from": "enriched.out", "to": "enriched_fanout.in"},
            {"from": "enriched_fanout.0", "to": "enriched_fanout_0.in"},
            {"from": "enriched_fanout.1", "to": "enriched_fanout_1.in"},
        ],
    }));
    assert_eq!(parse_ok(&reuse).artifact, expected);
    assert_eq!(parse_ok(&explicit).artifact, expected);
    // Reuse is how the tee prints, whatever its uses end in.
    assert_eq!(print_default(&expected), reuse);

    // A tee with any other ID is written in the core form, under a `let` named for it.
    let renamed = serde_json::to_string(&expected)
        .unwrap()
        .replace("enriched_fanout", "copies");
    let renamed = PolicyArtifact::from_json(&renamed).unwrap();
    let printed = r#"policy {
  let enriched = metrics | host_enrichment

  let copies = enriched | tee(ports: 2)

  copies.0 | emit output dd_metrics_encode

  copies.1 | emit output dsd_client_telemetry
}
"#
    .to_string();
    assert_round_trip(&printed, &renamed);
}

#[test]
fn spans_lead_back_to_nodes_parameters_and_edges() {
    let source = routing_source();
    let spans = parse_ok(&source).spans;

    // The implicit tee after a source has the span of the input's first use; a `where`'s node is the `where`.
    let node = spans.node("input_metrics_fanout").unwrap();
    assert_eq!(text(&source, node), "metrics");
    assert_eq!(node.start, source.find("  metrics | emit").unwrap() + 2);
    assert_eq!(
        text(&source, spans.node("input_metrics_fanout_0").unwrap()),
        "emit output dd_metrics_encode"
    );
    let filter = r#"where intake_name starts_with ["demo.checkout."]"#;
    assert_eq!(text(&source, spans.node("metrics_routing_filter_0").unwrap()), filter);
    assert_eq!(
        text(&source, spans.node("metrics_routing_filter_0_true").unwrap()),
        "emit output metrics_routing_encode_0"
    );
    // The `drop` a `where` makes has the `where`'s span.
    assert_eq!(
        text(&source, spans.node("metrics_routing_filter_0_false").unwrap()),
        filter
    );
    assert_eq!(spans.node("missing"), None);

    // An implicit tee's port count comes from the uses, so the parameter has no span of its own.
    assert_eq!(spans.param("input_metrics_fanout", "ports"), None);
    assert_eq!(
        text(&source, spans.param("metrics_routing_filter_0", "predicate").unwrap()),
        r#"intake_name starts_with ["demo.checkout."]"#
    );
    // Paths inside a predicate fall back to the predicate.
    assert_eq!(
        spans.param("metrics_routing_filter_0", "predicate.intake_name"),
        spans.param("metrics_routing_filter_0", "predicate")
    );
    assert_eq!(
        text(&source, spans.param("input_metrics_fanout_0", "output").unwrap()),
        "dd_metrics_encode"
    );

    // Edges in the artifact's order: each has the span of the element it enters.
    let edges = (0..5)
        .map(|i| text(&source, spans.edge(i).unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(
        edges,
        [
            text(&source, node),
            "emit output dd_metrics_encode",
            filter,
            "emit output metrics_routing_encode_0",
            filter,
        ]
    );
    assert_eq!(spans.edge(5), None);
}

#[test]
fn spans_map_parameter_errors_back_to_the_source() {
    let source = policy(
        r#"  metrics
    | tag_projection(
        rules: [{ metric_name: "a", action: "drop_it", tags: [] }],
        value_rules: [],
        cache_capacity: -1,
      ) as scrub
    | drop"#,
    );
    let parsed = parse_ok(&source);
    let node = &parsed.artifact.nodes[0];

    // The validator reports parameter problems by node and parameter path, as configuring the operator does.
    let errors = match OperatorRegistry::native().configure(node.op.as_str(), &node.params) {
        Err(ConfigureError::InvalidParams { errors, .. }) => errors,
        other => panic!("the parameters should be invalid: {:?}", other),
    };
    let located = errors
        .iter()
        .map(|error| {
            let span = parsed
                .spans
                .param(node.id.as_str(), &error.path)
                .expect("every path has a span");
            (error.path.as_str(), text(&source, span))
        })
        .collect::<Vec<_>>();
    assert_eq!(located, [("rules[0].action", "\"drop_it\""), ("cache_capacity", "-1")]);
}

#[test]
fn values_convert_to_their_json_shapes_and_print_back() {
    let mut blobs = Blobs::default();
    let hash = blobs.insert(json!(["a", "b"])).unwrap().to_string();
    // Escapes, multi-byte characters, and characters that do not print.
    let tags = r#"["quote\" back\\ tab\t nl\n", "é ✓ 😀", "\u{1}\u{202e}"]"#;
    let source = format!(
        r#"policy {{
  blobs {{
    "{hash}" = ["a", "b"],
  }}

  let scrub = metrics
    | tag_projection(rules: [{{ metric_name: "a.b", action: "exclude", tags: ["user_id"] }}], cache_capacity: 1000)

  let stamp = scrub | host_tags({tags}, deadline: 1700000300s)

  let agg = stamp
    | aggregate(
        window: 10s,
        flush_interval: config.dogstatsd.aggregation.flush_interval,
        context_limit: -5,
        counter_expiry: "1 minute",
        flush_open_windows: true,
        histogram_aggregates: ["max", "median"],
        histogram_percentiles: [95, 99],
        copy_to_distribution: true,
        copy_to_distribution_prefix: "dist.",
      )

  let odd = agg
    | frobnicate(
        big: 18446744073709551615,
        nothing: null,
        ratio: 1.5,
        record: {{ nested: {{ deeper: [true, false] }}, plain: [], "weird key": 1 }},
        shared: blob("{hash}"),
        small: -9223372036854775808,
        span: "500ms",
      )

  odd | drop
}}
"#
    );
    let expected = artifact(json!({
        "ir_version": 1,
        "nodes": [
            {"id": "scrub", "op": "tag_projection", "params": {
                "rules": [{"metric_name": "a.b", "action": "exclude", "tags": ["user_id"]}],
                "cache_capacity": 1000,
            }},
            {"id": "stamp", "op": "host_tags", "params": {
                "tags": ["quote\" back\\ tab\t nl\n", "é ✓ \u{1F600}", "\u{1}\u{202E}"],
                "deadline": "1700000300s",
            }},
            {"id": "agg", "op": "aggregate", "params": {
                "window": "10s",
                "flush_interval": {"config": "dogstatsd.aggregation.flush_interval"},
                "context_limit": -5,
                "counter_expiry": "1 minute",
                "flush_open_windows": true,
                "histogram_aggregates": ["max", "median"],
                "histogram_percentiles": [95, 99],
                "copy_to_distribution": true,
                "copy_to_distribution_prefix": "dist.",
            }},
            {"id": "odd", "op": "frobnicate", "params": {
                "big": 18446744073709551615u64,
                "nothing": null,
                "ratio": 1.5,
                "record": {"weird key": 1, "plain": [], "nested": {"deeper": [true, false]}},
                "shared": {"blob": hash.clone()},
                "small": i64::MIN,
                "span": "500ms",
            }},
            // An unknown operator is assumed to have one output, `out`.
            {"id": "odd_out", "op": "drop"},
        ],
        "edges": [
            {"from": "input.metrics", "to": "scrub.in"},
            {"from": "scrub.out", "to": "stamp.in"},
            {"from": "stamp.out", "to": "agg.in"},
            {"from": "agg.out", "to": "odd.in"},
            {"from": "odd.out", "to": "odd_out.in"},
        ],
        "blobs": blob_table(&hash, json!(["a", "b"])),
    }));
    assert_round_trip(&source, &expected);
}

#[test]
fn other_spellings_of_values_convert_the_same_way() {
    // (value as written, its JSON)
    let cases: Vec<(&str, Value)> = vec![
        ("500ms", json!("500ms")),
        ("060s", json!("060s")),
        ("\"10s\"", json!("10s")),
        ("-0", json!(0)),
        ("1e3", json!(1000.0)),
        ("[1, 2,]", json!([1, 2])),
        ("[]", json!([])),
        ("{}", json!({})),
        ("{ a: 1, \"b c\": [], }", json!({"a": 1, "b c": []})),
        ("{ true: null }", json!({"true": null})),
        ("config.a.b_c.0", json!({"config": "a.b_c.0"})),
        // Members that are reserved in parameters are written as they are; validation rejects them.
        ("{ config: \"Not A Binding\" }", json!({"config": "Not A Binding"})),
        ("\"\"", json!("")),
    ];
    for (written, expected) in cases {
        let source = policy(&format!("  metrics | frobnicate(v: {}) as n | drop", written));
        let parsed = parse_ok(&source);
        assert_eq!(parsed.artifact.nodes[0].params.get("v"), Some(&expected), "{}", written);
        // Whatever the spelling, printing and parsing again keeps the value.
        let reparsed = parse_ok(&print_default(&parsed.artifact));
        assert_eq!(reparsed.artifact, parsed.artifact, "{}", written);
    }
}

#[test]
fn printing_breaks_long_headers_lists_and_predicates() {
    // One line, as an author might write it.
    let compact = concat!(
        r#"policy { let ts = metrics | timestamp_split(window: 10s) let gateway = "#,
        r#"ts.default | tag_projection(rules: [{ "#,
        r#"metric_name: "checkout.requests.latency", action: "exclude", tags: ["user_id", "session_id", "#,
        r#""request_id"] }, { tags: ["env", "service", "version", "region", "availability_zone", "#,
        r#""cluster_name", "team"], action: "include", metric_name: "checkout.requests.count" }], value_rules: "#,
        r#"config.dogstatsd.tag_value_allowlist, cache_capacity: 50000) as scrub | "#,
        r#"split(config.multi_region_failover.enabled and (is_empty(config."#,
        r#"multi_region_failover.metric_mirroring.allowlist) or name matches(values: config."#,
        r#"multi_region_failover.metric_mirroring.allowlist, prefix: false)) and not host in ["#,
        r#""web-01.example.com", "web-02.example.com", "web-03.example.com", "web-04.example.com"]) "#,
        r#"gateway.true | emit output mrf_metrics_encode gateway.false | where intake_name in ["#,
        r#""checkout.requests.count", "checkout.requests.latency.p99", "checkout.errors.count", "#,
        r#""checkout.cart.items.added", "checkout.cart.items.removed"] as routing | emit output "#,
        r#"metrics_routing_encode_0 ts.passthrough | emit output dd_metrics_encode }"#
    );
    let expected = r#"policy {
  let ts = metrics | timestamp_split(10s)

  let scrub = ts.default
    | tag_projection(
        rules: [
          {
            metric_name: "checkout.requests.latency",
            action: "exclude",
            tags: ["user_id", "session_id", "request_id"],
          },
          {
            metric_name: "checkout.requests.count",
            action: "include",
            tags: ["env", "service", "version", "region", "availability_zone", "cluster_name", "team"],
          },
        ],
        value_rules: config.dogstatsd.tag_value_allowlist,
        cache_capacity: 50000,
      )

  let gateway = scrub
    | split(
        config.multi_region_failover.enabled
        and (
          is_empty(config.multi_region_failover.metric_mirroring.allowlist)
          or name in config.multi_region_failover.metric_mirroring.allowlist
        )
        and not host in ["web-01.example.com", "web-02.example.com", "web-03.example.com", "web-04.example.com"]
      )

  gateway.true | emit output mrf_metrics_encode

  let routing = gateway.false
    | where intake_name in [
        "checkout.requests.count",
        "checkout.requests.latency.p99",
        "checkout.errors.count",
        "checkout.cart.items.added",
        "checkout.cart.items.removed",
      ]

  routing | emit output metrics_routing_encode_0

  ts.passthrough | emit output dd_metrics_encode
}
"#
    .to_string();
    let printed = print_default(&parse_ok(compact).artifact);
    assert_eq!(printed, expected);
    assert!(printed.lines().all(|line| line.chars().count() <= 120));

    // A narrower width breaks more, and still parses back to the same artifact.
    let narrow = print(&parse_ok(compact).artifact, &PrintOptions { width: 40 });
    assert!(narrow.lines().count() > expected.lines().count());
    assert_eq!(parse_ok(&narrow).artifact, parse_ok(compact).artifact);
}

#[test]
fn nodes_are_ordered_by_the_graph_not_by_the_text() {
    let forward = r#"policy {
  let fork = metrics | split(tags.env is set)

  let tagged = fork.true | host_enrichment

  let joined = (tagged, fork.false)

  joined | emit output dd_metrics_encode
}
"#
    .to_string();
    let expected = artifact(json!({
        "ir_version": 1,
        "nodes": [
            {"id": "fork", "op": "split", "params": {"predicate": {"has_tag": "env"}}},
            {"id": "tagged", "op": "host_enrichment"},
            {"id": "joined", "op": "merge", "params": {"inputs": 2}},
            {"id": "joined_out", "op": "emit", "params": {"output": "dd_metrics_encode"}},
        ],
        "edges": [
            {"from": "input.metrics", "to": "fork.in"},
            {"from": "fork.true", "to": "tagged.in"},
            // The merge is reached when its last input is: the traversal finishes `tagged` first.
            {"from": "tagged.out", "to": "joined.0"},
            {"from": "fork.false", "to": "joined.1"},
            {"from": "joined.out", "to": "joined_out.in"},
        ],
    }));
    assert_round_trip(&forward, &expected);

    // Statements that do not depend on each other may come in any order: the artifact lists its nodes and edges from
    // the inputs in name order, and the text prints the same way.
    let two = r#"policy {
  metrics | host_enrichment | emit output dd_metrics_encode

  otlp | host_enrichment | drop
}
"#
    .to_string();
    let swapped = r#"policy {
  otlp | host_enrichment | drop
  metrics | host_enrichment | emit output dd_metrics_encode
}"#
    .to_string();
    let expected = artifact(json!({
        "ir_version": 1,
        "nodes": [
            {"id": "input_metrics_host_enrichment", "op": "host_enrichment"},
            {"id": "input_metrics_host_enrichment_out", "op": "emit", "params": {"output": "dd_metrics_encode"}},
            {"id": "input_otlp_host_enrichment", "op": "host_enrichment"},
            {"id": "input_otlp_host_enrichment_out", "op": "drop"},
        ],
        "edges": [
            {"from": "input.metrics", "to": "input_metrics_host_enrichment.in"},
            {"from": "input_metrics_host_enrichment.out", "to": "input_metrics_host_enrichment_out.in"},
            {"from": "input.otlp", "to": "input_otlp_host_enrichment.in"},
            {"from": "input_otlp_host_enrichment.out", "to": "input_otlp_host_enrichment_out.in"},
        ],
    }));
    assert_round_trip(&two, &expected);
    let parsed = parse_ok(&swapped);
    assert_eq!(parsed.artifact, expected);
    assert_eq!(print_default(&parsed.artifact), two);
}

#[test]
fn a_let_with_no_stages_names_a_stream() {
    // Used once, an alias makes no node: its use is the stream it names, so printing the artifact writes the stream.
    let once = policy("  let x = metrics\n  x | host_enrichment | drop");
    let parsed = parse_ok(&once);
    let ids = parsed
        .artifact
        .nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        ["input_metrics_host_enrichment", "input_metrics_host_enrichment_out"]
    );
    assert_eq!(
        print_default(&parsed.artifact),
        policy("  metrics | host_enrichment | drop")
    );

    // Used several times, it makes its own tee, `<name>_fanout`, which the use of the stream it names feeds, and
    // prints back as written.
    let several = policy(
        "  let ts = metrics | timestamp_split(10s)\n\n  ts.default | drop\n\n  let p = ts.passthrough\n\n  p | drop\n\n  \
         p | emit output dd_metrics_encode",
    );
    let expected = artifact(json!({
        "ir_version": 1,
        "nodes": [
            {"id": "ts", "op": "timestamp_split", "params": {"window": "10s"}},
            {"id": "ts_default", "op": "drop"},
            {"id": "p_fanout", "op": "tee", "params": {"ports": 2}},
            {"id": "p_fanout_0", "op": "drop"},
            {"id": "p_fanout_1", "op": "emit", "params": {"output": "dd_metrics_encode"}},
        ],
        "edges": [
            {"from": "input.metrics", "to": "ts.in"},
            {"from": "ts.default", "to": "ts_default.in"},
            {"from": "ts.passthrough", "to": "p_fanout.in"},
            {"from": "p_fanout.0", "to": "p_fanout_0.in"},
            {"from": "p_fanout.1", "to": "p_fanout_1.in"},
        ],
    }));
    assert_round_trip(&several, &expected);
}

#[test]
fn keywords_can_be_names_and_terminals_can_be_named() {
    let source = r#"policy {
  let fork = metrics | split(true)

  let drop = fork.true | host_enrichment

  drop | emit output dd_metrics_encode

  fork.false | drop as sink
}
"#
    .to_string();
    let expected = artifact(json!({
        "ir_version": 1,
        "nodes": [
            {"id": "fork", "op": "split", "params": {"predicate": {"const": true}}},
            {"id": "drop", "op": "host_enrichment"},
            {"id": "drop_out", "op": "emit", "params": {"output": "dd_metrics_encode"}},
            // Named by an `as` clause, it is not named by the derived-ID rule.
            {"id": "sink", "op": "drop"},
        ],
        "edges": [
            {"from": "input.metrics", "to": "fork.in"},
            {"from": "fork.true", "to": "drop.in"},
            {"from": "drop.out", "to": "drop_out.in"},
            {"from": "fork.false", "to": "sink.in"},
        ],
    }));
    assert_round_trip(&source, &expected);
}

#[test]
fn inline_terminals_of_scope_inputs_and_long_parents_use_the_derived_id() {
    // The derived IDs were computed independently with Python, as the plan crate's own test of the rule does.
    let long = format!("a{}", "b".repeat(62));
    let source = format!(
        r#"policy {{
  first | drop

  let {long} = second | where false

  {long} | emit output dd_metrics_encode
}}
"#
    );
    let expected = artifact(json!({
        "ir_version": 1,
        "nodes": [
            {"id": "input_first", "op": "drop"},
            {"id": long, "op": "split", "params": {"predicate": {"const": false}}},
            {"id": format!("a{}_3c6e0fb7", "b".repeat(53)), "op": "emit", "params": {"output": "dd_metrics_encode"}},
            {"id": format!("a{}_08eae15a", "b".repeat(53)), "op": "drop"},
        ],
        "edges": [
            {"from": "input.first", "to": "input_first.in"},
            {"from": "input.second", "to": format!("{}.in", long)},
            {"from": format!("{}.true", long), "to": format!("a{}_3c6e0fb7.in", "b".repeat(53))},
            {"from": format!("{}.false", long), "to": format!("a{}_08eae15a.in", "b".repeat(53))},
        ],
    }));
    assert_round_trip(&source, &expected);
}

#[test]
fn other_ir_versions_are_written_out() {
    let mut old = routing_artifact();
    old.ir_version = 0;
    let printed = print_default(&old);
    assert!(printed.starts_with("policy {\n  ir_version 0\n"), "{}", printed);
    assert_eq!(parse_ok(&printed).artifact, old);

    // The current version may be written, and is left out when printing.
    let explicit = routing_source().replace("policy {\n", "policy {\n  ir_version 1\n");
    let parsed = parse_ok(&explicit);
    assert_eq!(parsed.artifact, routing_artifact());
    assert_eq!(print_default(&parsed.artifact), routing_source());
}

#[test]
fn invalid_artifacts_print_and_parse_back_or_report_why() {
    // Representable although invalid: a `tee` without a port count, and a split with an extra parameter, whose ports
    // are then not known and are written in order of name.
    let representable = raw_artifact(json!({
        "ir_version": 1,
        "nodes": [
            {"id": "e", "op": "tee"},
            {"id": "s", "op": "split", "params": {"predicate": {"const": true}, "extra": 1}},
            {"id": "s_false", "op": "drop"},
            {"id": "s_true", "op": "drop"},
            {"id": "d", "op": "drop"},
        ],
        "edges": [
            {"from": "input.metrics", "to": "e.in"},
            {"from": "e.0", "to": "s.in"},
            {"from": "s.false", "to": "s_false.in"},
            {"from": "s.true", "to": "s_true.in"},
            {"from": "e.1", "to": "d.in"},
        ],
    }));
    let printed = print_default(&representable);
    assert!(printed.contains("let e = metrics | tee\n"), "{}", printed);
    assert!(printed.contains("let s = e.0 | split(true, extra: 1)\n"), "{}", printed);
    assert!(printed.contains("\n  s.false | drop\n"), "{}", printed);
    assert_eq!(parse_ok(&printed).artifact, representable);

    // Not representable: an edge back to a source, which text cannot write, and an edge to a node the artifact
    // does not have. They print as streams that go nowhere, and parsing the text reports it.
    let back = raw_artifact(json!({
        "ir_version": 1,
        "nodes": [{"id": "t", "op": "tee", "params": {"ports": 2}}, {"id": "d", "op": "drop"}],
        "edges": [
            {"from": "input.metrics", "to": "t.in"},
            {"from": "t.0", "to": "input.metrics"},
            {"from": "t.1", "to": "d.in"},
        ],
    }));
    let dangling = raw_artifact(json!({
        "ir_version": 1,
        "nodes": [],
        "edges": [{"from": "input.metrics", "to": "ghost.in"}, {"from": "phantom.out", "to": "ghost.in"}],
    }));
    for artifact in [back, dangling] {
        let printed = print_default(&artifact);
        let messages = parse_err(&printed).into_iter().map(|d| d.message).collect::<Vec<_>>();
        assert!(
            messages
                .iter()
                .any(|message| message.starts_with("This pipeline does not end")),
            "{:?} for\n{}",
            messages,
            printed
        );
    }
}

#[test]
fn comments_and_whitespace_do_not_matter() {
    let spaced = routing_source()
        .replace('\n', "\n\n   # a comment line\n")
        .replace(" | ", " |\t");
    assert_eq!(parse_ok(&spaced).artifact, routing_artifact());
}

#[test]
fn emits_to_recipients_name_them_bare_or_quoted_and_print_back() {
    let source = r#"policy {
  metrics | emit [primary, "https://127.0.0.1:17102"]

  metrics | emit output dsd_client_telemetry
}
"#
    .to_string();
    let expected = artifact(json!({
        "ir_version": 1,
        "nodes": [
            {"id": "input_metrics_fanout", "op": "tee", "params": {"ports": 2}},
            {"id": "input_metrics_fanout_0", "op": "emit", "params": {"to": ["primary", "https://127.0.0.1:17102"]}},
            {"id": "input_metrics_fanout_1", "op": "emit", "params": {"output": "dsd_client_telemetry"}},
        ],
        "edges": [
            {"from": "input.metrics", "to": "input_metrics_fanout.in"},
            {"from": "input_metrics_fanout.0", "to": "input_metrics_fanout_0.in"},
            {"from": "input_metrics_fanout.1", "to": "input_metrics_fanout_1.in"},
        ],
    }));
    assert_round_trip(&source, &expected);

    // An output may be called `to`.
    let parsed = parse_ok(&policy("  metrics | emit output to\n"));
    assert_eq!(parsed.artifact.nodes[0].params.get("output"), Some(&json!("to")));

    // A name that is neither an identifier nor a string is refused where it is written.
    let errors = parse_err(&policy("  metrics | emit [1]\n"));
    assert!(errors[0].message.contains("a recipient name"), "{:?}", errors);
}
