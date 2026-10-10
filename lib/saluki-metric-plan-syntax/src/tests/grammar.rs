//! Every grammar form, parsed against hand-written artifacts and printed against hand-written text.

use saluki_metric_plan::{
    ir::{Blobs, PolicyArtifact},
    operators::{ConfigureError, OperatorRegistry},
};
use serde_json::{json, Value};

use super::*;

const FP2: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
const FP3: &str = "sha256:3333333333333333333333333333333333333333333333333333333333333333";

/// Returns a blob table document holding one blob.
fn blob_table(hash: &str, blob: Value) -> Value {
    let mut table = serde_json::Map::new();
    table.insert(hash.to_string(), blob);
    Value::Object(table)
}

/// The routing example: a `tee` and a `split` written with their sugar, and inline terminals.
fn routing_source() -> String {
    format!(
        r#"policy post_enrich {{
  base "{BASE}"
  outputs {{
    dd_metrics_encode = "{FP2}",
    metrics_routing_encode_0 = "{FP3}",
  }}

  input.metrics -> metrics_policy

  node metrics_policy = tee {{
    -> emit dd_metrics_encode
    -> metrics_routing_filter_0
  }}

  node metrics_routing_filter_0 = split(intake_name prefix_in ["demo.checkout."]) {{
    true  -> emit metrics_routing_encode_0
    false -> drop
  }}
}}
"#
    )
}

fn routing_artifact() -> PolicyArtifact {
    artifact(json!({
        "ir_version": 1,
        "scope": "post_enrich",
        "base": {
            "program_hash": BASE,
            "outputs": {"dd_metrics_encode": FP2, "metrics_routing_encode_0": FP3},
        },
        "nodes": [
            {"id": "metrics_policy", "op": "tee", "params": {"ports": 2}},
            {"id": "metrics_policy_0", "op": "emit", "params": {"output": "dd_metrics_encode"}},
            {"id": "metrics_routing_filter_0", "op": "split",
             "params": {"predicate": {"intake_name": {"prefix_in": ["demo.checkout."]}}}},
            {"id": "metrics_routing_filter_0_true", "op": "emit", "params": {"output": "metrics_routing_encode_0"}},
            {"id": "metrics_routing_filter_0_false", "op": "drop"},
        ],
        "edges": [
            {"from": "input.metrics", "to": "metrics_policy.in"},
            {"from": "metrics_policy.0", "to": "metrics_policy_0.in"},
            {"from": "metrics_policy.1", "to": "metrics_routing_filter_0.in"},
            {"from": "metrics_routing_filter_0.true", "to": "metrics_routing_filter_0_true.in"},
            {"from": "metrics_routing_filter_0.false", "to": "metrics_routing_filter_0_false.in"},
        ],
    }))
}

#[test]
fn a_policy_needs_only_its_base() {
    let source = format!("policy post_enrich {{\n  base \"{}\"\n}}\n", BASE);
    let expected = artifact(json!({
        "ir_version": 1,
        "scope": "post_enrich",
        "base": {"program_hash": BASE},
        "nodes": [],
        "edges": [],
    }));
    assert_round_trip(&source, &expected);
}

#[test]
fn sugar_and_inline_terminals_build_the_routing_example() {
    assert_round_trip(&routing_source(), &routing_artifact());
}

#[test]
fn general_forms_in_any_order_build_the_same_artifact_as_the_sugar() {
    let source = format!(
        r#"policy post_enrich {{
  # The routing example in the general forms, in another order, on fewer lines.
  node metrics_routing_filter_0 = split(predicate: intake_name prefix_in ["demo.checkout."]) {{
    false -> metrics_routing_filter_0_false.in
    true -> metrics_routing_filter_0_true
  }}
  base "{BASE}" outputs {{ metrics_routing_encode_0 = "{FP3}", dd_metrics_encode = "{FP2}", }}
  node metrics_routing_filter_0_false = drop()
  node metrics_routing_filter_0_true = emit(output: "metrics_routing_encode_0")
  node metrics_policy_0 = emit(output: "dd_metrics_encode") node metrics_policy = tee(ports: 2) {{
    1 -> metrics_routing_filter_0 0 -> metrics_policy_0
  }}
  input.metrics -> metrics_policy.in
}}"#
    );
    let parsed = parse_ok(&source);
    assert_eq!(parsed.artifact, routing_artifact());
    // Terminals named by the derived-ID rule print inline, and the sugar is used where it applies.
    assert_eq!(print_default(&parsed.artifact), routing_source());
}

