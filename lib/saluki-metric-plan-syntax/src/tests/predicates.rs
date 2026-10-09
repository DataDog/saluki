//! Every predicate leaf and composition, against hand-written JSON.

use saluki_metric_plan::predicate::Predicate;
use serde_json::{json, Value};

use super::*;

/// Returns a policy whose only node is a split with `predicate`, in printed form.
fn split_source(predicate: &str) -> String {
    policy(&format!("  node f = split({})", predicate))
}

/// Parses a split with `predicate`, and returns its predicate parameter.
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
            r#"name prefix_in ["x.", "y."]"#,
            json!({"name": {"prefix_in": ["x.", "y."]}}),
            true,
        ),
        (r#"name in []"#, json!({"name": {"in": []}}), true),
        (
            r#"intake_name matches(values: config("f.values"), prefix: config("f.prefix"))"#,
            json!({"intake_name": {"matches": {"values": {"config": "f.values"}, "prefix": {"config": "f.prefix"}}}}),
            true,
        ),
        (
            r#"host matches(values: ["a"], prefix: true)"#,
            json!({"host": {"matches": {"values": ["a"], "prefix": true}}}),
            true,
        ),
        ("host is unset", json!({"host_unset": {}}), true),
        // Tags and origin tags.
        (r#"has_tag("env")"#, json!({"has_tag": "env"}), true),
        (
            r#"tag("team") == "platform""#,
            json!({"tag": {"key": "team", "value": {"eq": "platform"}}}),
            true,
        ),
        (
            r#"tag("team") starts_with "plat""#,
            json!({"tag": {"key": "team", "value": {"prefix": "plat"}}}),
            true,
        ),
        (
            r#"tag("team") in ["a", "b"]"#,
            json!({"tag": {"key": "team", "value": {"in": ["a", "b"]}}}),
            true,
        ),
        (
            r#"tag("team") prefix_in ["a"]"#,
            json!({"tag": {"key": "team", "value": {"prefix_in": ["a"]}}}),
            true,
        ),
        (
            r#"tag("team") matches(values: config("t.values"), prefix: false)"#,
            json!({"tag": {"key": "team", "value": {"matches": {"values": {"config": "t.values"}, "prefix": false}}}}),
            true,
        ),
        (r#"has_origin_tag("pod")"#, json!({"has_origin_tag": "pod"}), true),
        (
            r#"origin_tag("pod") starts_with "web-""#,
            json!({"origin_tag": {"key": "pod", "value": {"prefix": "web-"}}}),
            true,
        ),
        (
            r#"origin_tag("pod") == "web-1""#,
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
        ("flag(true)", json!({"flag": true}), true),
        (
            r#"flag(config("mrf.enabled"))"#,
            json!({"flag": {"config": "mrf.enabled"}}),
            true,
        ),
        ("is_empty([])", json!({"is_empty": []}), true),
        (r#"is_empty(["a"])"#, json!({"is_empty": ["a"]}), true),
        (
            r#"is_empty(config("mrf.allowlist"))"#,
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
        (r#"not has_tag("a")"#, json!({"not": {"has_tag": "a"}}), true),
        ("not not true", json!({"not": {"not": {"const": true}}}), true),
        (
            r#"has_tag("a") and has_tag("b") and has_tag("c")"#,
            json!({"all": [{"has_tag": "a"}, {"has_tag": "b"}, {"has_tag": "c"}]}),
            true,
        ),
        (
            r#"has_tag("a") or has_tag("b")"#,
            json!({"any": [{"has_tag": "a"}, {"has_tag": "b"}]}),
            true,
        ),
        (
            r#"(has_tag("a") or has_tag("b")) and has_tag("c")"#,
            json!({"all": [{"any": [{"has_tag": "a"}, {"has_tag": "b"}]}, {"has_tag": "c"}]}),
            true,
        ),
        (
            r#"(has_tag("a") and has_tag("b")) or has_tag("c")"#,
            json!({"any": [{"all": [{"has_tag": "a"}, {"has_tag": "b"}]}, {"has_tag": "c"}]}),
            true,
        ),
        // Parentheses keep a nested junction of the same kind.
        (
            r#"has_tag("a") and (has_tag("b") and has_tag("c"))"#,
            json!({"all": [{"has_tag": "a"}, {"all": [{"has_tag": "b"}, {"has_tag": "c"}]}]}),
            true,
        ),
        (
            r#"not (has_tag("a") or has_tag("b"))"#,
            json!({"not": {"any": [{"has_tag": "a"}, {"has_tag": "b"}]}}),
            true,
        ),
        // Junctions of fewer than two operands.
        ("all()", json!({"all": []}), true),
        ("any()", json!({"any": []}), true),
        (r#"all(has_tag("a"))"#, json!({"all": [{"has_tag": "a"}]}), true),
        ("any(not true)", json!({"any": [{"not": {"const": true}}]}), true),
        (
            r#"not all(has_tag("a")) and any()"#,
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
            r#"has_tag("a") and { weird: 1 }"#,
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
            r#"has_tag("b") and { all: [true] }"#,
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
        let source = split_source(written);
        assert_eq!(print_default(&parse_ok(&source).artifact), source, "{}", written);
    }
}

#[test]
fn other_spellings_of_predicates_have_the_same_json() {
    // (predicate as written, its JSON, its printed form)
    let cases: Vec<(&str, Value, &str)> = vec![
        (r#"name=="x""#, json!({"name": {"eq": "x"}}), r#"name == "x""#),
        (
            r#"all(has_tag("a"), has_tag("b"))"#,
            json!({"all": [{"has_tag": "a"}, {"has_tag": "b"}]}),
            r#"has_tag("a") and has_tag("b")"#,
        ),
        (
            r#"any(has_tag("a"), has_tag("b"),)"#,
            json!({"any": [{"has_tag": "a"}, {"has_tag": "b"}]}),
            r#"has_tag("a") or has_tag("b")"#,
        ),
        // `and` binds tighter than `or`.
        (
            r#"has_tag("a") and has_tag("b") or has_tag("c")"#,
            json!({"any": [{"all": [{"has_tag": "a"}, {"has_tag": "b"}]}, {"has_tag": "c"}]}),
            r#"(has_tag("a") and has_tag("b")) or has_tag("c")"#,
        ),
        (
            r#"has_tag("a") or has_tag("b") and has_tag("c")"#,
            json!({"any": [{"has_tag": "a"}, {"all": [{"has_tag": "b"}, {"has_tag": "c"}]}]}),
            r#"has_tag("a") or (has_tag("b") and has_tag("c"))"#,
        ),
        // `not` binds tighter than `and`.
        (
            r#"not has_tag("a") and has_tag("b")"#,
            json!({"all": [{"not": {"has_tag": "a"}}, {"has_tag": "b"}]}),
            r#"not has_tag("a") and has_tag("b")"#,
        ),
        (r#"((has_tag("a")))"#, json!({"has_tag": "a"}), r#"has_tag("a")"#),
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
    // Printed as `all(true)`, this predicate once parsed back as `{"all": [{"const": true}]}`, which configures the
    // split, so the next print ordered its ports differently: the text changed on the second round.
    let source = policy(
        "  input.metrics -> s\n\n  node s = split({ all: [true] }) {\n    false -> drop\n    true  -> drop\n  }",
    );
    let parsed = parse_ok(&source);
    assert_eq!(
        parsed.artifact.nodes[0].params.get("predicate"),
        Some(&json!({"all": [true]}))
    );
    // The predicate does not configure the split, so its ports are unknown and are written in order of name.
    assert_eq!(print_default(&parsed.artifact), source);
}

#[test]
fn the_named_form_takes_the_same_predicate_syntax() {
    let positional = predicate_json(r#"has_tag("a") or host is unset"#);
    let named = parse_ok(&policy(r#"  node f = split(predicate: has_tag("a") or host is unset)"#));
    assert_eq!(named.artifact.nodes[0].params.get("predicate"), Some(&positional));
}
