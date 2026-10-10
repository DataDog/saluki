//! Property tests: printing and parsing are inverse over generated artifacts, and no input makes parsing panic.

use proptest::{
    collection::vec, option, prelude::*, sample::select, sample::Index, strategy::ValueTree as _,
    test_runner::TestRunner,
};
use saluki_metric_plan::{
    canonical::{strip_resolved_values, PlanHash},
    ir::{derived_terminal_id, Blobs, LocalId, NodeId, PolicyArtifact, PortName},
    testing::arb_predicate,
};
use serde_json::{json, Map, Value};

use super::*;

/// The outputs generated artifacts emit to.
const OUTPUTS: &[&str] = &["dd_metrics_encode", "metrics_routing_encode_0", "mrf_metrics_encode"];

/// The scope exits generated artifacts leave by.
const EXITS: &[&str] = &["aggregated", "passthrough"];

/// Strings that exercise escaping and the syntax's own punctuation.
const TRICKY_STRINGS: &[&str] = &[
    "",
    "plain",
    "quote\"d",
    "back\\slash",
    "line\nbreak\r\ttab",
    "é ✓ \u{1F600}",
    "\u{0}\u{1}\u{7f}\u{85}",
    "\u{202E}\u{200B}\u{FEFF}",
    "# not a comment",
    "-> emit drop",
    "{ a: [1] }",
    "config(\"x\")",
    "10s",
];

const DURATIONS: &[&str] = &["0s", "10s", "15s", "1m", "500ms", "2h", "1700000300s"];

const BINDINGS: &[&str] = &["dogstatsd.prefix_filter.metric_namespace", "a.b", "x_1.y_2.z0"];

