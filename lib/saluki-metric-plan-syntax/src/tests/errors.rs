//! Diagnostics for common mistakes, and the bounds on what a source may contain.

use saluki_metric_plan::limits::values::ARTIFACT_SIZE;
use serde_json::{json, Value};

use super::*;
use crate::{MAX_DEPTH, MAX_SOURCE_SIZE, MAX_TOKENS};

/// Returns a policy with [`BASE`] and the given items, the base on line 2 and the items from line 3.
fn lines(items: &str) -> String {
    format!("policy p {{\n  base \"{}\"\n{}\n}}\n", BASE, items)
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
        case(
            lines("  input.metrics -> s\n  node s = split(true) {\n    maybe -> drop\n  }"),
            5,
            5,
            "maybe",
            "Node `s` (`split`) has no output port `maybe`; its output ports are `true`, `false`.",
        ),
        case(
            lines("  input.metrics -> a\n  node a = tee {\n    -> drop\n  node b = drop"),
            6,
            3,
            "node",
            "Expected `}` to close the block of node `a` opened at line 4, column 16, found `node`.",
        ),
        case(
            format!("policy p {{\n  base \"{}\"\n  node a = tee {{\n    -> drop\n", BASE),
            5,
            1,
            "",
            "Expected `}` to close the block of node `a` opened at line 3, column 16, found the end of the input.",
        ),
        case(
            lines(r#"  node n = frobnicate(v: "a\qb")"#),
            3,
            28,
            r"\q",
            "Unknown escape sequence `\\q` in a string.",
        ),
        case(
            lines(r#"  node n = host_tags(tags: ["a", "b")"#),
            3,
            37,
            ")",
            "Expected `,` or `]` after the list element, to continue or close the list opened at line 3, column 28, \
             found `)`.",
        ),
        case(
            lines(r#"  node n = frobnicate(v: "abc)"#),
            3,
            26,
            "\"abc)",
            "Unterminated string: a string cannot span lines",
        ),
        case(
            lines("  input.metrics -> s\n  node s = split(true) {\n    -> drop\n  }"),
            5,
            5,
            "-> drop",
            "Node `s` (`split`) has 2 output ports (`true`, `false`); name the port this edge leaves, as in \
             `true -> ...`.",
        ),
        case(
            lines("  input.metrics -> nowhere"),
            3,
            20,
            "nowhere",
            "There is no node `nowhere`; define it with `node nowhere = ...`.",
        ),
        case(
            lines("  node a = drop\n  node a = drop"),
            4,
            8,
            "a",
            "The node `a` is defined twice; it is first defined at line 3, column 3.",
        ),
        case(
            lines(concat!(
                "  input.metrics -> s\n",
                "  node s = split(true) {\n    true  -> drop\n    false -> drop\n  }\n",
                "  node s_true = drop",
            )),
            5,
            14,
            "drop",
            "This inline `drop` is the node `s_true` (named after port `true` of `s`), but `s_true` is already \
             defined at line 8, column 3. Rename that node, or define this terminal as a node of its own.",
        ),
        case(
            lines("  input.metrics -> m\n  node m = merge(inputs: 2) {\n    -> drop\n  }"),
            3,
            20,
            "m",
            "Node `m` (`merge`) has 2 input ports (`0`, `1`); name the port this edge enters, as in `m.0`.",
        ),
        case(
            lines("  input.metrics -> m.7\n  node m = merge(inputs: 2) {\n    -> drop\n  }"),
            3,
            20,
            "m.7",
            "Node `m` (`merge`) has no input port `7`; its input ports are `0`, `1`.",
        ),
        case(
            "policy p {\n}\n".to_string(),
            2,
            1,
            "}",
            "Expected `base \"<program hash>\"` in the policy",
        ),
        case(
            lines("  node Bad = drop"),
            3,
            8,
            "Bad",
            "Invalid artifact node ID `Bad`: identifiers start with a lowercase ASCII letter",
        ),
        case(
            lines("  node input = drop"),
            3,
            8,
            "input",
            "Invalid artifact node ID `input`: `input` is reserved for the scope's inputs",
        ),
        case(
            lines("  node s = split(kind in [countr])"),
            3,
            27,
            "countr",
            "Unknown metric kind `countr`; the kinds are counter, rate, gauge, set, histogram, distribution.",
        ),
        case(
            lines(r#"  node n = namespace_prefix(namespace: config("Not A Binding"))"#),
            3,
            47,
            "\"Not A Binding\"",
            "Invalid binding name `Not A Binding`",
        ),
        case(
            lines("  node n = namespace_prefix(namespace: dogstatsd)"),
            3,
            40,
            "dogstatsd",
            "Expected a value, found `dogstatsd`. Strings are written in double quotes, as \"dogstatsd\".",
        ),
        case(
            lines("  node n = host_tags(deadline: 1s, deadline: 2s)"),
            3,
            36,
            "deadline",
            "The parameter `deadline` is given twice.",
        ),
        case(
            lines("  input.metrics -> t\n  node t = tee {\n    0 -> drop\n  }"),
            5,
            5,
            "0",
            "A `tee` written without parameters numbers its ports in order",
        ),
        case(
            "policy p {\n  base \"sha256:xyz\"\n}\n".to_string(),
            2,
            8,
            "\"sha256:xyz\"",
            "Invalid hash `sha256:xyz`",
        ),
        case(
            "node a = drop".to_string(),
            1,
            1,
            "node",
            "Expected `policy`, which starts a policy artifact, found `node`.",
        ),
        case(
            format!("policy p {{\n  base \"{}\"\n}}\nextra\n", BASE),
            4,
            1,
            "extra",
            "Expected the end of the input after the policy's closing `}`, found `extra`.",
        ),
        case(
            lines("  input.metrics -> s\n  node s = split(true) {\n    true -> emit\n  }"),
            6,
            3,
            "}",
            "Expected an output ID after `emit`, found `}`.",
        ),
        case(
            lines("  node s = split(name ==)"),
            3,
            25,
            ")",
            "Expected a string after `==`, found `)`.",
        ),
        case(
            lines("  node s = split((true or false {"),
            3,
            33,
            "{",
            "Expected `and`, `or`, or `)` to close the parenthesis opened at line 3, column 18, found `{`.",
        ),
        case(
            lines("  frob"),
            3,
            3,
            "frob",
            "Expected `base \"<program hash>\"`, `node <id> = ...`, an edge such as `input.metrics -> <target>`, or \
             `}` to close the policy, found `frob`.",
        ),
        case(
            lines("  node t = tee(2)"),
            3,
            16,
            "2",
            "Expected a parameter, written `<name>: <value>`, or `)`; `tee` takes: ports, found the number `2`.",
        ),
        case(
            lines("  node x drop"),
            3,
            10,
            "drop",
            "Expected `=` and the node's operator, found `drop`.",
        ),
        case(
            lines("  ir_version one"),
            3,
            14,
            "one",
            "Expected an IR version, a whole number such as `1`, found `one`.",
        ),
        case(
            lines("  blobs [1]"),
            3,
            9,
            "[",
            "Expected `{` to open the blob table, found `[`.",
        ),
        case(
            lines("  node x = 3"),
            3,
            12,
            "3",
            "Expected an operator name, such as `split` or `tee`, found the number `3`.",
        ),
        // `emit` without its output takes the port of the next line as its output.
        case(
            lines("  input.metrics -> s\n  node s = split(true) {\n    true -> emit false -> drop\n  }"),
            5,
            18,
            "false",
            "Expected an output ID after `emit`, found `false`, which is a port of node `s` (`split`) and starts the \
             next edge.",
        ),
        // A node's block missing its `}` takes the policy's.
        case(
            format!(
                "policy p {{\n  base \"{}\"\n  input.metrics -> s\n  node s = split(true) {{\n    true -> drop\n    \
                 false -> drop\n}}\n",
                BASE
            ),
            8,
            1,
            "",
            "Expected `}` to close the policy opened at line 1, column 10, found the end of the input. If a node's \
             block is missing its `}`, the policy's `}` closed it instead: check the block of node `s` opened at line \
             4, column 24.",
        ),
        case(
            lines(r#"  node s = split(has_tag "env")"#),
            3,
            26,
            "\"env\"",
            "Expected `(` after `has_tag`, as in `has_tag(\"env\")`, found a string.",
        ),
        case(
            lines(r#"  node s = split(name matches ["a"])"#),
            3,
            31,
            "[",
            "Expected `(` after `matches`, as in `matches(values: [\"a\"], prefix: false)`, found `[`.",
        ),
        case(
            lines(r#"  node s = split(name == "a" has_tag("b"))"#),
            3,
            30,
            "has_tag",
            "Expected `and`, `or`, or `)` after the predicate, found `has_tag`.",
        ),
        case(
            lines(r#"  node s = split(predicate: name == "a" has_tag("b"))"#),
            3,
            41,
            "has_tag",
            "Expected `and`, `or`, `,`, or `)` after the predicate, found `has_tag`.",
        ),
        case(
            lines("  node h = host_enrichment\n  h -> drop"),
            4,
            5,
            "->",
            "Expected `.` and a port after `h`, found `->`: an edge outside a node's block names the port it leaves, \
             as in `h.<port> -> <target>`. Or write the edge in the block of node `h`.",
        ),
        case(
            lines("  input -> drop"),
            3,
            9,
            "->",
            "Expected `.` and the input's name after `input`, as in `input.metrics -> <target>`, found `->`.",
        ),
        case(
            lines("  input.metrics -> s\n  node s = split(true) {\n    true = drop\n  }"),
            5,
            10,
            "=",
            "Expected `->` after the port `true`: edges are written `<port> -> <target>`, found `=`.",
        ),
        case(
            lines("  node h = host_tags(tags: [], deadline: 1.5s)"),
            3,
            42,
            "1.5s",
            "Invalid duration `1.5s`: a duration is a whole number followed by a unit, such as `1500ms`.",
        ),
        case(
            lines(r#"  node s = split(name != "a")"#),
            3,
            23,
            "!",
            "Unexpected character `!`. Negate a predicate with `not`, as in `not name == \"a\"`; there is no `!=`.",
        ),
        case(
            lines("  exit.aggregated -> drop"),
            3,
            3,
            "exit.aggregated",
            "An edge cannot leave `exit.aggregated`, a scope exit: exits are where edges leave the scope, so an exit \
             is only ever an edge's target, as in `<node>.<port> -> exit.aggregated`.",
        ),
        case(
            lines("  input.metrics -> exit.0"),
            3,
            25,
            "0",
            "Expected a scope exit name after `exit.`",
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
    let source = lines("  input.metrics -> s\n  node s = split(true) {\n    yes -> drop\n    false -> nowhere\n  }");
    let diagnostics = parse_err(&source);
    let found = diagnostics
        .iter()
        .map(|diagnostic| (diagnostic.line, diagnostic.column, text(&source, diagnostic.span)))
        .collect::<Vec<_>>();
    assert_eq!(found, [(5, 5, "yes"), (6, 14, "nowhere")]);
    let rendered = crate::Diagnostics::to_string(&parse(&source).unwrap_err());
    assert_eq!(
        rendered,
        "5:5: Node `s` (`split`) has no output port `yes`; its output ports are `true`, `false`.\n\
         6:14: There is no node `nowhere`; define it with `node nowhere = ...`."
    );
}

#[test]
fn misspelled_operators_are_named_when_their_arguments_do_not_parse() {
    // Without a registered operator's schema, a predicate cannot be read, so the error in its arguments comes with a
    // note on the operator, which is the mistake. (source line, offending text of the error, its column, message)
    let cases = [
        (
            r#"  node s = splt(name == "x") {"#,
            "name",
            17,
            "Expected a parameter, written `<name>: <value>`, or `)`, found `name`.",
        ),
        (
            r#"  node s = splt(predicate: name == "x") {"#,
            "name",
            28,
            "Expected a value, found `name`.",
        ),
        (
            r#"  node s = splt(has_tag("a")) {"#,
            "has_tag",
            17,
            "Expected a parameter, written `<name>: <value>`, or `)`, found `has_tag`.",
        ),
    ];
    for (line, offending, column, message) in cases {
        let source = lines(&format!(
            "  input.metrics -> s\n{}\n    true  -> drop\n    false -> drop\n  }}",
            line
        ));
        let diagnostics = parse_err(&source);
        assert_eq!(diagnostics.len(), 2, "{:?}", diagnostics);

        let note = &diagnostics[0];
        assert_eq!((note.line, note.column, text(&source, note.span)), (4, 12, "splt"));
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
        assert_eq!(
            (error.line, error.column, text(&source, error.span)),
            (4, column, offending)
        );
        assert!(error.message.starts_with(message), "{}", error.message);
    }

    // A registered operator's arguments get no note, and neither does an unknown operator whose arguments parse.
    assert_eq!(parse_err(&lines("  node s = split(name ==)")).len(), 1);
    let unknown = parse_ok(&lines("  node s = frobnicate(predicate: true)"));
    assert_eq!(unknown.artifact.nodes[0].op.as_str(), "frobnicate");
    // A name far from every operator gets no suggestion.
    let diagnostics = parse_err(&lines("  node s = qqqqqqqq(name)"));
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
            "  input.metrics -> h\n  node h = host_tags(tags: [\"{}\"], deadline: 0s) {{\n    -> drop\n  }}",
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
    assert_eq!(text(&over, diagnostics[0].span), "policy p");
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
    let source = policy(&format!("  node n = frobnicate(v: [{}])", list));
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

#[test]
fn wide_nodes_parse_and_print_in_time_proportional_to_their_edges() {
    // Regression guard: ordering a node's edges, and checking their ports, once scanned the node's ports for each
    // edge, so two such tees took seconds to parse and to print.
    let ports = 10_000;
    let tee = |name: &str, input: &str| {
        // Edge lines in a scrambled order: 7919 is prime, so multiplying by it permutes the ports.
        let lines = (0..ports)
            .map(|i| format!("    {} -> drop\n", i * 7919 % ports))
            .collect::<String>();
        format!(
            "  input.{} -> {}\n  node {} = tee(ports: {}) {{\n{}  }}\n",
            input, name, name, ports, lines
        )
    };
    let source = policy(&format!("{}{}", tee("t0", "i0"), tee("t1", "i1")));

    // The same graph as an artifact, with its edges in reverse order, as a tool writing JSON might give them.
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for (tee, input) in [("t0", "i0"), ("t1", "i1")] {
        nodes.push(json!({"id": tee, "op": "tee", "params": {"ports": ports}}));
        edges.push(json!({"from": format!("input.{}", input), "to": format!("{}.in", tee)}));
        for port in 0..ports {
            nodes.push(json!({"id": format!("{}_{}", tee, port), "op": "drop"}));
            edges.push(json!({"from": format!("{}.{}", tee, port), "to": format!("{}_{}.in", tee, port)}));
        }
    }
    let in_order = raw_artifact(json!({
        "ir_version": 1, "scope": "post_enrich", "base": {"program_hash": BASE}, "nodes": nodes, "edges": edges,
    }));
    let mut reversed = in_order.clone();
    reversed.edges.reverse();

    let started = std::time::Instant::now();
    // Each inline `drop` is a node of its own, so the artifact's JSON is over the size limit, which parsing finds
    // once it has built the artifact.
    let diagnostics = parse_err(&source);
    let printed = print_default(&reversed);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "took {:?}",
        started.elapsed()
    );
    assert!(
        diagnostics[0].message.starts_with("The artifact is "),
        "{}",
        diagnostics[0].message
    );
    // Edges follow the port order whatever order they are given in, so the tees print with their sugar.
    assert_eq!(printed, print_default(&in_order));
    assert!(
        printed.contains("  node t0 = tee {\n    -> drop\n    -> drop\n"),
        "{}",
        &printed[..400]
    );
    assert_eq!(printed.matches("-> drop\n").count(), 2 * ports);
}

/// Returns a split with `predicate`, whose ports both lead to inline drops, as an artifact.
fn split_artifact(predicate: Value) -> PolicyArtifact {
    artifact(json!({
        "ir_version": 1,
        "scope": "post_enrich",
        "base": {"program_hash": BASE},
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
    assert!(printed.contains("\n      \"00001\",\n"));

    // The same kind of list nested in alternating junctions almost as deep as JSON allows: one element per line, at
    // the depth of its nesting, would be far longer than parsing accepts, so it is printed on one line.
    let mut deep = json!({"name": {"in": names(20_000, 7)}});
    for level in 0..58 {
        let junction = if level % 2 == 0 { "all" } else { "any" };
        deep = json!({junction: [{"has_tag": format!("k{}", level)}, deep]});
    }
    let deep = split_artifact(deep);
    configures(&deep);
    let printed = assert_large_round_trip(&deep, &PrintOptions::default());
    assert!(printed.contains(r#"node s = split(has_tag("k57") or (has_tag("k56") and (has_tag("k55") or"#));
    assert!(printed.lines().count() < 20, "{} lines", printed.lines().count());

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
        "scope": "post_enrich",
        "base": {"program_hash": BASE},
        "nodes": [{"id": "n", "op": "frobnicate", "params": {"v": vec![[0]; 60_000]}}],
        "edges": [],
    }));
    let narrow = PrintOptions {
        width: 0,
        ..PrintOptions::default()
    };
    let printed = assert_large_round_trip(&dense, &narrow);
    assert!(printed.contains("node n = frobnicate(v: [[0], [0], "));
}

#[test]
fn sources_with_too_many_tokens_are_rejected() {
    // Every element is two tokens, `1` and `,`, of one byte each.
    let elements = MAX_TOKENS / 2 + 1;
    let source = policy(&format!("  node n = frobnicate(v: [{}])", "1,".repeat(elements)));
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

/// Returns a policy whose `split` predicate nests `levels` deep: the predicate is one level, and each of the
/// repetitions of `open` in it one more.
fn nested_predicate(levels: usize, open: &str, inner: &str, close: &str) -> String {
    policy(&format!("  node s = split({})", nest(levels - 1, open, inner, close)))
}

/// A form of nesting, with a policy that nests it a given number of levels deep, as the depth limit counts them.
type NestingForm = (&'static str, fn(usize) -> String);

/// Every form of nesting.
const NESTING_FORMS: &[NestingForm] = &[
    ("lists", |levels| {
        policy(&format!("  node n = host_tags(tags: {})", nest(levels, "[", "", "]")))
    }),
    ("records", |levels| {
        policy(&format!(
            "  node n = host_tags(tags: {})",
            nest(levels, "{ a: ", "1", " }")
        ))
    }),
    ("an unregistered operator's parameter", |levels| {
        policy(&format!("  node n = frobnicate(v: {})", nest(levels, "[", "", "]")))
    }),
    ("a blob", |levels| {
        policy(&format!("  blobs {{ \"{}\" = {} }}", BASE, nest(levels, "[", "", "]")))
    }),
    ("parentheses", |levels| nested_predicate(levels, "(", "true", ")")),
    ("`not`", |levels| nested_predicate(levels, "not ", "true", "")),
    ("`and`", |levels| nested_predicate(levels, "(true and ", "true", ")")),
    ("`or`", |levels| nested_predicate(levels, "(true or ", "true", ")")),
    ("`all`", |levels| nested_predicate(levels, "all(", "true", ")")),
    ("`any` of several operands", |levels| {
        nested_predicate(levels, "any(true, ", "false", ")")
    }),
    ("`not` of `and`", |levels| {
        // Each repetition is two levels, `not` and the parenthesis; a leading `not` makes up an odd remainder.
        let not = "not ".repeat((levels - 1) % 2);
        policy(&format!(
            "  node s = split({}{})",
            not,
            nest((levels - 1) / 2, "not (true and ", "true", ")")
        ))
    }),
    ("raw records in a predicate", |levels| {
        nested_predicate(levels, "{ not: ", "true", " }")
    }),
    ("raw lists in a predicate", |levels| {
        nested_predicate(levels, "[", "", "]")
    }),
    ("a named predicate parameter", |levels| {
        policy(&format!(
            "  node s = split(predicate: {})",
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
        assert_eq!(diagnostics[0].column, 28 + MAX_DEPTH);

        // Far deeper inputs of every nesting form are rejected the same way.
        let depth = 30_000;
        let deep_inputs = [
            format!("  node n = host_tags(tags: {})", "[".repeat(depth)),
            format!("  node n = host_tags(tags: {})", "{ a: ".repeat(depth)),
            format!("  node s = split({}true)", "not ".repeat(depth)),
            format!("  node s = split({}true)", "(".repeat(depth)),
            format!("  node s = split({}true)", "all(".repeat(depth)),
            format!("  node s = split({}true)", "not (".repeat(depth)),
            format!("  node s = split({{ a: {})", "[".repeat(depth)),
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
