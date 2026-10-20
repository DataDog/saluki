//! Diagnostics for common mistakes, and the bounds on what a source may contain.
//!
//! The passes' own tests cover each syntax error and each name and port rule in detail; these go through the public
//! `parse` and `print` and read the rendered diagnostics: where each is, what text it underlines, and what it says.

use saluki_metric_plan::limits::values::ARTIFACT_SIZE;
use serde_json::{json, Value};

use super::*;
use crate::{MAX_DEPTH, MAX_SOURCE_SIZE, MAX_TOKENS};

/// Returns a policy with a comment on line 2 and the given items from line 3.
fn lines(items: &str) -> String {
    format!("policy {{\n  # The items follow.\n{}\n}}\n", items)
}

/// One expected diagnostic: where it is, what it covers, and part of its message.
struct Expected {
    source: String,
    line: usize,
    column: usize,
    offending: &'static str,
    message: &'static str,
}

fn case(source: String, line: usize, column: usize, offending: &'static str, message: &'static str) -> Expected {
    Expected {
        source,
        line,
        column,
        offending,
        message,
    }
}

#[test]
fn common_mistakes_are_reported_at_the_offending_text() {
    let cases = vec![
        // Habits from the node-and-edge syntax.
        case(
            lines("  dogstatsd -> drop"),
            3,
            13,
            "->",
            "`->` is retired: join a pipeline's stages with `|`, as in `dogstatsd | emit primary`.",
        ),
        case(
            lines("  node x = drop"),
            3,
            3,
            "node",
            "`node <id> = ...` is retired: write the operator as a stage of a pipeline, as in `dogstatsd | \
             aggregate(window: 10s) as <id>`.",
        ),
        case(
            lines("  input.dogstatsd | drop"),
            3,
            3,
            "input.dogstatsd",
            "`input.` is retired: a pipeline names a source alone, as in `dogstatsd | ...`.",
        ),
        case(
            lines(r#"  dogstatsd | host_tags(tags: config("host.tags")) | drop"#),
            3,
            31,
            r#"config("host.tags")"#,
            "`config(\"...\")` is retired: a configuration binding is written `config.` and the binding's name, as in \
             `config.host.tags`.",
        ),
        case(
            lines(r#"  dogstatsd | where name prefix_in ["a."] | drop"#),
            3,
            26,
            "prefix_in",
            "`prefix_in` is retired: write `starts_with` and the list, as in `name starts_with [\"a.\", \"b.\"]`.",
        ),
        case(
            lines("  dogstatsd | where all(true, false) | drop"),
            3,
            21,
            "all",
            "`all(...)` is retired: join the predicates with `and`, as in `a and b and c`.",
        ),
        // Writing a pipeline.
        case(
            lines("  dogstatsd host_tags | drop"),
            3,
            13,
            "host_tags",
            "Expected `|` after `dogstatsd`, found `host_tags`.",
        ),
        case(
            lines("  dogstatsd | host_enrichment as | drop"),
            3,
            34,
            "|",
            "Expected a node name after `as`, found `|`.",
        ),
        case(
            lines("  dogstatsd | drop as"),
            4,
            1,
            "}",
            "Expected a node name after `as`, found `}`.",
        ),
        case(
            lines("  dogstatsd | host_enrichment as x as y | drop"),
            3,
            36,
            "as",
            "A node takes one `as`, and this one already has `as x`.",
        ),
        case(
            lines("  exit.aggregated -> drop"),
            3,
            19,
            "->",
            "`->` is retired: join a pipeline's stages with `|`",
        ),
        case(
            lines("  dogstatsd | drop | host_enrichment | drop"),
            3,
            15,
            "drop",
            "`drop` ends a pipeline; to send a copy and keep going, name the stream with `let` and use it twice.",
        ),
        case(
            lines("  dogstatsd | emit primary | drop"),
            3,
            15,
            "emit",
            "`emit` ends a pipeline; to send a copy and keep going, name the stream with `let` and use it twice.",
        ),
        case(
            lines("  let a = dogstatsd | drop\n  a | drop"),
            3,
            23,
            "drop",
            "`drop` ends a pipeline, but `let a` names a stream, which later statements continue",
        ),
        case(
            lines("  dogstatsd | host_enrichment"),
            3,
            3,
            "dogstatsd | host_enrichment",
            "This pipeline does not end: a statement without `let` ends in a sink",
        ),
        case(
            lines("  dogstatsd | host_enrichment (tags: [])| drop"),
            3,
            31,
            "(",
            "Write the arguments of `host_enrichment` right after its name, without a space, as in \
             `host_enrichment(...)`: a `(` after a space starts a tuple, the head of the next statement.",
        ),
        // The retired `fork`.
        case(
            lines("  dogstatsd | fork {\n    emit primary\n    drop\n  }"),
            3,
            15,
            "fork",
            "`fork` is gone: name the stream with `let` and use it once per branch.",
        ),
        case(
            lines("  let a = dogstatsd | fork { drop }"),
            3,
            23,
            "fork",
            "`fork` is gone: name the stream with `let` and use it once per branch.",
        ),
        case(
            lines("  dogstatsd | where | drop"),
            3,
            21,
            "|",
            "Expected a predicate, such as `name == \"requests\"`",
        ),
        case(lines("  dogstatsd | | drop"), 3, 15, "|", "Expected a stage after `|`"),
        case(
            lines("  let = dogstatsd | host_enrichment"),
            3,
            7,
            "=",
            "Expected a name for the stream after `let`, found `=`.",
        ),
        case(
            lines("  (dogstatsd otlp) | drop"),
            3,
            14,
            "otlp",
            "Expected `,` or `)` after the stream, to continue or close the tuple opened at line 3, column 3, found \
             `otlp`.",
        ),
        // Names and ports.
        case(
            lines("  let a = dogstatsd | host_enrichment\n  dogstatsd | drop"),
            3,
            7,
            "a",
            "`a` is never used; use it, write `a | drop` to discard it, or remove the `let`.",
        ),
        case(
            lines("  let s = dogstatsd | split(true)\n  s.true | drop"),
            3,
            7,
            "s",
            "`s.false` is never used; write `s.false | drop`.",
        ),
        case(
            lines("  let t = dogstatsd | split(true)\n  t | drop"),
            4,
            3,
            "t",
            "`t` is a `split`, which has several output ports, so pick a port: `t.true` or `t.false`.",
        ),
        case(
            lines("  dogstatsd | split(true) | drop"),
            3,
            29,
            "drop",
            "`split` has two output ports, `true` and `false`; name it with `let s = dogstatsd | split(true)` and use \
             `s.true` and `s.false`.",
        ),
        case(
            lines("  dogstatsd | tee(ports: 3) | drop"),
            3,
            31,
            "drop",
            "`tee` has three output ports, `0`, `1`, and `2`; name it with `let s = dogstatsd | tee(ports: 3)` and use \
             `s.0`, `s.1`, and `s.2`.",
        ),
        // A name taken twice is reported once, not again for the node it would name.
        case(
            lines("  let x = dogstatsd | host_enrichment\n  let x = otlp | host_enrichment\n  x | drop"),
            4,
            7,
            "x",
            "`x` is defined twice; it is first defined at line 3, column 7.",
        ),
        case(
            lines(
                "  dogstatsd | host_enrichment | drop\n  let input_dogstatsd_host_enrichment = otlp | host_enrichment\n  \
                 input_dogstatsd_host_enrichment | drop",
            ),
            3,
            15,
            "host_enrichment",
            "This `host_enrichment` would be the node `input_dogstatsd_host_enrichment`, which is already the name of \
             the `let` at line 4, column 7.",
        ),
        case(
            lines("  a | drop\n  let a = dogstatsd | host_enrichment\n  a | drop"),
            3,
            3,
            "a",
            "`a` is used before it is defined, at line 4, column 7; programs cannot loop, so define it first.",
        ),
        case(
            lines("  let s = dogstatsd | split(true)\n  s.true | drop\n  s.false | drop\n  s.maybe | drop"),
            6,
            5,
            "maybe",
            "`s` is a `split`, which has no output port `maybe`; pick one of `s.true` or `s.false`.",
        ),
        case(
            lines("  dogstatsd.x | drop"),
            3,
            13,
            "x",
            "`dogstatsd` is a source, which has no ports; write `dogstatsd`.",
        ),
        case(
            lines("  dogstatsd | merge(inputs: 2) | drop"),
            3,
            15,
            "merge(inputs: 2)",
            "`merge` has 2 input ports, so a pipeline cannot enter it; combine streams with a tuple, as in `(a, b)`.",
        ),
        case(
            lines("  dogstatsd | host_enrichment as Bad | drop"),
            3,
            34,
            "Bad",
            "Invalid artifact node ID `Bad`: identifiers start with a lowercase ASCII letter",
        ),
        // Values, predicates, and the header.
        case(
            lines(r#"  dogstatsd | frobnicate(v: "a\qb") | drop"#),
            3,
            31,
            r"\q",
            "Unknown escape sequence `\\q` in a string.",
        ),
        case(
            lines(r#"  dogstatsd | host_tags(tags: ["a", "b") | drop"#),
            3,
            40,
            ")",
            "Expected `,` or `]` after the list element, to continue or close the list opened at line 3, column 31, \
             found `)`.",
        ),
        case(
            lines("  dogstatsd | frobnicate(v: \"abc) | drop"),
            3,
            29,
            "\"abc) | drop",
            "Unterminated string: a string cannot span lines",
        ),
        case(
            lines("  dogstatsd | host_tags(deadline: 1s, deadline: 2s) | drop"),
            3,
            39,
            "deadline",
            "The parameter `deadline` is given twice.",
        ),
        case(
            lines("  dogstatsd | host_tags(tags: [], deadline: 1.5s) | drop"),
            3,
            45,
            "1.5s",
            "Invalid duration `1.5s`: a duration is a whole number followed by a unit, such as `1500ms`.",
        ),
        case(
            lines("  dogstatsd | split(kind in [countr]) | drop"),
            3,
            30,
            "countr",
            "Unknown metric kind `countr`; the kinds are counter, rate, gauge, set, histogram, distribution.",
        ),
        case(
            lines("  dogstatsd | where tags.env | drop"),
            3,
            30,
            "|",
            "Expected `is set`, `is unset`, or a string match after `tags.<key>`: `== \"<s>\"`",
        ),
        case(
            lines("  dogstatsd | where name == | drop"),
            3,
            29,
            "|",
            "Expected a string after `==`, found `|`.",
        ),
        case(
            lines("  dogstatsd | where (true or false | drop"),
            3,
            36,
            "|",
            "Expected `and`, `or`, or `)` to close the parenthesis opened at line 3, column 21, found `|`.",
        ),
        case(
            lines("  dogstatsd | drop").replace("\n}\n", "\n"),
            4,
            1,
            "",
            "Expected `}` to close the policy opened at line 1, column 8, found the end of the input.",
        ),
        case(
            "policy metrics {\n}\n".to_string(),
            1,
            8,
            "metrics",
            "A program is whole: `policy` takes no scope name. Write `policy {`.",
        ),
        case(
            "policy {\n  base \"sha256:xyz\"\n}\n".to_string(),
            2,
            3,
            "base",
            "`base` was removed: a program records nothing of what it was written against.",
        ),
        case(
            lines("  dogstatsd | exit aggregated"),
            3,
            15,
            "exit",
            "`exit` was removed: a program is whole, so every path ends in `emit` or `drop`.",
        ),
        case(
            "dogstatsd | drop".to_string(),
            1,
            1,
            "dogstatsd",
            "Expected `policy`, which starts a policy artifact, found `dogstatsd`.",
        ),
        case(
            "policy {\n}\nextra\n".to_string(),
            3,
            1,
            "extra",
            "Expected the end of the input after the policy's closing `}`, found `extra`.",
        ),
    ];

    for expected in cases {
        let diagnostics = parse_err(&expected.source);
        assert_eq!(diagnostics.len(), 1, "{:?}\n{}", diagnostics, expected.source);
        let diagnostic = &diagnostics[0];
        assert!(
            diagnostic.message.starts_with(expected.message),
            "message {:?} should start with {:?}",
            diagnostic.message,
            expected.message
        );
        assert_eq!(
            (diagnostic.line, diagnostic.column),
            (expected.line, expected.column),
            "{}",
            diagnostic.message
        );
        assert_eq!(
            text(&expected.source, diagnostic.span),
            expected.offending,
            "{}",
            diagnostic.message
        );
    }
}

#[test]
fn every_reference_problem_is_reported_in_source_order() {
    // Lowering finds the name and port problems of the whole program, whatever kind each is, and reports them in the
    // order of the text they underline.
    let source = lines(concat!(
        "  later | drop\n",
        "  let s = dogstatsd | split(true)\n",
        "  s.true | drop\n",
        "  s.maybe | drop\n",
        "  let unused = otlp | host_enrichment\n",
        "  let t = checks | split(true)\n",
        "  t | drop\n",
        "  nowhere.out | drop\n",
        "  let later = liveness | host_enrichment\n",
        "  later | drop",
    ));
    let diagnostics = parse_err(&source);
    let found = diagnostics
        .iter()
        .map(|diagnostic| (diagnostic.line, diagnostic.column, text(&source, diagnostic.span)))
        .collect::<Vec<_>>();
    assert!(
        found
            .windows(2)
            .all(|pair| (pair[0].0, pair[0].1) <= (pair[1].0, pair[1].1)),
        "{:?}",
        found
    );
    let rendered = crate::Diagnostics::to_string(&parse(&source).unwrap_err());
    assert_eq!(
        rendered,
        "3:3: `later` is used before it is defined, at line 11, column 7; programs cannot loop, so define it first.\n\
         4:7: `s.false` is never used; write `s.false | drop`.\n\
         6:5: `s` is a `split`, which has no output port `maybe`; pick one of `s.true` or `s.false`.\n\
         7:7: `unused` is never used; use it, write `unused | drop` to discard it, or remove the `let`.\n\
         9:3: `t` is a `split`, which has several output ports, so pick a port: `t.true` or `t.false`.\n\
         10:11: `nowhere` is a source, which has no ports; write `nowhere`."
    );
}

#[test]
fn misspelled_operators_are_named_when_their_arguments_do_not_parse() {
    // Without a registered operator's schema, a predicate cannot be read, so the error in its arguments comes with a
    // note on the operator, which is the mistake. (stage, offending text of the error, its column, message)
    let cases = [
        (
            r#"splt(name == "x")"#,
            "name",
            "Expected a parameter, written `<name>: <value>`, or `)`, found `name`.",
        ),
        (
            r#"splt(predicate: name == "x")"#,
            "name",
            "Expected a value, found `name`.",
        ),
        (
            r#"splt(tags.team is set)"#,
            "tags",
            "Expected a parameter, written `<name>: <value>`, or `)`, found `tags`.",
        ),
    ];
    for (stage, offending, message) in cases {
        let source = lines(&format!("  dogstatsd | {} | drop", stage));
        let diagnostics = parse_err(&source);
        assert_eq!(diagnostics.len(), 2, "{:?}", diagnostics);

        let note = &diagnostics[0];
        assert_eq!((note.line, note.column, text(&source, note.span)), (3, 15, "splt"));
        assert!(
            note.message.starts_with(
                "`splt` is not a registered operator, so its arguments can only be plain values, written \
                 `<name>: <value>`. Did you mean `split`? The operators are "
            ),
            "{}",
            note.message
        );
        assert!(
            note.message.contains("split, tee, merge, emit, drop"),
            "{}",
            note.message
        );

        let error = &diagnostics[1];
        assert_eq!(text(&source, error.span), offending);
        assert_eq!(error.line, 3);
        assert!(error.message.starts_with(message), "{}", error.message);
    }

    // A registered operator's arguments get no note, and neither does an unknown operator whose arguments parse.
    assert_eq!(parse_err(&lines("  dogstatsd | split(name ==) | drop")).len(), 1);
    let unknown = parse_ok(&lines("  dogstatsd | frobnicate(predicate: true) | drop"));
    assert_eq!(unknown.artifact.nodes[0].op.as_str(), "frobnicate");
    // A misspelled operator with no arguments is an unknown operator, which parsing accepts: validation of the plan it
    // is composed into is what finds it.
    let typo = parse_ok(&lines("  dogstatsd | host_enrichmnt | drop"));
    assert_eq!(typo.artifact.nodes[0].op.as_str(), "host_enrichmnt");
    // A name far from every operator gets no suggestion.
    let diagnostics = parse_err(&lines("  dogstatsd | qqqqqqqq(name) | drop"));
    assert!(
        !diagnostics[0].message.contains("Did you mean"),
        "{}",
        diagnostics[0].message
    );
}

#[test]
fn sources_over_the_source_size_limit_are_rejected() {
    let limit = MAX_SOURCE_SIZE;
    let at_limit = {
        let mut source = policy("");
        let padding = limit - source.len() - 1;
        source.push('#');
        source.push_str(&"x".repeat(padding));
        source
    };
    assert_eq!(at_limit.len(), limit);
    parse_ok(&at_limit);

    let over = format!("{}x", at_limit);
    let diagnostics = parse_err(&over);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].span, crate::Span::new(limit, limit + 1));
    assert_eq!(
        diagnostics[0].message,
        format!(
            "The source is {} bytes, more than the {} bytes a policy's text may have.",
            limit + 1,
            limit
        )
    );
}

#[test]
fn artifacts_over_the_artifact_size_limit_are_rejected() {
    // A list of tags whose artifact is exactly at the limit as JSON, and one byte over it.
    let source_with = |tag: &str| {
        lines(&format!(
            "  dogstatsd | host_tags(tags: [\"{}\"], deadline: 0s) | drop",
            tag
        ))
    };
    let json_size = |source: &str| serde_json::to_vec(&parse_ok(source).artifact).unwrap().len();
    let base = json_size(&source_with(""));
    let limit = ARTIFACT_SIZE as usize;
    let at_limit = source_with(&"x".repeat(limit - base));
    assert_eq!(json_size(&at_limit), limit);

    let over = source_with(&"x".repeat(limit - base + 1));
    let diagnostics = parse_err(&over);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(text(&over, diagnostics[0].span), "policy");
    assert_eq!(
        diagnostics[0].message,
        format!(
            "The artifact is {} bytes as JSON, more than the artifact size limit of {} bytes.",
            limit + 1,
            limit
        )
    );
}

#[test]
fn sources_near_the_limits_parse_in_linear_time() {
    // Regression guard: messages that name where a list was opened were once built for every element, scanning the
    // whole source each time, so a long list took time quadratic in its length (minutes at this size).
    let elements = 40_000;
    let list = vec!["\"a\""; elements].join(", ");
    let source = policy(&format!("  dogstatsd | frobnicate(v: [{}]) | drop", list));
    assert!(source.len() < ARTIFACT_SIZE as usize);

    let started = std::time::Instant::now();
    let parsed = parse_ok(&source);
    let printed = print_default(&parsed.artifact);
    // Printed one element per line, and parsed back.
    assert!(printed.lines().count() > elements);
    assert_eq!(parse_ok(&printed).artifact, parsed.artifact);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "took {:?}",
        started.elapsed()
    );
    let value = parsed.artifact.nodes[0].params.get("v").unwrap();
    assert_eq!(value.as_array().map(Vec::len), Some(elements));
}

/// Returns a policy with the width of a wide node in each form: a `tee`, a tuple, and a stream used many times (in one
/// tuple).
fn wide_sources(width: usize) -> [(&'static str, String); 3] {
    // Scrambled orders: 7919 is prime, so multiplying by it permutes the indexes.
    let scrambled = |i: usize| i * 7919 % width;
    let tee = format!(
        "  let t = dogstatsd | tee(ports: {})\n{}",
        width,
        (0..width)
            .map(|i| format!("  t.{} | drop", scrambled(i)))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let tuple = format!(
        "  ({}) | drop",
        (0..width)
            .map(|i| format!("i{}", scrambled(i)))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let uses = format!(
        "  let x = dogstatsd | host_enrichment\n  ({}) | drop",
        vec!["x"; width].join(", ")
    );
    [
        ("tee", policy(&tee)),
        ("tuple", policy(&tuple)),
        ("uses", policy(&uses)),
    ]
}

#[test]
fn wide_nodes_within_the_size_limit_parse_and_print_back() {
    // As wide as the artifact size limit lets each form be.
    for (form, source) in wide_sources(400) {
        let parsed = parse_ok(&source);
        let printed = print_default(&parsed.artifact);
        let reparsed = parse_ok(&printed);
        assert_eq!(reparsed.artifact, parsed.artifact, "{} prints and parses back", form);
        assert_eq!(
            print_default(&reparsed.artifact),
            printed,
            "{} prints to a fixed point",
            form
        );
    }
}

#[test]
fn wide_nodes_parse_and_print_in_time_proportional_to_their_edges() {
    // Regression guard: ordering a node's edges, and checking their ports, once scanned the node's ports for each
    // edge, so two such tees took seconds to parse and to print.
    let width = 10_000;

    // Each form's artifact, built by hand with its edges in reverse order, as a tool writing JSON might give them. A
    // `tee` in the core form, a tuple's merge, and the tee after a reused stream.
    let artifact_of = |nodes: Vec<Value>, mut edges: Vec<Value>| {
        edges.reverse();
        raw_artifact(json!({
            "ir_version": 1, "nodes": nodes, "edges": edges,
        }))
    };
    let mut nodes = vec![json!({"id": "t", "op": "tee", "params": {"ports": width}})];
    let mut edges = vec![json!({"from": "input.dogstatsd", "to": "t.in"})];
    for port in 0..width {
        nodes.push(json!({"id": format!("t_{}", port), "op": "drop"}));
        edges.push(json!({"from": format!("t.{}", port), "to": format!("t_{}.in", port)}));
    }
    let tee = artifact_of(nodes, edges);

    let mut nodes = vec![json!({"id": "input_i0_in", "op": "merge", "params": {"inputs": width}})];
    let mut edges = Vec::new();
    for port in 0..width {
        edges.push(json!({"from": format!("input.i{}", port), "to": format!("input_i0_in.{}", port)}));
    }
    nodes.push(json!({"id": "input_i0_in_out", "op": "drop"}));
    edges.push(json!({"from": "input_i0_in.out", "to": "input_i0_in_out.in"}));
    let tuple = artifact_of(nodes, edges);

    let mut nodes = vec![
        json!({"id": "x", "op": "host_enrichment"}),
        json!({"id": "x_fanout", "op": "tee", "params": {"ports": width}}),
    ];
    let mut edges = vec![
        json!({"from": "input.dogstatsd", "to": "x.in"}),
        json!({"from": "x.out", "to": "x_fanout.in"}),
    ];
    for port in 0..width {
        edges.push(json!({"from": format!("x_fanout.{}", port), "to": format!("x_fanout_0_in.{}", port)}));
    }
    nodes.push(json!({"id": "x_fanout_0_in", "op": "merge", "params": {"inputs": width}}));
    nodes.push(json!({"id": "x_fanout_0_in_out", "op": "drop"}));
    edges.push(json!({"from": "x_fanout_0_in.out", "to": "x_fanout_0_in_out.in"}));
    let uses = artifact_of(nodes, edges);

    for (form, source) in wide_sources(width) {
        // Each source is over the artifact size limit, which parsing finds once it has built the artifact.
        let started = std::time::Instant::now();
        let diagnostics = parse_err(&source);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "parsing {} took {:?}",
            form,
            started.elapsed()
        );
        assert!(
            diagnostics[0].message.starts_with("The artifact is "),
            "{}: {}",
            form,
            diagnostics[0].message
        );
    }

    for (form, wide, sugar) in [
        (
            "tee",
            &tee,
            "  let t = dogstatsd | tee(ports: 10000)\n\n  t.0 | drop\n\n  t.1 | drop\n",
        ),
        ("tuple", &tuple, "(\n    i0,\n    i1,\n    i2,\n"),
        ("uses", &uses, "(\n    x,\n    x,\n    x,\n"),
    ] {
        let started = std::time::Instant::now();
        let printed = print_default(wide);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "printing {} took {:?}",
            form,
            started.elapsed()
        );
        // Edges follow the port order whatever order they are given in, so each prints with its sugar.
        let mut in_order = wide.clone();
        in_order.edges.reverse();
        assert_eq!(printed, print_default(&in_order), "{}", form);
        assert!(printed.contains(sugar), "{}: {}", form, &printed[..400]);
    }
}

/// Returns a split with `predicate`, whose ports both lead to inline drops, as an artifact.
fn split_artifact(predicate: Value) -> PolicyArtifact {
    artifact(json!({
        "ir_version": 1,
        "nodes": [
            {"id": "s", "op": "split", "params": {"predicate": predicate}},
            {"id": "s_true", "op": "drop"},
            {"id": "s_false", "op": "drop"},
        ],
        "edges": [
            {"from": "input.metrics", "to": "s.in"},
            {"from": "s.true", "to": "s_true.in"},
            {"from": "s.false", "to": "s_false.in"},
        ],
    }))
}

/// Asserts that `artifact` is within the artifact size limit, valid as far as its operators can tell, and printed
/// with `options`, parses back to itself, and that printing what it parses to gives the same text. Returns the text.
#[track_caller]
fn assert_large_round_trip(artifact: &PolicyArtifact, options: &PrintOptions) -> String {
    let json = serde_json::to_string(artifact).unwrap();
    assert!(
        json.len() <= ARTIFACT_SIZE as usize,
        "the artifact is {} bytes",
        json.len()
    );
    assert_eq!(&PolicyArtifact::from_json(&json).unwrap(), artifact);
    let printed = print(artifact, options);
    assert!(printed.len() <= MAX_SOURCE_SIZE, "the text is {} bytes", printed.len());
    let parsed = parse(&printed).unwrap_or_else(|diagnostics| panic!("{}", diagnostics));
    assert!(&parsed.artifact == artifact, "the text parses back to another artifact");
    assert!(
        print(&parsed.artifact, options) == printed,
        "printing again gives other text"
    );
    printed
}

#[test]
fn artifacts_within_the_size_limit_print_to_text_that_parses_back() {
    use saluki_metric_plan::operators::OperatorRegistry;

    let names = |count: usize, width: usize| {
        (0..count)
            .map(|i| format!("{:0width$}", i, width = width))
            .collect::<Vec<_>>()
    };
    let configures = |artifact: &PolicyArtifact| {
        let node = &artifact.nodes[0];
        assert!(OperatorRegistry::native()
            .configure(node.op.as_str(), &node.params)
            .is_ok());
    };

    // A long allowlist of short names, printed one per line: about twice its JSON.
    let allowlist = split_artifact(json!({"name": {"in": names(32_000, 5)}}));
    configures(&allowlist);
    let printed = assert_large_round_trip(&allowlist, &PrintOptions::default());
    assert!(
        printed.len() > 2 * ARTIFACT_SIZE as usize - 100_000,
        "{}",
        printed.len()
    );
    assert!(printed.contains("\n        \"00001\",\n"));

    // The same kind of list nested in alternating junctions as deep as JSON allows, with one element per line, at the
    // depth of its nesting, which the source size limit still has room for.
    let mut deep = json!({"name": {"in": names(10_000, 7)}});
    for level in 0..58 {
        let junction = if level % 2 == 0 { "all" } else { "any" };
        deep = json!({junction: [{"has_tag": format!("k{}", level)}, deep]});
    }
    let deep = split_artifact(deep);
    configures(&deep);
    let printed = assert_large_round_trip(&deep, &PrintOptions::default());
    assert!(printed.contains("| where tags.k57 is set\n        or (\n          tags.k56 is set\n          and (\n"));
    assert!(printed.lines().count() > 10_000, "{} lines", printed.lines().count());

    // A string of characters written as escapes takes six times its JSON.
    let length = ARTIFACT_SIZE as usize - 1000;
    let escaped = split_artifact(json!({"name": {"eq": "\u{7f}".repeat(length)}}));
    configures(&escaped);
    let printed = assert_large_round_trip(&escaped, &PrintOptions::default());
    assert!(printed.len() > 6 * length, "{}", printed.len());

    // Lists of single values, each broken across lines at a width of zero, add a token for each trailing comma, which
    // would put the text over the token limit, so it is printed on one line.
    let dense = raw_artifact(json!({
        "ir_version": 1,
        "nodes": [
            {"id": "input_dogstatsd_frobnicate", "op": "frobnicate", "params": {"v": vec![[0]; 60_000]}},
            {"id": "input_dogstatsd_frobnicate_out", "op": "drop"},
        ],
        "edges": [
            {"from": "input.dogstatsd", "to": "input_dogstatsd_frobnicate.in"},
            {"from": "input_dogstatsd_frobnicate.out", "to": "input_dogstatsd_frobnicate_out.in"},
        ],
    }));
    let narrow = PrintOptions { width: 0 };
    let printed = assert_large_round_trip(&dense, &narrow);
    assert!(printed.contains("| frobnicate(v: [[0], [0], "));
}

#[test]
fn sources_with_too_many_tokens_are_rejected() {
    // Every element is two tokens, `1` and `,`, of one byte each.
    let elements = MAX_TOKENS / 2 + 1;
    let source = policy(&format!(
        "  dogstatsd | frobnicate(v: [{}]) | drop",
        "1,".repeat(elements)
    ));
    assert!(source.len() < MAX_SOURCE_SIZE);
    let diagnostics = parse_err(&source);
    assert_eq!(diagnostics.len(), 1);
    assert!(
        diagnostics[0]
            .message
            .starts_with("The source has more than 262144 tokens"),
        "{}",
        diagnostics[0].message
    );
}

/// Returns `open` repeated `levels` times, then `inner`, then `close` as many times.
fn nest(levels: usize, open: &str, inner: &str, close: &str) -> String {
    format!("{}{}{}", open.repeat(levels), inner, close.repeat(levels))
}

/// Returns a policy whose `where` predicate nests `levels` deep: the predicate is one level, and each of the
/// repetitions of `open` in it one more.
fn nested_predicate(levels: usize, open: &str, inner: &str, close: &str) -> String {
    policy(&format!(
        "  dogstatsd | where {} | drop",
        nest(levels - 1, open, inner, close)
    ))
}

/// A form of nesting, with a policy that nests it a given number of levels deep, as the depth limit counts them.
type NestingForm = (&'static str, fn(usize) -> String);

/// Every form of nesting.
const NESTING_FORMS: &[NestingForm] = &[
    ("lists", |levels| {
        policy(&format!(
            "  dogstatsd | host_tags(tags: {}) | drop",
            nest(levels, "[", "", "]")
        ))
    }),
    ("records", |levels| {
        policy(&format!(
            "  dogstatsd | host_tags(tags: {}) | drop",
            nest(levels, "{ a: ", "1", " }")
        ))
    }),
    ("an unregistered operator's parameter", |levels| {
        policy(&format!(
            "  dogstatsd | frobnicate(v: {}) | drop",
            nest(levels, "[", "", "]")
        ))
    }),
    ("a blob", |levels| {
        policy(&format!("  blobs {{ \"{}\" = {} }}", HASH, nest(levels, "[", "", "]")))
    }),
    ("parentheses", |levels| nested_predicate(levels, "(", "true", ")")),
    ("`not`", |levels| nested_predicate(levels, "not ", "true", "")),
    ("`and`", |levels| nested_predicate(levels, "(true and ", "true", ")")),
    ("`or`", |levels| nested_predicate(levels, "(true or ", "true", ")")),
    ("`not` of `and`", |levels| {
        // Each repetition is two levels, `not` and the parenthesis; a leading `not` makes up an odd remainder.
        let not = "not ".repeat((levels - 1) % 2);
        policy(&format!(
            "  dogstatsd | where {}{} | drop",
            not,
            nest((levels - 1) / 2, "not (true and ", "true", ")")
        ))
    }),
    ("a `split` operator's predicate", |levels| {
        policy(&format!(
            "  let s = dogstatsd | split({})\n  s.true | drop\n  s.false | drop",
            nest(levels - 1, "(", "true", ")")
        ))
    }),
    ("a named predicate parameter", |levels| {
        policy(&format!(
            "  let s = dogstatsd | split(predicate: {})\n  s.true | drop\n  s.false | drop",
            nest(levels - 1, "(", "true", ")")
        ))
    }),
];

#[test]
fn nesting_is_bounded_without_exhausting_the_stack() {
    on_small_stack(|| {
        let too_deep = |diagnostic: &Diagnostic| diagnostic.message.starts_with("This nests more than 128 levels deep");

        // Every form nested exactly as deep as allowed parses and prints back, and one level deeper is too deep. This
        // is where parsing and printing use the most stack.
        for (form, nested) in NESTING_FORMS {
            let parsed = parse_ok(&nested(MAX_DEPTH));
            let printed = print_default(&parsed.artifact);
            let reparsed = parse_ok(&printed);
            assert_eq!(reparsed.artifact, parsed.artifact, "{} print and parse back", form);
            assert_eq!(
                print_default(&reparsed.artifact),
                printed,
                "{} print to a fixed point",
                form
            );

            // An unregistered operator's error follows a note naming the operator.
            let diagnostics = parse_err(&nested(MAX_DEPTH + 1));
            assert!(diagnostics.iter().any(too_deep), "{}: {:?}", form, diagnostics);
        }

        // The error is reported at the bracket that goes too deep.
        let diagnostics = parse_err(&NESTING_FORMS[0].1(MAX_DEPTH + 1));
        assert_eq!(diagnostics[0].column, 31 + MAX_DEPTH);

        // Far deeper inputs of every nesting form are rejected the same way.
        let depth = 30_000;
        let deep_inputs = [
            format!("  dogstatsd | host_tags(tags: {}) | drop", "[".repeat(depth)),
            format!("  dogstatsd | host_tags(tags: {}) | drop", "{ a: ".repeat(depth)),
            format!("  dogstatsd | where {}true | drop", "not ".repeat(depth)),
            format!("  dogstatsd | where {}true | drop", "(".repeat(depth)),
            format!("  dogstatsd | where {}true | drop", "not (".repeat(depth)),
        ];
        for items in deep_inputs {
            let diagnostics = parse_err(&policy(&items));
            assert!(
                too_deep(&diagnostics[0]),
                "{}: {}",
                &items[..40],
                diagnostics[0].message
            );
        }
    });
}
