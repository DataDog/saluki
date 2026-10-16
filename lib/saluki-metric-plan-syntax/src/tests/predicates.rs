//! Every predicate leaf and composition, against hand-written JSON.

use saluki_metric_plan::predicate::Predicate;
use serde_json::{json, Value};

use super::*;

/// Returns a policy whose only node is a `where` with `predicate`, named `f`, in printed form.
fn split_source(predicate: &str) -> String {
    policy(&format!("  metrics | where {} as f | drop", predicate))
}

/// Returns a policy whose only node is a `split` with `predicate`, named `f`, in printed form, for a predicate the
/// plan crate does not read.
///
/// Such a split does not configure, so its ports are unknown and are written in order of name, the core form with
/// each port used; a stage that does not fit the line is moved to a line of its own.
fn raw_split_source(predicate: &str) -> String {
    let stage = format!("split({})", predicate);
    let head = if "  let f = metrics | ".len() + stage.len() > 120 {
        format!("  let f = metrics\n    | {}", stage)
    } else {
        format!("  let f = metrics | {}", stage)
    };
    policy(&format!("{}\n\n  f.false | drop\n\n  f.true | drop", head))
}

/// Parses a `where` with `predicate`, and returns its predicate parameter.
#[track_caller]
fn predicate_json(predicate: &str) -> Value {
    let parsed = parse_ok(&split_source(predicate));
    parsed.artifact.nodes[0]
        .params
        .get("predicate")
        .cloned()
        .expect("the split has a predicate")
}

const ZERO_HASH: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