#[test]
fn spans_lead_back_to_nodes_parameters_and_edges() {
    let source = routing_source();
    let spans = parse_ok(&source).spans;

    let node = spans.node("metrics_policy").unwrap();
    assert!(text(&source, node).starts_with("node metrics_policy = tee {"));
    assert!(text(&source, node).ends_with("-> metrics_routing_filter_0\n  }"));
    assert_eq!(
        text(&source, spans.node("metrics_policy_0").unwrap()),
        "emit dd_metrics_encode"
    );
    assert_eq!(
        text(&source, spans.node("metrics_routing_filter_0_false").unwrap()),
        "drop"
    );
    assert_eq!(spans.node("missing"), None);

    // A `tee` without parameters takes its port count from its block.
    assert!(text(&source, spans.param("metrics_policy", "ports").unwrap()).starts_with("{\n    -> emit"));
    assert_eq!(
        text(&source, spans.param("metrics_routing_filter_0", "predicate").unwrap()),
        r#"intake_name prefix_in ["demo.checkout."]"#
    );
    // Paths inside a predicate fall back to the predicate.
    assert_eq!(
        spans.param("metrics_routing_filter_0", "predicate.intake_name"),
        spans.param("metrics_routing_filter_0", "predicate")
    );
    assert_eq!(
        text(&source, spans.param("metrics_policy_0", "output").unwrap()),
        "dd_metrics_encode"
    );

    // Edges in the artifact's order.
    let edges = (0..5)
        .map(|i| text(&source, spans.edge(i).unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(
        edges,
        [
            "input.metrics -> metrics_policy",
            "-> emit dd_metrics_encode",
            "-> metrics_routing_filter_0",
            "true  -> emit metrics_routing_encode_0",
            "false -> drop",
        ]
    );
    assert_eq!(spans.edge(5), None);
}

#[test]
fn spans_map_parameter_errors_back_to_the_source() {
    let source = policy(
        r#"  input.metrics -> scrub

  node scrub = tag_projection(
    rules: [{ metric_name: "a", action: "drop_it", tags: [] }],
    value_rules: [],
    cache_capacity: -1,
  ) {
    -> drop
  }"#,
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
        r#"policy post_enrich {{
  base "{BASE}"

  input.metrics -> scrub

  node scrub = tag_projection(
    rules: [{{ metric_name: "a.b", action: "exclude", tags: ["user_id"] }}],
    value_rules: [],
    cache_capacity: 1000,
  ) {{
    -> stamp
  }}

  node stamp = host_tags(tags: {tags}, deadline: 1700000300s) {{
    -> agg
  }}

  node agg = aggregate(
    window: 10s,
    flush_interval: config("dogstatsd.aggregation.flush_interval"),
    context_limit: -5,
    counter_expiry: "1 minute",
    flush_open_windows: false,
    histogram_aggregates: ["max", "median"],
    histogram_percentiles: [95, 99],
    copy_to_distribution: true,
    copy_to_distribution_prefix: "",
  ) {{
    -> odd
  }}

  node odd = frobnicate(
    big: 18446744073709551615,
    nothing: null,
    ratio: 1.5,
    record: {{ nested: {{ deeper: [true, false] }}, plain: [], "weird key": 1 }},
    shared: blob("{hash}"),
    small: -9223372036854775808,
    span: "500ms",
  ) {{
    -> drop
  }}

  blobs {{
    "{hash}" = ["a", "b"],
  }}
}}
"#
    );
    let expected = artifact(json!({
        "ir_version": 1,
        "scope": "post_enrich",
        "base": {"program_hash": BASE},
        "nodes": [
            {"id": "scrub", "op": "tag_projection", "params": {
                "rules": [{"metric_name": "a.b", "action": "exclude", "tags": ["user_id"]}],
                "value_rules": [],
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
                "flush_open_windows": false,
                "histogram_aggregates": ["max", "median"],
                "histogram_percentiles": [95, 99],
                "copy_to_distribution": true,
                "copy_to_distribution_prefix": "",
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
        ("config(\"a.b_c.0\")", json!({"config": "a.b_c.0"})),
        // Members that are reserved in parameters are written as they are; validation rejects them.
        ("{ config: \"Not A Binding\" }", json!({"config": "Not A Binding"})),
        ("\"\"", json!("")),
    ];
    for (written, expected) in cases {
        let source = policy(&format!("  node n = frobnicate(v: {})", written));
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
    let compact = format!(
        concat!(
            r#"policy post_enrich {{ base "{}" input.metrics -> ts node ts = timestamp_split(window: 10s) {{ "#,
            r#"default -> scrub passthrough -> emit dd_metrics_encode }} node scrub = tag_projection(rules: [{{ "#,
            r#"metric_name: "checkout.requests.latency", action: "exclude", tags: ["user_id", "session_id", "#,
            r#""request_id"] }}, {{ tags: ["env", "service", "version", "region", "availability_zone", "#,
            r#""cluster_name", "team"], action: "include", metric_name: "checkout.requests.count" }}], value_rules: "#,
            r#"config("dogstatsd.tag_value_allowlist"), cache_capacity: 100000) {{ -> gateway }} node gateway = "#,
            r#"split(flag(config("multi_region_failover.enabled")) and (is_empty(config("#,
            r#""multi_region_failover.metric_mirroring.allowlist")) or name matches(values: config("#,
            r#""multi_region_failover.metric_mirroring.allowlist"), prefix: false)) and not host in ["#,
            r#""web-01.example.com", "web-02.example.com", "web-03.example.com", "web-04.example.com"]) {{ "#,
            r#"true -> emit mrf_metrics_encode false -> routing }} node routing = split(intake_name in ["#,
            r#""checkout.requests.count", "checkout.requests.latency.p99", "checkout.errors.count", "#,
            r#""checkout.cart.items.added", "checkout.cart.items.removed"]) {{ true -> emit "#,
            r#"metrics_routing_encode_0 false -> drop }} }}"#
        ),
        BASE
    );
    let expected = format!(
        r#"policy post_enrich {{
  base "{BASE}"

  input.metrics -> ts

  node ts = timestamp_split(window: 10s) {{
    default     -> scrub
    passthrough -> emit dd_metrics_encode
  }}

  node scrub = tag_projection(
    rules: [
      {{ metric_name: "checkout.requests.latency", action: "exclude", tags: ["user_id", "session_id", "request_id"] }},
      {{
        metric_name: "checkout.requests.count",
        action: "include",
        tags: ["env", "service", "version", "region", "availability_zone", "cluster_name", "team"],
      }},
    ],
    value_rules: config("dogstatsd.tag_value_allowlist"),
    cache_capacity: 100000,
  ) {{
    -> gateway
  }}

  node gateway = split(
    flag(config("multi_region_failover.enabled"))
    and (
      is_empty(config("multi_region_failover.metric_mirroring.allowlist"))
      or name matches(values: config("multi_region_failover.metric_mirroring.allowlist"), prefix: false)
    )
    and not host in ["web-01.example.com", "web-02.example.com", "web-03.example.com", "web-04.example.com"]
  ) {{
    true  -> emit mrf_metrics_encode
    false -> routing
  }}

  node routing = split(
    intake_name in [
      "checkout.requests.count",
      "checkout.requests.latency.p99",
      "checkout.errors.count",
      "checkout.cart.items.added",
      "checkout.cart.items.removed",
    ]
  ) {{
    true  -> emit metrics_routing_encode_0
    false -> drop
  }}
}}
"#
    );
    let printed = print_default(&parse_ok(&compact).artifact);
    assert_eq!(printed, expected);
    assert!(printed.lines().all(|line| line.chars().count() <= 120));

    // A narrower width breaks more, and still parses back to the same artifact.
    let narrow = print(
        &parse_ok(&compact).artifact,
        &PrintOptions {
            width: 40,
            ..PrintOptions::default()
        },
    );
    assert!(narrow.lines().count() > expected.lines().count());
    assert_eq!(parse_ok(&narrow).artifact, parse_ok(&compact).artifact);
}

#[test]
fn nodes_are_ordered_by_the_graph_not_by_the_text() {
    let forward = format!(
        r#"policy post_enrich {{
  base "{BASE}"

  input.metrics -> fork

  node fork = split(has_tag("env")) {{
    true  -> tagged
    false -> joined.1
  }}

  node tagged = host_enrichment {{
    -> joined.0
  }}

  node joined = merge(inputs: 2) {{
    -> emit dd_metrics_encode
  }}
}}
"#
    );
    let shuffled = format!(
        r#"policy post_enrich {{
  node joined = merge(inputs: 2) {{ out -> emit dd_metrics_encode }}
  base "{BASE}"
  node tagged = host_enrichment() {{ out -> joined.0 }}
  fork.false -> joined.1
  node fork = split(predicate: has_tag("env")) {{ true -> tagged.in }}
  input.metrics -> fork
}}"#
    );
    let expected = artifact(json!({
        "ir_version": 1,
        "scope": "post_enrich",
        "base": {"program_hash": BASE},
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
    let parsed = parse_ok(&shuffled);
    assert_eq!(parsed.artifact, expected);
    assert_eq!(print_default(&parsed.artifact), forward);
}

#[test]
fn keywords_can_be_node_ids_and_terminals_can_be_named() {
    let source = format!(
        r#"policy post_enrich {{
  base "{BASE}"

  input.metrics -> fork

  node fork = split(true) {{
    true  -> drop.in
    false -> sink
  }}

  node drop = host_enrichment {{
    -> emit dd_metrics_encode
  }}

  node sink = drop
}}
"#
    );
    let expected = artifact(json!({
        "ir_version": 1,
        "scope": "post_enrich",
        "base": {"program_hash": BASE},
        "nodes": [
            {"id": "fork", "op": "split", "params": {"predicate": {"const": true}}},
            {"id": "drop", "op": "host_enrichment"},
            {"id": "drop_out", "op": "emit", "params": {"output": "dd_metrics_encode"}},
            // Not named by the derived-ID rule, so it is not printed inline.
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
        r#"policy post_enrich {{
  base "{BASE}"

  input.first -> drop
  input.second -> {long}

  node {long} = split(false) {{
    true  -> emit dd_metrics_encode
    false -> drop
  }}
}}
"#
    );
    let expected = artifact(json!({
        "ir_version": 1,
        "scope": "post_enrich",
        "base": {"program_hash": BASE},
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
fn elided_fingerprints_print_without_outputs() {
    let printed = print(
        &routing_artifact(),
        &PrintOptions {
            elide_fingerprints: true,
            ..PrintOptions::default()
        },
    );
    let expected = routing_source().replace(
        &format!(
            "\n  outputs {{\n    dd_metrics_encode = \"{}\",\n    metrics_routing_encode_0 = \"{}\",\n  }}",
            FP2, FP3
        ),
        "",
    );
    assert_eq!(printed, expected);
    assert!(!printed.contains("outputs"));

    let mut without = routing_artifact();
    without.base.outputs.clear();
    assert_eq!(parse_ok(&printed).artifact, without);
}

#[test]
fn other_ir_versions_are_written_out() {
    let mut old = routing_artifact();
    old.ir_version = 0;
    let printed = print_default(&old);
    assert!(
        printed.starts_with("policy post_enrich {\n  ir_version 0\n  base"),
        "{}",
        printed
    );
    assert_eq!(parse_ok(&printed).artifact, old);

    // The current version may be written, and is left out when printing.
    let explicit = routing_source().replace("  base", "  ir_version 1\n  base");
    let parsed = parse_ok(&explicit);
    assert_eq!(parsed.artifact, routing_artifact());
    assert_eq!(print_default(&parsed.artifact), routing_source());
}

#[test]
fn invalid_artifacts_print_and_parse_back_or_report_why() {
    // Representable although invalid: an edge back to a scope input, a tee whose port count disagrees with its
    // edges, an empty tee, and a split with an extra parameter.
    let representable = raw_artifact(json!({
        "ir_version": 1,
        "scope": "post_enrich",
        "base": {"program_hash": BASE},
        "nodes": [
            {"id": "t", "op": "tee", "params": {"ports": 4}},
            {"id": "e", "op": "tee"},
            {"id": "s", "op": "split", "params": {"predicate": {"const": true}, "extra": 1}},
        ],
        "edges": [
            {"from": "input.metrics", "to": "t.in"},
            {"from": "t.0", "to": "input.metrics"},
            {"from": "t.1", "to": "e.in"},
            {"from": "t.2", "to": "s.in"},
        ],
    }));
    let printed = print_default(&representable);
    assert!(
        printed.contains("node t = tee(ports: 4) {\n    0 -> input.metrics"),
        "{}",
        printed
    );
    assert!(printed.contains("node e = tee()"), "{}", printed);
    assert!(
        printed.contains("node s = split(predicate: true, extra: 1)"),
        "{}",
        printed
    );
    assert_eq!(parse_ok(&printed).artifact, representable);

    // Not representable: an edge to a node the artifact does not have prints, and parsing it reports the node.
    let dangling = raw_artifact(json!({
        "ir_version": 1,
        "scope": "post_enrich",
        "base": {"program_hash": BASE},
        "nodes": [],
        "edges": [{"from": "input.metrics", "to": "ghost.in"}, {"from": "phantom.out", "to": "ghost.in"}],
    }));
    let printed = print_default(&dangling);
    assert!(printed.contains("  input.metrics -> ghost\n"), "{}", printed);
    assert!(printed.contains("  phantom.out -> ghost\n"), "{}", printed);
    let messages = parse_err(&printed).into_iter().map(|d| d.message).collect::<Vec<_>>();
    assert_eq!(
        messages,
        [
            "There is no node `ghost`; define it with `node ghost = ...`.",
            "There is no node `phantom`; define it with `node phantom = ...`.",
            "There is no node `ghost`; define it with `node ghost = ...`.",
        ]
    );
}

#[test]
fn edges_leave_a_scope_that_ends_before_its_paths_do_by_its_exits() {
    // A scope with exits, as the DogStatsD pre-aggregation scope is: an edge into an exit is written as its target,
    // in a node's block or as a statement of its own, and never makes a node.
    let source = format!(
        r#"policy dogstatsd_pre_aggregation {{
  base "{BASE}"

  input.metrics -> blocklist

  node blocklist = split(name starts_with "blocked.") {{
    true  -> drop
    false -> exit.aggregated
  }}
}}
"#
    );
    let expected = artifact(json!({
        "ir_version": 1,
        "scope": "dogstatsd_pre_aggregation",
        "base": {"program_hash": BASE},
        "nodes": [
            {"id": "blocklist", "op": "split", "params": {"predicate": {"name": {"prefix": "blocked."}}}},
            {"id": "blocklist_true", "op": "drop"},
        ],
        "edges": [
            {"from": "input.metrics", "to": "blocklist.in"},
            {"from": "blocklist.true", "to": "blocklist_true.in"},
            {"from": "blocklist.false", "to": "exit.aggregated"},
        ],
    }));
    assert_round_trip(&source, &expected);
    let parsed = parse_ok(&source);
    assert_eq!(text(&source, parsed.spans.edge(2).unwrap()), "false -> exit.aggregated");

    // A scope input may lead straight to an exit, and an edge statement may name one.
    let source = format!(
        r#"policy dogstatsd_pre_aggregation {{
  base "{BASE}"

  input.metrics -> exit.passthrough
}}
"#
    );
    let expected = artifact(json!({
        "ir_version": 1,
        "scope": "dogstatsd_pre_aggregation",
        "base": {"program_hash": BASE},
        "nodes": [],
        "edges": [{"from": "input.metrics", "to": "exit.passthrough"}],
    }));
    assert_round_trip(&source, &expected);
    let statement = source.replace(
        "input.metrics -> exit.passthrough",
        "input.metrics -> keep\n\n  node keep = tee(ports: 1)\n\n  keep.0 -> exit.passthrough",
    );
    let parsed = parse_ok(&statement);
    assert_eq!(
        serde_json::to_value(&parsed.artifact.edges).unwrap(),
        json!([
            {"from": "input.metrics", "to": "keep.in"},
            {"from": "keep.0", "to": "exit.passthrough"},
        ])
    );
}

#[test]
fn comments_and_whitespace_do_not_matter() {
    let spaced = routing_source()
        .replace('\n', "\n\n   # a comment line\n")
        .replace(" -> ", " ->\t");
    assert_eq!(parse_ok(&spaced).artifact, routing_artifact());
}