/// The shape of the part of the graph that one edge leads to.
#[derive(Clone, Debug)]
enum Shape {
    /// An `emit`, named by the derived-ID rule when `inline`, and by a fresh ID otherwise.
    Emit { output: usize, inline: bool },
    /// A `drop`, named like an `emit`.
    Drop { inline: bool },
    /// An edge out of the scope, into one of its exits.
    Exit { exit: usize },
    /// A `split`, and what each of its ports leads to.
    Split {
        predicate: Value,
        on_true: Box<Shape>,
        on_false: Box<Shape>,
    },
    /// A `tee`, and what each of its ports leads to.
    Tee(Vec<Shape>),
    /// A `timestamp_split`, and what each of its ports leads to.
    TimestampSplit {
        window: String,
        default: Box<Shape>,
        passthrough: Box<Shape>,
    },
    /// A node with one input and one output.
    Chain {
        op: &'static str,
        params: Value,
        next: Box<Shape>,
    },
    /// A `split` whose ports rejoin in a `merge`, its `true` port through an optional chain node.
    Diamond {
        predicate: Value,
        via: Option<(&'static str, Value)>,
        next: Box<Shape>,
    },
}

/// What a generated artifact's scope inputs lead to.
#[derive(Clone, Debug)]
enum Inputs {
    /// One input, `metrics`.
    One(Shape),
    /// Two inputs, `a` and `b`, each leading to its own part of the graph.
    Two(Shape, Shape),
    /// Two inputs, `a` and `b`, joined by a `merge`.
    Merged(Shape),
}

fn arb_string() -> impl Strategy<Value = String> {
    // Boxed, since parameters hold many strings: see `arb_chain`.
    prop_oneof![
        select(TRICKY_STRINGS).prop_map(str::to_string),
        "[a-z.]{0,12}",
        any::<String>(),
    ]
    .boxed()
}

fn arb_strings() -> impl Strategy<Value = Value> {
    vec(arb_string(), 0..3).prop_map(|strings| json!(strings))
}

/// Returns a strategy for a value that is a binding, or else a literal from `literal`.
fn bindable<S>(literal: S) -> impl Strategy<Value = Value>
where
    S: Strategy<Value = Value> + 'static,
{
    prop_oneof![
        3 => literal.boxed(),
        1 => select(BINDINGS).prop_map(|binding| json!({"config": binding})),
    ]
}

fn arb_duration() -> impl Strategy<Value = String> {
    select(DURATIONS).prop_map(str::to_string)
}

fn arb_predicate_value() -> impl Strategy<Value = Value> {
    // Artifacts carry no resolved values.
    arb_predicate().prop_map(|predicate| {
        let mut value = serde_json::to_value(predicate).expect("predicates serialize");
        strip_resolved_values(&mut value);
        value
    })
}

/// Returns a strategy for a node with one input and one output, with parameters that its operator's schema accepts.
///
/// Its arms are boxed, as are those of the other unions of large strategies here: proptest keeps a union's value trees
/// inline, so in a debug build an unboxed union takes a stack frame several times the size of all its arms' trees, and
/// a recursive strategy takes such a frame at each level. Unboxed, generating a deep shape took more than the 2 MiB
/// stack of a test thread.
fn arb_chain() -> impl Strategy<Value = (&'static str, Value)> {
    let count = || (0..100_000i64).prop_map(Value::from);
    let tag_rule = (arb_string(), select(&["include", "exclude"][..]), arb_strings())
        .prop_map(|(metric_name, action, tags)| json!({"metric_name": metric_name, "action": action, "tags": tags}));
    let value_rule = (arb_string(), arb_string(), arb_strings(), option::of(arb_string())).prop_map(
        |(metric_prefix, tag_name, values, replacement)| {
            let mut rule = json!({"metric_prefix": metric_prefix, "tag_name": tag_name, "values": values});
            match replacement {
                Some(replacement) => {
                    rule["on_miss"] = json!("replace");
                    rule["replacement"] = json!(replacement);
                }
                None => rule["on_miss"] = json!("remove"),
            }
            rule
        },
    );
    let mapping = (arb_string(), select(&["wildcard", "regex"][..]), arb_string(), arb_string(), arb_string())
        .prop_map(|(pattern, match_type, name, tag, value)| {
            json!({"match": pattern, "match_type": match_type, "name": name, "tags": [{"name": tag, "value": value}]})
        });
    let profile = (arb_string(), arb_string(), vec(mapping, 0..2))
        .prop_map(|(name, prefix, mappings)| json!({"name": name, "prefix": prefix, "mappings": mappings}));
    prop_oneof![
        Just(("host_enrichment", json!({}))).boxed(),
        (bindable(arb_string().prop_map(Value::from)), bindable(arb_strings()))
            .prop_map(|(namespace, exclusions)| (
                "namespace_prefix",
                json!({
                    "namespace": namespace,
                    "exclusions": exclusions,
                })
            ))
            .boxed(),
        (arb_strings(), arb_duration())
            .prop_map(|(tags, deadline)| ("host_tags", json!({"tags": tags, "deadline": deadline})))
            .boxed(),
        (
            bindable(vec(tag_rule, 0..2).prop_map(Value::from)),
            bindable(vec(value_rule, 0..2).prop_map(Value::from)),
            bindable(count()),
        )
            .prop_map(|(rules, value_rules, cache_capacity)| (
                "tag_projection",
                json!({
                    "rules": rules,
                    "value_rules": value_rules,
                    "cache_capacity": cache_capacity,
                })
            ))
            .boxed(),
        (
            bindable(arb_duration().prop_map(Value::from)),
            bindable(arb_duration().prop_map(Value::from)),
            bindable(count()),
            any::<bool>(),
            vec(0..=100i64, 0..3),
        )
            .prop_map(|(window, flush_interval, context_limit, flag, percentiles)| (
                "aggregate",
                json!({
                    "window": window,
                    "flush_interval": flush_interval,
                    "context_limit": context_limit,
                    "counter_expiry": "0s",
                    "flush_open_windows": flag,
                    "histogram_aggregates": ["max", "median"],
                    "histogram_percentiles": percentiles,
                    "copy_to_distribution": !flag,
                    "copy_to_distribution_prefix": "dist.",
                })
            ))
            .boxed(),
        (bindable(vec(profile, 0..2).prop_map(Value::from)), count(), count())
            .prop_map(|(profiles, cache_size, interner)| (
                "dogstatsd_mapper",
                json!({
                    "profiles": profiles,
                    "cache_size": cache_size,
                    "string_interner_bytes": interner,
                })
            ))
            .boxed(),
    ]
}

fn arb_shape() -> impl Strategy<Value = Shape> {
    let leaf = prop_oneof![
        (0..OUTPUTS.len(), any::<bool>()).prop_map(|(output, inline)| Shape::Emit { output, inline }),
        any::<bool>().prop_map(|inline| Shape::Drop { inline }),
        (0..EXITS.len()).prop_map(|exit| Shape::Exit { exit }),
    ];
    leaf.prop_recursive(4, 24, 3, |inner| {
        // Each arm is boxed, as `arb_chain` explains, or every level of a deep shape takes a frame of hundreds of
        // kilobytes in a debug build.
        prop_oneof![
            (arb_predicate_value(), inner.clone(), inner.clone())
                .prop_map(|(predicate, on_true, on_false)| {
                    Shape::Split {
                        predicate,
                        on_true: Box::new(on_true),
                        on_false: Box::new(on_false),
                    }
                })
                .boxed(),
            vec(inner.clone(), 1..4).prop_map(Shape::Tee).boxed(),
            (arb_duration(), inner.clone(), inner.clone())
                .prop_map(|(window, default, passthrough)| {
                    Shape::TimestampSplit {
                        window,
                        default: Box::new(default),
                        passthrough: Box::new(passthrough),
                    }
                })
                .boxed(),
            (arb_chain(), inner.clone())
                .prop_map(|((op, params), next)| Shape::Chain {
                    op,
                    params,
                    next: Box::new(next),
                })
                .boxed(),
            (arb_predicate_value(), option::of(arb_chain()), inner)
                .prop_map(|(predicate, via, next)| {
                    Shape::Diamond {
                        predicate,
                        via,
                        next: Box::new(next),
                    }
                })
                .boxed(),
        ]
    })
}

fn arb_inputs() -> impl Strategy<Value = Inputs> {
    prop_oneof![
        2 => arb_shape().prop_map(Inputs::One),
        1 => (arb_shape(), arb_shape()).prop_map(|(a, b)| Inputs::Two(a, b)),
        1 => arb_shape().prop_map(Inputs::Merged),
    ]
}

/// Builds an artifact document from shapes, writing nodes and edges in the order a depth-first traversal from the
/// scope inputs reaches them, the order lowering writes plans in.
///
/// This is written independently of the crate's own ordering: it follows the shapes, and knows where each shape's
/// edges complete a node's inputs.
#[derive(Default)]
struct Builder {
    nodes: Vec<Value>,
    edges: Vec<Value>,
    emitted: Vec<&'static str>,
    next_id: usize,
}

impl Builder {
    fn fresh(&mut self) -> String {
        self.next_id += 1;
        format!("n{}", self.next_id)
    }