#[test]
fn every_leaf_and_composition_has_its_json_and_prints_back() {
    let blob_leaf = format!("{{ name: {{ in: blob(\"{}\") }} }}", ZERO_HASH);
    // (predicate in printed form, its JSON, whether the plan crate reads it as a predicate)
    let cases: Vec<(&str, Value, bool)> = vec![
        // Name, intake name, and host, with every string match.
        (r#"name == "requests""#, json!({"name": {"eq": "requests"}}), true),
        (
            r#"intake_name starts_with "demo.""#,
            json!({"intake_name": {"prefix": "demo."}}),
            true,
        ),
        (r#"host in ["a", "b"]"#, json!({"host": {"in": ["a", "b"]}}), true),
        (
            r#"name starts_with ["x.", "y."]"#,
            json!({"name": {"prefix_in": ["x.", "y."]}}),
            true,
        ),
        (r#"name in []"#, json!({"name": {"in": []}}), true),
        (
            r#"intake_name matches(values: config.f.values, prefix: config.f.prefix)"#,
            json!({"intake_name": {"matches": {"values": {"config": "f.values"}, "prefix": {"config": "f.prefix"}}}}),
            true,
        ),
        (
            r#"host matches(values: ["a"], prefix: true)"#,
            json!({"host": {"matches": {"values": ["a"], "prefix": true}}}),
            true,
        ),
        ("host is unset", json!({"host_unset": {}}), true),
        ("host is set", json!({"not": {"host_unset": {}}}), true),
        // The negated forms, which are `not` of the leaf, and the bound sets of names and prefixes.
        (
            r#"name != "requests""#,
            json!({"not": {"name": {"eq": "requests"}}}),
            true,
        ),
        (r#"host != "a""#, json!({"not": {"host": {"eq": "a"}}}), true),
        (
            "intake_name in config.dogstatsd.metric_filter.values",
            json!({"intake_name": {"matches": {"values": {"config": "dogstatsd.metric_filter.values"}, "prefix": false}}}),
            true,
        ),
        (
            "name starts_with config.x",
            json!({"name": {"matches": {"values": {"config": "x"}, "prefix": true}}}),
            true,
        ),
        // Tags and origin tags.
        ("tags.env is set", json!({"has_tag": "env"}), true),
        ("tags.env is unset", json!({"not": {"has_tag": "env"}}), true),
        // A key that is not an identifier is quoted.
        (
            r#"tags["app.kubernetes.io/name"] is set"#,
            json!({"has_tag": "app.kubernetes.io/name"}),
            true,
        ),
        (r#"tags["1st"] is unset"#, json!({"not": {"has_tag": "1st"}}), true),
        (
            r#"tags.team == "platform""#,
            json!({"tag": {"key": "team", "value": {"eq": "platform"}}}),
            true,
        ),
        (
            r#"tags.team starts_with "plat""#,
            json!({"tag": {"key": "team", "value": {"prefix": "plat"}}}),
            true,
        ),
        (
            r#"tags.team in ["a", "b"]"#,
            json!({"tag": {"key": "team", "value": {"in": ["a", "b"]}}}),
            true,
        ),
        (
            r#"tags.team starts_with ["a"]"#,
            json!({"tag": {"key": "team", "value": {"prefix_in": ["a"]}}}),
            true,
        ),
        (
            r#"tags.team in config.t.values"#,
            json!({"tag": {"key": "team", "value": {"matches": {"values": {"config": "t.values"}, "prefix": false}}}}),
            true,
        ),
        (
            r#"tags.team starts_with config.t.values"#,
            json!({"tag": {"key": "team", "value": {"matches": {"values": {"config": "t.values"}, "prefix": true}}}}),
            true,
        ),
        (
            r#"tags.team matches(values: ["a"], prefix: false)"#,
            json!({"tag": {"key": "team", "value": {"matches": {"values": ["a"], "prefix": false}}}}),
            true,
        ),
        ("origin_tags.pod is set", json!({"has_origin_tag": "pod"}), true),
        (
            r#"origin_tags.pod starts_with "web-""#,
            json!({"origin_tag": {"key": "pod", "value": {"prefix": "web-"}}}),
            true,
        ),
        (
            r#"origin_tags.pod == "web-1""#,
            json!({"origin_tag": {"key": "pod", "value": {"eq": "web-1"}}}),
            true,
        ),
        // Kinds.
        (
            "kind in [counter, gauge]",
            json!({"kind_in": ["counter", "gauge"]}),
            true,
        ),
        (
            "kind in [counter, rate, gauge, set, histogram, distribution]",
            json!({"kind_in": "all"}),
            true,
        ),
        ("kind in []", json!({"kind_in": []}), true),
        // Constants, flags, and empty lists.
        ("true", json!({"const": true}), true),
        ("false", json!({"const": false}), true),
        ("{ flag: true }", json!({"flag": true}), true),
        ("config.mrf.enabled", json!({"flag": {"config": "mrf.enabled"}}), true),
        ("is_empty([])", json!({"is_empty": []}), true),
        (r#"is_empty(["a"])"#, json!({"is_empty": ["a"]}), true),
        (
            "is_empty(config.mrf.allowlist)",
            json!({"is_empty": {"config": "mrf.allowlist"}}),
            true,
        ),
        // Strings with escapes.
        (
            r#"name == "a\"b\\c\n\u{202e}""#,
            json!({"name": {"eq": "a\"b\\c\n\u{202E}"}}),
            true,
        ),
        // Composition.
        (
            r#"tags.a != "x""#,
            json!({"not": {"tag": {"key": "a", "value": {"eq": "x"}}}}),
            true,
        ),
        ("not not true", json!({"not": {"not": {"const": true}}}), true),
        (
            r#"tags.a is set and tags.b is set and tags.c is set"#,
            json!({"all": [{"has_tag": "a"}, {"has_tag": "b"}, {"has_tag": "c"}]}),
            true,
        ),
        (
            r#"tags.a is set or tags.b is set"#,
            json!({"any": [{"has_tag": "a"}, {"has_tag": "b"}]}),
            true,
        ),
        (
            r#"(tags.a is set or tags.b is set) and tags.c is set"#,
            json!({"all": [{"any": [{"has_tag": "a"}, {"has_tag": "b"}]}, {"has_tag": "c"}]}),
            true,
        ),
        // An `and` under an `or` keeps its parentheses, for the reader.
        (
            r#"(tags.a is set and tags.b is set) or tags.c is set"#,
            json!({"any": [{"all": [{"has_tag": "a"}, {"has_tag": "b"}]}, {"has_tag": "c"}]}),
            true,
        ),
        // Parentheses keep a nested junction of the same kind.
        (
            r#"tags.a is set and (tags.b is set and tags.c is set)"#,
            json!({"all": [{"has_tag": "a"}, {"all": [{"has_tag": "b"}, {"has_tag": "c"}]}]}),
            true,
        ),
        (
            r#"not (tags.a is set or tags.b is set)"#,
            json!({"not": {"any": [{"has_tag": "a"}, {"has_tag": "b"}]}}),
            true,
        ),
        // Junctions of fewer than two operands have no syntax of their own and are written raw.
        ("{ all: [] }", json!({"all": []}), true),
        ("{ any: [] }", json!({"any": []}), true),
        (
            r#"{ all: [{ has_tag: "a" }] }"#,
            json!({"all": [{"has_tag": "a"}]}),
            true,
        ),
        (
            "{ any: [{ not: { const: true } }] }",
            json!({"any": [{"not": {"const": true}}]}),
            true,
        ),
        (
            r#"not { all: [{ has_tag: "a" }] } and { any: [] }"#,
            json!({"all": [{"not": {"all": [{"has_tag": "a"}]}}, {"any": []}]}),
            true,
        ),
        // Values the leaf syntax does not write are written raw: a blob reference inside a leaf, an unknown leaf, and
        // kinds out of their declared order.
        (blob_leaf.as_str(), json!({"name": {"in": {"blob": ZERO_HASH}}}), false),
        (r#"{ name: { equals: "x" } }"#, json!({"name": {"equals": "x"}}), false),
        (
            r#"{ kind_in: ["gauge", "counter"] }"#,
            json!({"kind_in": ["gauge", "counter"]}),
            true,
        ),
        (
            r#"tags.a is set and { weird: 1 }"#,
            json!({"all": [{"has_tag": "a"}, {"weird": 1}]}),
            false,
        ),
        (r#"not "text""#, json!({"not": "text"}), false),
        // Where a predicate is expected, `true` and `false` are constant leaves, so a junction with a bare boolean as
        // an operand is written raw, which keeps the boolean.
        ("{ all: [true] }", json!({"all": [true]}), false),
        ("{ not: false }", json!({"not": false}), false),
        (
            r#"{ any: [{ has_tag: "a" }, true] }"#,
            json!({"any": [{"has_tag": "a"}, true]}),
            false,
        ),
        (
            r#"tags.b is set and { all: [true] }"#,
            json!({"all": [{"has_tag": "b"}, {"all": [true]}]}),
            false,
        ),
    ];
    for (written, expected, is_predicate) in cases {
        assert_eq!(predicate_json(written), expected, "{}", written);
        assert_eq!(
            serde_json::from_value::<Predicate>(expected.clone()).is_ok(),
            is_predicate,
            "{}",
            written
        );
        // A predicate the plan crate reads prints as `where`.
        let source = if is_predicate {
            split_source(written)
        } else {
            raw_split_source(written)
        };
        assert_eq!(print_default(&parse_ok(&source).artifact), source, "{}", written);
    }
}

#[test]
fn other_spellings_of_predicates_have_the_same_json() {
    // (predicate as written, its JSON, its printed form)
    let cases: Vec<(&str, Value, &str)> = vec![
        (r#"name=="x""#, json!({"name": {"eq": "x"}}), r#"name == "x""#),
        // `!=` and `not` of an equality are the same predicate, printed as `!=`; likewise `is unset` and `not` of
        // `is set`, `is set` and `not` of `is unset`.
        (
            r#"not name == "a""#,
            json!({"not": {"name": {"eq": "a"}}}),
            r#"name != "a""#,
        ),
        (
            r#"not (intake_name == "a")"#,
            json!({"not": {"intake_name": {"eq": "a"}}}),
            r#"intake_name != "a""#,
        ),
        (
            r#"not tags.env == "prod""#,
            json!({"not": {"tag": {"key": "env", "value": {"eq": "prod"}}}}),
            r#"tags.env != "prod""#,
        ),
        ("not tags.a is set", json!({"not": {"has_tag": "a"}}), "tags.a is unset"),
        (
            "not tags.a is unset",
            json!({"not": {"not": {"has_tag": "a"}}}),
            "not tags.a is unset",
        ),
        ("not host is unset", json!({"not": {"host_unset": {}}}), "host is set"),
        (
            "not host is set",
            json!({"not": {"not": {"host_unset": {}}}}),
            "not host is set",
        ),
        // A key that is an identifier may be quoted, and a quoted one is printed bare.
        (r#"tags["env"] is set"#, json!({"has_tag": "env"}), "tags.env is set"),
        // `and` binds tighter than `or`; printing writes the parentheses that say so.
        (
            r#"tags.a is set and tags.b is set or tags.c is set"#,
            json!({"any": [{"all": [{"has_tag": "a"}, {"has_tag": "b"}]}, {"has_tag": "c"}]}),
            r#"(tags.a is set and tags.b is set) or tags.c is set"#,
        ),
        (
            r#"tags.a is set or tags.b is set and tags.c is set"#,
            json!({"any": [{"has_tag": "a"}, {"all": [{"has_tag": "b"}, {"has_tag": "c"}]}]}),
            r#"tags.a is set or (tags.b is set and tags.c is set)"#,
        ),
        // `not` binds tighter than `and`.
        (
            r#"not tags.a is set and tags.b is set"#,
            json!({"all": [{"not": {"has_tag": "a"}}, {"has_tag": "b"}]}),
            r#"tags.a is unset and tags.b is set"#,
        ),
        (r#"((tags.a is set))"#, json!({"has_tag": "a"}), r#"tags.a is set"#),
        (
            r#"name matches(prefix: false, values: ["a"],)"#,
            json!({"name": {"matches": {"values": ["a"], "prefix": false}}}),
            r#"name matches(values: ["a"], prefix: false)"#,
        ),
        (
            "kind in [gauge, counter,]",
            json!({"kind_in": ["counter", "gauge"]}),
            "kind in [counter, gauge]",
        ),
        (r#"host in ["a",]"#, json!({"host": {"in": ["a"]}}), r#"host in ["a"]"#),
        (
            r#"split_me == "x""#,
            json!(null),
            "", // Not a predicate: see below.
        ),
    ];
    for (written, expected, printed) in cases {
        if written.starts_with("split_me") {
            // A word that starts no predicate is reported, not taken as a value.
            let diagnostics = parse_err(&split_source(written));
            assert!(
                diagnostics[0].message.starts_with("Expected a predicate"),
                "{}",
                diagnostics[0].message
            );
            continue;
        }
        assert_eq!(predicate_json(written), expected, "{}", written);
        let artifact = parse_ok(&split_source(written)).artifact;
        assert_eq!(print_default(&artifact), split_source(printed), "{}", written);
    }
}

#[test]
fn printing_a_raw_predicate_keeps_the_ports_it_was_parsed_with() {
    // Printed as a junction of `true`, this predicate once parsed back as `{"all": [{"const": true}]}`, which
    // configures the split, so the next print ordered its ports differently: the text changed on the second round.
    let source = policy("  let s = metrics | split({ all: [true] })\n\n  s.false | drop\n\n  s.true | drop");
    let parsed = parse_ok(&source);
    assert_eq!(
        parsed.artifact.nodes[0].params.get("predicate"),
        Some(&json!({"all": [true]}))
    );
    // The predicate does not configure the split, so its ports are unknown and are written in order of name.
    assert_eq!(print_default(&parsed.artifact), source);
}

#[test]
fn the_named_form_and_where_take_the_same_predicate_syntax() {
    let positional = predicate_json(r#"tags.a is set or host is unset"#);
    let named = parse_ok(&policy(
        "  let f = metrics | split(predicate: tags.a is set or host is unset)\n\n  f.true | drop\n\n  f.false | drop",
    ));
    assert_eq!(named.artifact.nodes[0].params.get("predicate"), Some(&positional));
}