    fn edge(&mut self, from: &str, to: &str) {
        self.edges.push(json!({"from": from, "to": to}));
    }

    fn node(&mut self, id: &str, op: &str, params: Value) {
        let mut node = json!({"id": id, "op": op});
        if params.as_object().is_some_and(|params| !params.is_empty()) {
            node["params"] = params;
        }
        self.nodes.push(node);
    }

    /// Adds the edge from port `port` of `parent` (which is `input` for a scope input) and the nodes it leads to.
    fn attach(&mut self, parent: &str, port: &str, shape: &Shape) {
        let from = format!("{}.{}", parent, port);
        let terminal = |builder: &mut Self, inline: bool| {
            if inline {
                let parent = NodeId::reserved(LocalId::new(parent).unwrap());
                derived_terminal_id(&parent, &PortName::new(port).unwrap()).to_string()
            } else {
                builder.fresh()
            }
        };
        match shape {
            Shape::Emit { output, inline } => {
                let id = terminal(self, *inline);
                self.edge(&from, &format!("{}.in", id));
                self.node(&id, "emit", json!({"output": OUTPUTS[*output]}));
                self.emitted.push(OUTPUTS[*output]);
            }
            Shape::Drop { inline } => {
                let id = terminal(self, *inline);
                self.edge(&from, &format!("{}.in", id));
                self.node(&id, "drop", json!({}));
            }
            Shape::Exit { exit } => self.edge(&from, &format!("exit.{}", EXITS[*exit])),
            Shape::Split {
                predicate,
                on_true,
                on_false,
            } => {
                let id = self.fresh();
                self.edge(&from, &format!("{}.in", id));
                self.node(&id, "split", json!({"predicate": predicate}));
                self.attach(&id, "true", on_true);
                self.attach(&id, "false", on_false);
            }
            Shape::Tee(branches) => {
                let id = self.fresh();
                self.edge(&from, &format!("{}.in", id));
                self.node(&id, "tee", json!({"ports": branches.len()}));
                for (i, branch) in branches.iter().enumerate() {
                    self.attach(&id, &i.to_string(), branch);
                }
            }
            Shape::TimestampSplit {
                window,
                default,
                passthrough,
            } => {
                let id = self.fresh();
                self.edge(&from, &format!("{}.in", id));
                self.node(&id, "timestamp_split", json!({"window": window}));
                self.attach(&id, "default", default);
                self.attach(&id, "passthrough", passthrough);
            }
            Shape::Chain { op, params, next } => {
                let id = self.fresh();
                self.edge(&from, &format!("{}.in", id));
                self.node(&id, op, params.clone());
                self.attach(&id, "out", next);
            }
            Shape::Diamond { predicate, via, next } => {
                let split = self.fresh();
                let merge = self.fresh();
                self.edge(&from, &format!("{}.in", split));
                self.node(&split, "split", json!({"predicate": predicate}));
                match via {
                    Some((op, params)) => {
                        let via = self.fresh();
                        self.edge(&format!("{}.true", split), &format!("{}.in", via));
                        self.node(&via, op, params.clone());
                        self.edge(&format!("{}.out", via), &format!("{}.0", merge));
                    }
                    None => self.edge(&format!("{}.true", split), &format!("{}.0", merge)),
                }
                // The `false` edge is the merge's last input, so the merge comes next.
                self.edge(&format!("{}.false", split), &format!("{}.1", merge));
                self.node(&merge, "merge", json!({"inputs": 2}));
                self.attach(&merge, "out", next);
            }
        }
    }
}

prop_compose! {
    fn arb_artifact()(
        inputs in arb_inputs(),
        scope in select(&["post_enrich", "other_scope"][..]),
        program_hash in any::<[u8; 32]>(),
        fingerprints in vec(any::<[u8; 32]>(), OUTPUTS.len()),
        blob in option::of(arb_strings()),
    ) -> PolicyArtifact {
        let mut builder = Builder::default();
        match &inputs {
            Inputs::One(shape) => builder.attach("input", "metrics", shape),
            Inputs::Two(a, b) => {
                builder.attach("input", "a", a);
                builder.attach("input", "b", b);
            }
            Inputs::Merged(next) => {
                let merge = builder.fresh();
                builder.edge("input.a", &format!("{}.0", merge));
                builder.edge("input.b", &format!("{}.1", merge));
                builder.node(&merge, "merge", json!({"inputs": 2}));
                builder.attach(&merge, "out", next);
            }
        }

        let mut outputs = Map::new();
        for output in &builder.emitted {
            let index = OUTPUTS.iter().position(|o| o == output).unwrap();
            outputs.insert(output.to_string(), json!(PlanHash::from_digest(fingerprints[index]).to_string()));
        }
        let mut document = json!({
            "ir_version": 1,
            "scope": scope,
            "base": {"program_hash": PlanHash::from_digest(program_hash).to_string(), "outputs": outputs},
            "nodes": builder.nodes,
            "edges": builder.edges,
        });
        if let Some(blob) = blob {
            let mut blobs = Blobs::default();
            blobs.insert(blob).unwrap();
            document["blobs"] = serde_json::to_value(&blobs).unwrap();
            // Refer to the blob from a parameter, as an artifact sharing a long list would.
            let hash = blobs.iter().next().unwrap().0.to_string();
            document["nodes"].as_array_mut().unwrap().push(json!({
                "id": "shared", "op": "host_tags", "params": {"tags": {"blob": hash}, "deadline": "0s"},
            }));
        }
        PolicyArtifact::from_value(document).expect("generated artifacts are valid artifact documents")
    }
}

/// Words, literals, and punctuation of the grammar, for generating sources that are nearly valid.
const TOKENS: &[&str] = &[
    "policy",
    "p",
    "{",
    "}",
    "base",
    "\"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"",
    "outputs",
    "blobs",
    "ir_version",
    "node",
    "n",
    "m",
    "=",
    "split",
    "tee",
    "merge",
    "emit",
    "drop",
    "(",
    ")",
    "->",
    ".",
    "input",
    "metrics",
    "true",
    "false",
    "null",
    "[",
    "]",
    ",",
    ":",
    "\"s\"",
    "\"",
    "\\",
    "1",
    "-1",
    "1.5",
    "1.0715660391465826e-75",
    "{ all: [true] }",
    "10s",
    "not",
    "and",
    "or",
    "kind",
    "in",
    "counter",
    "name",
    "host",
    "is",
    "unset",
    "==",
    "starts_with",
    "prefix_in",
    "matches",
    "values",
    "prefix",
    "tag",
    "has_tag",
    "flag",
    "is_empty",
    "all",
    "any",
    "config",
    "blob",
    "#",
    "\n",
    "0",
    "out",
    "ports",
    "inputs",
    "output",
    "predicate",
    "é",
    "\u{202E}",
];

/// Returns a strategy for sources that are random sequences of grammar tokens, with or without a valid opening.
fn arb_token_soup() -> impl Strategy<Value = String> {
    (any::<bool>(), vec(select(TOKENS), 0..80)).prop_map(|(opened, tokens)| {
        let soup = tokens.join(" ");
        if opened {
            format!("policy p {{ base \"{}\" {} }}", BASE, soup)
        } else {
            soup
        }
    })
}

/// Returns a strategy for printed artifacts truncated, cut, or with a token inserted, at character boundaries.
fn arb_damaged_source() -> impl Strategy<Value = String> {
    (arb_artifact(), any::<Index>(), any::<Index>(), 0..3u8, select(TOKENS)).prop_map(
        |(artifact, at, until, damage, token)| {
            let text = print_default(&artifact);
            let boundaries = text
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(text.len()))
                .collect::<Vec<_>>();
            let at = boundaries[at.index(boundaries.len())];
            let until = boundaries[until.index(boundaries.len())].max(at);
            match damage {
                0 => text[..at].to_string(),
                1 => format!("{}{}", &text[..at], &text[until..]),
                _ => format!("{} {} {}", &text[..at], token, &text[at..]),
            }
        },
    )
}

/// Returns the line and column of a byte offset, computed directly.
fn line_and_column(source: &str, offset: usize) -> (usize, usize) {
    let before = &source[..offset];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |newline| newline + 1);
    (line, source[line_start..offset].chars().count() + 1)
}

/// Generating the property tests' inputs and checking them fits in a small stack.
///
/// In a debug build, generating a value moves proptest's value trees through a stack frame at every level of its
/// strategy, and an unboxed union of large strategies takes hundreds of kilobytes at each level of `arb_shape`: that
/// took more than a test thread's 2 MiB, and aborted the property tests now and then. Unlike them, this draws the same
/// inputs on every run.
#[test]
fn generating_and_checking_property_inputs_fits_in_a_small_stack() {
    on_small_stack(|| {
        let mut runner = TestRunner::deterministic();
        let artifacts = arb_artifact();
        let sources = prop_oneof![any::<String>(), arb_token_soup(), arb_damaged_source()];
        for _ in 0..256 {
            let artifact = artifacts.new_tree(&mut runner).expect("artifacts generate").current();
            let printed = print_default(&artifact);
            assert_eq!(parse_ok(&printed).artifact, artifact, "{}", printed);

            let source = sources.new_tree(&mut runner).expect("sources generate").current();
            if let Ok(parsed) = parse(&source) {
                let printed = print_default(&parsed.artifact);
                assert_eq!(parse_ok(&printed).artifact, parsed.artifact, "{}", printed);
            }
        }
    });
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn property_test_printed_artifacts_parse_back_to_themselves(artifact in arb_artifact()) {
        let printed = print_default(&artifact);
        let parsed = match parse(&printed) {
            Ok(parsed) => parsed,
            Err(diagnostics) => return Err(TestCaseError::fail(format!("{}\n---\n{}", diagnostics, printed))),
        };
        prop_assert_eq!(&parsed.artifact, &artifact, "{}", printed);
        // Printing what the text parses to gives the text back.
        prop_assert_eq!(print_default(&parsed.artifact), printed.clone());
        // The text has fewer tokens than the artifact's JSON has bytes, which keeps the token limit above the text of
        // any artifact within the size limit.
        let tokens = crate::lexer::lex(&printed).map(|tokens| tokens.len()).unwrap_or(usize::MAX);
        let json = serde_json::to_vec(&artifact).unwrap().len();
        prop_assert!(tokens < json, "{} tokens, {} bytes of JSON", tokens, json);

        // Every node and edge has a span, and each node's span is its definition or its inline terminal.
        for node in &artifact.nodes {
            let span = parsed.spans.node(node.id.as_str());
            prop_assert!(span.is_some(), "{} has no span", node.id);
            let written = text(&printed, span.unwrap());
            let defined = written.starts_with(&format!("node {} = {}", node.id, node.op));
            let inline = written == "drop" || written.starts_with("emit ");
            prop_assert!(defined || inline, "{}: {:?}", node.id, written);
        }
        prop_assert!(parsed.spans.edge(artifact.edges.len().saturating_sub(1)).is_some() || artifact.edges.is_empty());
        prop_assert!(parsed.spans.edge(artifact.edges.len()).is_none());

        // Without fingerprints, the text parses to the artifact without them.
        let elided = print(&artifact, &PrintOptions { elide_fingerprints: true, ..PrintOptions::default() });
        let mut without = artifact.clone();
        without.base.outputs.clear();
        prop_assert_eq!(parse(&elided).map(|parsed| parsed.artifact), Ok(without));
    }

    #[test]
    fn property_test_printing_and_parsing_depend_only_on_the_graph(
        (artifact, nodes, edges) in arb_artifact().prop_flat_map(|artifact| {
            let nodes = Just(artifact.nodes.clone()).prop_shuffle();
            let edges = Just(artifact.edges.clone()).prop_shuffle();
            (Just(artifact), nodes, edges)
        }),
    ) {
        // Generated artifacts are in the canonical order, the order lowering writes. The same graph in any other order
        // prints the same text, which parses to the artifact in canonical order.
        let mut shuffled = artifact.clone();
        shuffled.nodes = nodes;
        shuffled.edges = edges;
        let printed = print_default(&shuffled);
        prop_assert_eq!(&printed, &print_default(&artifact));
        prop_assert_eq!(parse(&printed).map(|parsed| parsed.artifact), Ok(artifact));
    }

    #[test]
    fn property_test_numbers_print_and_parse_back_exactly(
        float in any::<f64>().prop_filter("finite", |float| float.is_finite()),
        signed in any::<i64>(),
        unsigned in any::<u64>(),
    ) {
        let values = json!([float, signed, unsigned]);
        let document = json!({
            "ir_version": 1,
            "scope": "post_enrich",
            "base": {"program_hash": BASE},
            "nodes": [{"id": "n", "op": "frobnicate", "params": {"v": values}}],
            "edges": [],
        });
        let artifact = PolicyArtifact::from_value(document).unwrap();
        let printed = print_default(&artifact);
        let parsed = parse(&printed).map(|parsed| parsed.artifact);
        prop_assert_eq!(parsed.as_ref(), Ok(&artifact), "{}", printed);
        let float_bits = |artifact: &PolicyArtifact| {
            artifact.nodes[0].params.get("v").unwrap()[0]
                .as_f64()
                .map(f64::to_bits)
        };
        prop_assert_eq!(parsed.as_ref().map(float_bits).ok().flatten(), Some(float.to_bits()));
    }

    #[test]
    fn property_test_any_input_parses_or_is_diagnosed_without_panicking(
        source in prop_oneof![any::<String>(), arb_token_soup(), arb_damaged_source()],
    ) {
        match parse(&source) {
            Ok(parsed) => {
                // Whatever parses prints to a fixed point.
                let printed = print_default(&parsed.artifact);
                let reparsed = parse(&printed).map(|parsed| parsed.artifact);
                prop_assert_eq!(reparsed.as_ref(), Ok(&parsed.artifact), "{}", printed);
                if let Ok(reparsed) = &reparsed {
                    prop_assert_eq!(print_default(reparsed), printed);
                }
            }
            Err(diagnostics) => {
                prop_assert!(!diagnostics.is_empty());
                for diagnostic in &diagnostics {
                    let span = diagnostic.span;
                    prop_assert!(span.start <= span.end && span.end <= source.len(), "{:?}", diagnostic);
                    prop_assert!(source.is_char_boundary(span.start) && source.is_char_boundary(span.end));
                    prop_assert_eq!((diagnostic.line, diagnostic.column), line_and_column(&source, span.start));
                    prop_assert!(!diagnostic.message.is_empty());
                }
            }
        }
    }
}
