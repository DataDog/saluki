//! Property tests: printing and parsing are inverse over generated artifacts, printing is total and independent of the
//! order an artifact lists its graph in, and no input makes parsing panic.
//!
//! Three generators make artifacts:
//!
//! - [`arb_graph`] makes well-formed graphs a step at a time, from open streams: every node of the registry but
//!   `source` (which has no input, so no well-formed artifact has one), with rich parameters and predicates, merges,
//!   reused streams, aliases, `tee`s in the core form, `where`s, splits in both polarities, nodes with several output ports,
//!   and node IDs that are often the ones lowering derives and otherwise need a `let` or an `as`.
//! - [`arb_tree_artifact`] makes well-formed graphs from recursive shapes, with fresh node IDs, written in canonical
//!   order by a builder independent of the crate's.
//! - [`arb_arbitrary_artifact`] makes small graphs with arbitrary edges, mostly not well-formed.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use proptest::{
    collection::vec, option, prelude::*, sample::select, sample::Index, strategy::ValueTree as _,
    test_runner::TestRunner,
};
use saluki_metric_plan::{
    canonical::strip_resolved_values,
    ir::{
        derived_terminal_id, ArtifactNodeId, ArtifactPortRef, Blobs, LocalId, NodeId, OperatorName, PolicyArtifact,
        PortName,
    },
    normalize::normalize_artifact,
    operators::OperatorRegistry,
    testing::arb_predicate,
};
use serde_json::{json, Map, Value};

use super::*;
use crate::{graph::NodeShape, ids};

/// The outputs generated artifacts emit to.
const OUTPUTS: &[&str] = &["dd_metrics_encode", "metrics_routing_encode_0", "mrf_metrics_encode"];

/// The sources of graphs that [`arb_graph`] makes.
const INPUTS: &[&str] = &["dogstatsd", "otlp", "checks"];

/// Words the grammar gives a meaning somewhere, as node IDs and so as the names of streams.
const WORDS: &[&str] = &[
    "let",
    "as",
    "where",
    "fork",
    "emit",
    "drop",
    "exit",
    "output",
    "policy",
    "base",
    "blobs",
    "ir_version",
    "not",
    "and",
    "true",
    "null",
    "config",
    "blob",
    "tags",
    "kind",
    "host",
    "is",
    "node",
    "input_otlp",
];

/// Recipients of `emit`, including names that must be quoted: keywords, URLs, and the empty string.
const RECIPIENTS: &[&str] = &[
    "primary",
    "secondary",
    "https://b.example.com",
    "output",
    "as",
    "drop",
    "x y",
    "",
];

/// The output ports of the `opaque_host` nodes that [`arb_graph`] makes.
const OPAQUE_PORTS: &[&str] = &["a", "b", "out", "left", "true", "1"];

/// Unregistered operators, whose ports are unknown to the syntax.
const UNREGISTERED: &[&str] = &[
    "alpha", "beta", "alpha", "beta", "where", "fork", "emit", "exit", "let", "as", "output",
];

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
    "| where x != y",
    "{ a: [1] }",
    "config(\"x\")",
    "config.a.b",
    "10s",
];

const DURATIONS: &[&str] = &["0s", "10s", "15s", "1m", "500ms", "2h", "1700000300s"];

const BINDINGS: &[&str] = &["dogstatsd.prefix_filter.metric_namespace", "a.b", "x_1.y_2.z0"];

// Values and parameters.

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

/// Returns a strategy for any JSON value, as an unregistered operator's parameter may be: written without a schema.
fn arb_json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<f64>()
            .prop_filter("finite", |float| float.is_finite())
            .prop_map(Value::from),
        arb_string().prop_map(Value::from),
        select(DURATIONS).prop_map(Value::from),
        select(BINDINGS).prop_map(|binding| json!({"config": binding})),
    ];
    leaf.prop_recursive(3, 12, 3, |inner| {
        prop_oneof![
            vec(inner.clone(), 0..3).prop_map(Value::from).boxed(),
            vec((arb_string(), inner), 0..3)
                .prop_map(|members| Value::Object(members.into_iter().collect::<Map<_, _>>()))
                .boxed(),
        ]
    })
    .boxed()
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
        (bindable(arb_strings()), bindable(arb_duration().prop_map(Value::from)))
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
        // An `emit` or `drop` with parameters that do not configure it, whose ports are unknown, so that the syntax
        // takes it to have the conventional ports `in` and `out`, and writes it in the core form.
        (select(OUTPUTS), vec(select(RECIPIENTS), 0..2))
            .prop_map(|(output, to)| ("emit", json!({"output": output, "to": to})))
            .boxed(),
        Just(("drop", json!({"v": 1}))).boxed(),
        // An operator the registry does not know, with parameters read without a schema.
        (select(UNREGISTERED), option::of(arb_json()))
            .prop_map(|(op, value)| (op, value.map_or_else(|| json!({}), |value| json!({"v": value}))))
            .boxed(),
    ]
}

/// Returns a strategy for a node with one input and one output, as [`arb_chain`] makes, now and then with one
/// parameter replaced by any value, which its operator's schema may not accept: the syntax writes it as it is.
fn arb_noisy_chain() -> impl Strategy<Value = (&'static str, Value)> {
    (arb_chain(), option::weighted(0.15, (any::<Index>(), arb_json())))
        .prop_map(|((op, mut params), noise)| {
            if let (Some((at, value)), Some(members)) = (noise, params.as_object_mut()) {
                if !members.is_empty() {
                    let key = members.keys().nth(at.index(members.len())).unwrap().clone();
                    members.insert(key, value);
                }
            }
            (op, params)
        })
        .boxed()
}

/// Returns the parameters of an `opaque_host` with the given output ports.
fn opaque_params(ports: &[&str]) -> Value {
    let any = json!({"kinds": "all", "timestamps": "mixed"});
    let outputs = ports
        .iter()
        .map(|port| (port.to_string(), any.clone()))
        .collect::<Map<_, _>>();
    json!({"component": "metrics_gateway", "input": any, "outputs": outputs})
}

/// Returns the headers of a generated artifact: everything but its graph.
#[derive(Clone, Debug)]
struct Headers {
    ir_version: u32,
    blob: Option<Value>,
}

fn arb_headers() -> impl Strategy<Value = Headers> {
    (
        // Reading an artifact document accepts only the current IR version: see `arb_arbitrary_artifact`.
        Just(1u32),
        // A blob is canonical JSON, whose numbers are integers within the range every reader keeps exactly.
        option::of(prop_oneof![arb_strings().boxed(), arb_json()])
            .prop_map(|blob| blob.filter(|blob| Blobs::default().insert(blob.clone()).is_ok())),
    )
        .prop_map(|(ir_version, blob)| Headers { ir_version, blob })
}

/// Returns the artifact document of a graph, with `headers`.
fn document(headers: &Headers, nodes: Vec<Value>, edges: Vec<Value>) -> Value {
    let mut document = json!({
        "ir_version": headers.ir_version,
        "nodes": nodes,
        "edges": edges,
    });
    if let Some(blob) = &headers.blob {
        let mut blobs = Blobs::default();
        blobs.insert(blob.clone()).unwrap();
        document["blobs"] = serde_json::to_value(&blobs).unwrap();
    }
    document
}

/// Returns the hash of a generated artifact's blob, if it has one, as a parameter refers to it.
fn blob_ref(headers: &Headers) -> Option<Value> {
    let blob = headers.blob.as_ref()?;
    let mut blobs = Blobs::default();
    blobs.insert(blob.clone()).unwrap();
    let hash = blobs.iter().next().unwrap().0.to_string();
    Some(json!({"blob": hash}))
}

// Well-formed graphs, a step at a time.

/// What a step attaches to an open stream.
#[derive(Clone, Debug)]
enum Action {
    /// A node with one input and one output.
    Chain,
    /// Two or three nodes with one input and one output, with the IDs of `let s = <head> | a | b`, as a person who
    /// named an intermediate stream writes them.
    Run,
    /// A `split` whose `false` port leads to a `drop`: a `where` when the drop has the derived ID.
    Where,
    /// A `split` with both ports left open, for later steps.
    Split,
    /// A `tee` with one to three ports.
    Tee,
    /// A `tee` with two or three ports and the ID of the implicit tee after the stream: the stream, reused.
    Reuse,
    /// A `tee` with two or three ports named `<name>_fanout` for a fresh name: an alias of the stream, reused.
    Alias,
    /// A `timestamp_split`.
    TimestampSplit,
    /// An `opaque_host` with one to three output ports.
    Opaque,
    /// A `merge` of this stream and one or two others.
    Merge,
    /// A sink.
    End,
}

/// How a stream ends.
#[derive(Clone, Debug)]
enum SinkChoice {
    Output(usize),
    Recipients(Vec<&'static str>),
    Drop,
}

fn arb_sink() -> impl Strategy<Value = SinkChoice> {
    prop_oneof![
        3 => (0..OUTPUTS.len()).prop_map(SinkChoice::Output),
        1 => vec(select(RECIPIENTS), 1..3).prop_map(SinkChoice::Recipients),
        3 => Just(SinkChoice::Drop),
    ]
    .boxed()
}

/// One step of building a graph: what to attach to which open stream, and how to name it.
#[derive(Clone, Debug)]
struct Step {
    action: Action,
    /// The open stream to attach to.
    pick: Index,
    /// The other open streams a merge takes.
    others: (Index, Index),
    /// How to name the nodes: by the derived-ID rule for some position, or with a fresh ID.
    naming: u8,
    /// A count: of ports, of inputs, or of stages.
    count: u8,
    chain: (&'static str, Value),
    second: (&'static str, Value),
    predicate: Value,
    window: Value,
    sink: SinkChoice,
}

fn arb_action() -> impl Strategy<Value = Action> {
    prop_oneof![
        4 => Just(Action::Chain),
        1 => Just(Action::Run),
        3 => Just(Action::Where),
        1 => Just(Action::Split),
        2 => Just(Action::Tee),
        2 => Just(Action::Reuse),
        2 => Just(Action::Alias),
        1 => Just(Action::TimestampSplit),
        1 => Just(Action::Opaque),
        2 => Just(Action::Merge),
        2 => Just(Action::End),
    ]
}

fn arb_step() -> impl Strategy<Value = Step> {
    (
        arb_action(),
        any::<Index>(),
        any::<(Index, Index)>(),
        any::<u8>(),
        any::<u8>(),
        arb_noisy_chain(),
        arb_noisy_chain(),
        // Any value, apart from a bare boolean, which has no spelling where a predicate is expected but the constant.
        prop_oneof![
            9 => arb_predicate_value().boxed(),
            1 => arb_json().prop_map(|value| if value.is_boolean() { Value::Null } else { value }).boxed(),
        ],
        prop_oneof![9 => bindable(arb_duration().prop_map(Value::from)).boxed(), 1 => arb_json()],
        arb_sink(),
    )
        .prop_map(
            |(action, pick, others, naming, count, chain, second, predicate, window, sink)| Step {
                action,
                pick,
                others,
                naming,
                count,
                chain,
                second,
                predicate,
                window,
                sink,
            },
        )
        .boxed()
}

/// A graph described by the steps that build it.
#[derive(Clone, Debug)]
struct GraphPlan {
    inputs: usize,
    steps: Vec<Step>,
    /// How each stream still open after the steps ends, and how its sink is named.
    closers: Vec<(SinkChoice, u8)>,
    headers: Headers,
}

fn arb_graph_plan() -> impl Strategy<Value = GraphPlan> {
    (
        1..=INPUTS.len(),
        vec(arb_step(), 0..14),
        vec((arb_sink(), any::<u8>()), 0..8),
        arb_headers(),
    )
        .prop_map(|(inputs, steps, closers, headers)| GraphPlan {
            inputs,
            steps,
            closers,
            headers,
        })
}

/// A stream that no edge leaves yet.
#[derive(Clone, Debug)]
struct Open {
    /// The port, as an edge's `from`.
    from: String,
    /// The node it leaves, or `input`.
    parent: String,
    port: String,
    /// Whether the node it leaves has several output ports, so that a reference to it writes the port.
    multi: bool,
    /// The anchor that stages following it most likely derive their IDs from, with the stages counted under it.
    anchor: String,
    ordinals: BTreeMap<String, usize>,
}

impl Open {
    /// Returns the open stream that a node with one output port and the ID `id` leaves after this one, its stages
    /// counted under the same anchor.
    fn after(&self, id: &str, port: &str, op: &str) -> Self {
        let mut ordinals = self.ordinals.clone();
        *ordinals.entry(op.to_string()).or_default() += 1;
        Self {
            from: format!("{}.{}", id, port),
            parent: id.to_string(),
            port: port.to_string(),
            multi: false,
            anchor: self.anchor.clone(),
            ordinals,
        }
    }

    /// Returns the open stream at `port` of the node `id`, which has several output ports.
    fn port_of(id: &str, port: &str) -> Self {
        Self {
            from: format!("{}.{}", id, port),
            parent: id.to_string(),
            port: port.to_string(),
            multi: true,
            anchor: ids::port_anchor(id, Some(&PortName::new(port).unwrap())).to_string(),
            ordinals: BTreeMap::new(),
        }
    }

    /// Returns the ordinal the next stage running `op` under this stream's anchor has.
    fn ordinal(&self, op: &str) -> usize {
        self.ordinals.get(op).copied().unwrap_or(0) + 1
    }

    fn port_name(&self) -> PortName {
        PortName::new(self.port.as_str()).unwrap()
    }
}

/// Builds the graph a [`GraphPlan`] describes.
struct GraphBuilder<'h> {
    headers: &'h Headers,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    taken: HashSet<String>,
    open: Vec<Open>,
    fresh: usize,
}

impl GraphBuilder<'_> {
    /// Returns `candidate` if it is a free node ID, and otherwise a fresh one. Every fourth fresh ID is long, so that
    /// IDs derived from it are shortened.
    fn id(&mut self, candidate: Option<ArtifactNodeId>) -> String {
        if let Some(candidate) = candidate {
            let candidate = candidate.to_string();
            if self.taken.insert(candidate.clone()) {
                return candidate;
            }
        }
        loop {
            self.fresh += 1;
            let id = if self.fresh.is_multiple_of(4) {
                format!("{}{}", "l".repeat(56), self.fresh)
            } else {
                format!("n{}", self.fresh)
            };
            if self.taken.insert(id.clone()) {
                return id;
            }
        }
    }

    /// Returns the ID a node running `op` after `open` is most likely derived as, by `naming`, or `None` for a fresh
    /// one.
    fn candidate(open: &Open, op: &str, naming: u8) -> Option<ArtifactNodeId> {
        let port = open.port_name();
        match naming % 10 {
            0 => None,
            // The name of a source, which may be one the artifact reads from.
            8 => ArtifactNodeId::new(INPUTS[usize::from(naming / 10) % INPUTS.len()]).ok(),
            // A word the grammar reserves somewhere.
            9 => ArtifactNodeId::new(WORDS[usize::from(naming / 10) % WORDS.len()]).ok(),
            1 => Some(ids::terminal(&open.parent, &port)),
            2 => Some(ids::stage(&open.parent, op, 1)),
            3 => Some(ids::fanout(
                &open.parent,
                (open.multi || open.parent == "input").then_some(&port),
            )),
            // The first ID of its operator under the anchor, which a repeated operator does not derive.
            4 => Some(ids::stage(&open.anchor, op, 1)),
            _ => Some(ids::stage(&open.anchor, op, open.ordinal(op))),
        }
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

    /// Returns the parameters of a chain node, with a `host_tags` node's tags in the blob sometimes.
    fn chain_params(&self, op: &str, params: &Value, naming: u8) -> Value {
        let mut params = params.clone();
        if op == "host_tags" && naming.is_multiple_of(2) {
            if let Some(blob) = blob_ref(self.headers) {
                params["tags"] = blob;
            }
        }
        params
    }

    /// Adds a node with one input and one output after `open`, returning the stream it leaves.
    fn chain(&mut self, open: &Open, op: &str, params: Value, id: String) -> Open {
        self.edge(&open.from, &format!("{}.in", id));
        self.node(&id, op, params);
        open.after(&id, "out", op)
    }

    /// Ends `open` as `sink` says, naming the sink by the derived-ID rule unless `naming` says otherwise.
    fn end(&mut self, open: &Open, sink: &SinkChoice, naming: u8) {
        let derived = (!naming.is_multiple_of(4)).then(|| ids::terminal(&open.parent, &open.port_name()));
        let (op, params) = match sink {
            SinkChoice::Output(output) => ("emit", json!({"output": OUTPUTS[*output]})),
            SinkChoice::Recipients(to) => ("emit", json!({"to": to})),
            SinkChoice::Drop => ("drop", json!({})),
        };
        let id = self.id(derived);
        self.edge(&open.from, &format!("{}.in", id));
        self.node(&id, op, params);
    }

    fn step(&mut self, step: &Step) {
        if self.open.is_empty() {
            return;
        }
        let open = self.open.remove(step.pick.index(self.open.len()));
        let naming = step.naming;
        match &step.action {
            Action::Chain => {
                let (op, params) = &step.chain;
                let id = self.id(Self::candidate(&open, op, naming));
                let params = self.chain_params(op, params, naming);
                let next = self.chain(&open, op, params, id);
                self.open.push(next);
            }
            Action::Run => {
                // `let s = <open> | a | b`: `a` is `s_a`, and `b` is `s`, after which the stream's anchor is `s`.
                let name = self.id(None);
                let (first, first_params) = &step.chain;
                let (second, second_params) = &step.second;
                let mut ops = vec![(*first, first_params)];
                if step.count.is_multiple_of(2) {
                    ops.push((*first, first_params));
                }
                ops.push((*second, second_params));
                let mut ordinals = ids::Ordinals::default();
                let mut at = open.clone();
                for (k, (op, params)) in ops.iter().enumerate() {
                    let ordinal = ordinals.next(op);
                    let id = if k + 1 == ops.len() {
                        name.clone()
                    } else {
                        self.id(Some(ids::stage(&name, op, ordinal)))
                    };
                    let params = self.chain_params(op, params, naming);
                    at = self.chain(&at, op, params, id);
                }
                at.anchor = name;
                at.ordinals.clear();
                self.open.push(at);
            }
            Action::Where => {
                let id = self.id(Self::candidate(&open, "split", naming));
                self.edge(&open.from, &format!("{}.in", id));
                self.node(&id, "split", json!({"predicate": step.predicate}));
                // The drop has the derived ID, unless the count says otherwise.
                let derived =
                    (!step.count.is_multiple_of(5)).then(|| ids::terminal(&id, &PortName::new("false").unwrap()));
                let drop = self.id(derived);
                self.edge(&format!("{}.false", id), &format!("{}.in", drop));
                self.node(&drop, "drop", json!({}));
                self.open.push(open.after(&id, "true", "split"));
            }
            Action::Split => {
                let id = self.id(Self::candidate(&open, "split", naming));
                self.edge(&open.from, &format!("{}.in", id));
                self.node(&id, "split", json!({"predicate": step.predicate}));
                self.open.push(Open::port_of(&id, "true"));
                self.open.push(Open::port_of(&id, "false"));
            }
            Action::Tee | Action::Reuse => {
                let reuse = matches!(step.action, Action::Reuse);
                let ports = if reuse {
                    2 + usize::from(step.count % 2)
                } else {
                    1 + usize::from(step.count % 3)
                };
                let id = self.id(Self::candidate(&open, "tee", if reuse { 3 } else { naming }));
                self.edge(&open.from, &format!("{}.in", id));
                self.node(&id, "tee", json!({"ports": ports}));
                for port in 0..ports {
                    self.open.push(Open::port_of(&id, &port.to_string()));
                }
            }
            Action::Alias => {
                // The name is taken, so that no node has it.
                let name = self.id(None);
                let id = self.id(Some(ids::fanout(&name, None)));
                let ports = 2 + usize::from(step.count % 2);
                self.edge(&open.from, &format!("{}.in", id));
                self.node(&id, "tee", json!({"ports": ports}));
                for port in 0..ports {
                    self.open.push(Open::port_of(&id, &port.to_string()));
                }
            }
            Action::TimestampSplit => {
                let id = self.id(Self::candidate(&open, "timestamp_split", naming));
                self.edge(&open.from, &format!("{}.in", id));
                self.node(&id, "timestamp_split", json!({"window": step.window}));
                self.open.push(Open::port_of(&id, "default"));
                self.open.push(Open::port_of(&id, "passthrough"));
            }
            Action::Opaque => {
                let count = 1 + usize::from(step.count % 3);
                let start = usize::from(step.count / 3) % OPAQUE_PORTS.len();
                let ports = (0..count)
                    .map(|k| OPAQUE_PORTS[(start + k) % OPAQUE_PORTS.len()])
                    .collect::<Vec<_>>();
                let id = self.id(Self::candidate(&open, "opaque_host", naming));
                self.edge(&open.from, &format!("{}.in", id));
                let params = opaque_params(&ports);
                self.node(&id, "opaque_host", params.clone());
                // The ports in the order the operator declares them.
                let shape = NodeShape::of(
                    &OperatorName::new("opaque_host").unwrap(),
                    &serde_json::from_value(params).unwrap(),
                );
                let declared = shape.outputs().expect("the opaque host is configured").to_vec();
                if let [port] = declared.as_slice() {
                    self.open.push(open.after(&id, port.as_str(), "opaque_host"));
                } else {
                    for port in declared {
                        self.open.push(Open::port_of(&id, port.as_str()));
                    }
                }
            }
            Action::Merge => {
                let mut items = vec![open];
                for (k, other) in [step.others.0, step.others.1].iter().enumerate() {
                    if self.open.is_empty() || (k == 1 && step.count.is_multiple_of(2)) {
                        break;
                    }
                    items.push(self.open.remove(other.index(self.open.len())));
                }
                if step.count.is_multiple_of(7) && items.len() > 1 {
                    // A tuple of one item, which is a merge with one input.
                    let rest = items.split_off(1);
                    self.open.extend(rest);
                }
                let first = &items[0];
                let candidate = match naming % 4 {
                    0 => None,
                    1 => Some(ids::merge(&first.parent)),
                    _ => Some(ids::merge(&first.anchor)),
                };
                let id = self.id(candidate);
                for (port, item) in items.iter().enumerate() {
                    self.edge(&item.from, &format!("{}.{}", id, port));
                }
                self.node(&id, "merge", json!({"inputs": items.len()}));
                self.open.push(Open {
                    from: format!("{}.out", id),
                    parent: id.clone(),
                    port: "out".to_string(),
                    multi: false,
                    anchor: id,
                    ordinals: BTreeMap::new(),
                });
            }
            Action::End => self.end(&open, &step.sink, naming),
        }
    }
}

/// Builds the well-formed artifact a plan describes, in canonical order.
fn build_graph(plan: &GraphPlan) -> PolicyArtifact {
    let mut builder = GraphBuilder {
        headers: &plan.headers,
        nodes: Vec::new(),
        edges: Vec::new(),
        taken: HashSet::new(),
        open: INPUTS[..plan.inputs]
            .iter()
            .map(|input| Open {
                from: format!("input.{}", input),
                parent: "input".to_string(),
                port: input.to_string(),
                multi: false,
                anchor: ids::input_anchor(input).to_string(),
                ordinals: BTreeMap::new(),
            })
            .collect(),
        fresh: 0,
    };
    for step in &plan.steps {
        builder.step(step);
    }
    let open = std::mem::take(&mut builder.open);
    for (k, open) in open.iter().enumerate() {
        let (sink, naming) = plan.closers.get(k).cloned().unwrap_or((SinkChoice::Drop, 1));
        builder.end(open, &sink, naming);
    }
    let document = document(&plan.headers, builder.nodes, builder.edges);
    canonical(PolicyArtifact::from_value(document).expect("generated artifacts are valid artifact documents"))
}

/// Returns a strategy for well-formed artifacts in canonical order, built a step at a time.
fn arb_graph() -> impl Strategy<Value = PolicyArtifact> {
    arb_graph_plan().prop_map(|plan| build_graph(&plan))
}

// Well-formed graphs from recursive shapes.

/// The shape of the part of the graph that one edge leads to.
#[derive(Clone, Debug)]
enum Shape {
    /// An `emit`, named by the derived-ID rule when `inline`, and by a fresh ID otherwise.
    Emit { output: usize, inline: bool },
    /// A `drop`, named like an `emit`.
    Drop { inline: bool },
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

/// What a generated artifact's sources lead to.
#[derive(Clone, Debug)]
enum Inputs {
    /// One input, `metrics`.
    One(Shape),
    /// Two inputs, `a` and `b`, each leading to its own part of the graph.
    Two(Shape, Shape),
    /// Two inputs, `a` and `b`, joined by a `merge`.
    Merged(Shape),
}

fn arb_shape() -> impl Strategy<Value = Shape> {
    let leaf = prop_oneof![
        (0..OUTPUTS.len(), any::<bool>()).prop_map(|(output, inline)| Shape::Emit { output, inline }),
        any::<bool>().prop_map(|inline| Shape::Drop { inline }),
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
/// sources reaches them, the order lowering writes plans in.
///
/// This is written independently of the crate's own ordering: it follows the shapes, and knows where each shape's
/// edges complete a node's inputs.
#[derive(Default)]
struct TreeBuilder {
    nodes: Vec<Value>,
    edges: Vec<Value>,
    next_id: usize,
}

impl TreeBuilder {
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

    /// Adds the edge from port `port` of `parent` (which is `input` for a source) and the nodes it leads to.
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
            }
            Shape::Drop { inline } => {
                let id = terminal(self, *inline);
                self.edge(&from, &format!("{}.in", id));
                self.node(&id, "drop", json!({}));
            }
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
    /// Returns well-formed artifacts in canonical order, built from recursive shapes with fresh node IDs.
    fn arb_tree_artifact()(inputs in arb_inputs(), headers in arb_headers()) -> PolicyArtifact {
        let mut builder = TreeBuilder::default();
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
        if let Some(blob) = blob_ref(&headers) {
            // Refer to the blob from a parameter, as an artifact sharing a long list would, on a source of its
            // own, which comes last in canonical order.
            builder.edge("input.shared", "shared.in");
            builder.node("shared", "host_tags", json!({"tags": blob, "deadline": "0s"}));
            builder.edge("shared.out", "shared_out.in");
            builder.node("shared_out", "drop", json!({}));
        }
        let document = document(&headers, builder.nodes, builder.edges);
        PolicyArtifact::from_value(document).expect("generated artifacts are valid artifact documents")
    }
}

// Graphs with arbitrary edges.

/// Returns a strategy for small artifacts with arbitrary edges between a few nodes, well-formed or not: unknown
/// nodes and ports, ports used twice or never, cycles, duplicate IDs, IDs that are sources' names or derived IDs,
/// and parameters that do not configure their operator.
fn arb_arbitrary_artifact() -> impl Strategy<Value = PolicyArtifact> {
    const IDS: &[&str] = &[
        "a",
        "b",
        "c",
        "x",
        "a_fanout",
        "a_split",
        "a_false",
        "input_x_fanout",
        "input_x_split",
        "input_x_split_false",
        "c_0",
    ];
    const PORTS: &[&str] = &[
        "out",
        "in",
        "true",
        "false",
        "0",
        "1",
        "2",
        "default",
        "passthrough",
        "a",
        "b",
    ];
    let ops = prop_oneof![
        arb_chain().boxed(),
        Just(("drop", json!({}))).boxed(),
        (0..OUTPUTS.len())
            .prop_map(|o| ("emit", json!({"output": OUTPUTS[o]})))
            .boxed(),
        Just(("emit", json!({"to": ["primary"]}))).boxed(),
        arb_predicate_value()
            .prop_map(|p| ("split", json!({"predicate": p})))
            .boxed(),
        (1..4usize).prop_map(|n| ("tee", json!({"ports": n}))).boxed(),
        Just(("tee", json!({"ports": "many"}))).boxed(),
        (1..4usize).prop_map(|n| ("merge", json!({"inputs": n}))).boxed(),
        Just(("timestamp_split", json!({"window": "10s"}))).boxed(),
        Just(("opaque_host", opaque_params(&["a", "b"]))).boxed(),
        Just(("source", json!({"binding": "dogstatsd"}))).boxed(),
        Just(("split", json!({}))).boxed(),
    ];
    let end = prop_oneof![
        3 => (select(IDS), select(PORTS)).prop_map(|(n, p)| format!("{}.{}", n, p)),
        1 => select(&["input.x", "input.y", "exit.done"][..]).prop_map(str::to_string),
    ];
    (
        vec((select(IDS), ops), 0..7),
        vec((end.clone(), end), 0..12),
        arb_headers(),
        prop_oneof![3 => Just(0u32), 1 => 2..5u32],
    )
        .prop_map(|(nodes, edges, headers, version)| {
            let nodes = nodes
                .into_iter()
                .map(|(id, (op, params))| json!({"id": id, "op": op, "params": params}))
                .collect::<Vec<_>>();
            let edges = edges
                .into_iter()
                .map(|(from, to)| json!({"from": from, "to": to}))
                .collect::<Vec<_>>();
            let mut document = document(&headers, nodes, edges);
            if version != 0 {
                document["ir_version"] = json!(version);
            }
            serde_json::from_value(document).expect("the artifact has the artifact's shape")
        })
        .boxed()
}

/// Returns whether `artifact` is well-formed, checked directly: its node IDs are unique, its edges name nodes and
/// ports that exist (a node whose ports are unknown has the conventional input `in` and the outputs its edges leave, at
/// least one), one edge enters each input port and at most one leaves each output port and each source, every declared output port is
/// used, and every node is reached from a source without a cycle.
fn well_formed(artifact: &PolicyArtifact) -> bool {
    let mut index = HashMap::new();
    for (i, node) in artifact.nodes.iter().enumerate() {
        if index.insert(node.id.as_str(), i).is_some() {
            return false;
        }
    }
    let shapes = artifact
        .nodes
        .iter()
        .map(|node| NodeShape::of(&node.op, &node.params))
        .collect::<Vec<_>>();
    let inputs_of = |i: usize| match shapes[i].inputs() {
        Some(inputs) => inputs.to_vec(),
        None => vec![PortName::new("in").unwrap()],
    };
    let mut entered: HashMap<(usize, &PortName), usize> = HashMap::new();
    let mut left: HashSet<(usize, &PortName)> = HashSet::new();
    let mut successors = vec![Vec::new(); artifact.nodes.len()];
    let mut roots = Vec::new();
    let mut read = HashSet::new();
    for edge in &artifact.edges {
        let source = match &edge.from {
            ArtifactPortRef::Input(input) => {
                if !read.insert(input) {
                    return false;
                }
                None
            }
            ArtifactPortRef::Node { node, port } => {
                let Some(&i) = index.get(node.as_str()) else {
                    return false;
                };
                if shapes[i].outputs().is_some_and(|outputs| !outputs.contains(port)) || !left.insert((i, port)) {
                    return false;
                }
                Some(i)
            }
        };
        let target = match &edge.to {
            ArtifactPortRef::Node { node, port } => {
                let Some(&i) = index.get(node.as_str()) else {
                    return false;
                };
                if !inputs_of(i).contains(port) {
                    return false;
                }
                *entered.entry((i, port)).or_default() += 1;
                Some(i)
            }
            ArtifactPortRef::Input(_) => return false,
        };
        match (source, target) {
            (Some(source), Some(target)) => successors[source].push(target),
            (None, Some(target)) => roots.push(target),
            _ => {}
        }
    }
    for (i, shape) in shapes.iter().enumerate() {
        if inputs_of(i).iter().any(|port| entered.get(&(i, port)) != Some(&1)) {
            return false;
        }
        match shape.outputs() {
            Some(outputs) => {
                if outputs.iter().any(|port| !left.contains(&(i, port))) {
                    return false;
                }
            }
            None => {
                if !left.iter().any(|&(source, _)| source == i) {
                    return false;
                }
            }
        }
    }
    // Every node is reached, and only once its every input is: no node is on or after a cycle.
    let mut remaining = (0..artifact.nodes.len())
        .map(|i| inputs_of(i).len())
        .collect::<Vec<_>>();
    let mut visited = 0;
    let mut ready = Vec::new();
    for root in roots {
        remaining[root] -= 1;
        if remaining[root] == 0 {
            ready.push(root);
        }
    }
    while let Some(node) = ready.pop() {
        visited += 1;
        for &next in &successors[node] {
            remaining[next] -= 1;
            if remaining[next] == 0 {
                ready.push(next);
            }
        }
    }
    visited == artifact.nodes.len()
}

// Checks.

/// The ways an artifact is printed.
fn print_options() -> Vec<PrintOptions> {
    let mut options = Vec::new();
    for width in [0, 40, 120, usize::MAX] {
        options.push(PrintOptions { width });
    }
    options
}

/// Returns `artifact` without the parameters equal to their registry default, which printing leaves out and
/// normalization fills back in.
fn without_defaults(artifact: &PolicyArtifact) -> PolicyArtifact {
    let mut stripped = artifact.clone();
    for node in &mut stripped.nodes {
        let Some(def) = OperatorRegistry::native().get(node.op.as_str()) else {
            continue;
        };
        node.params = node
            .params
            .iter()
            .filter(|(name, value)| {
                let default = def
                    .params()
                    .iter()
                    .find(|spec| spec.name == name.as_str())
                    .and_then(|spec| spec.default)
                    .map(|json| serde_json::from_str::<Value>(json).expect("defaults are JSON"));
                default.as_ref() != Some(*value)
            })
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
    }
    stripped
}

/// Checks that `artifact`, well-formed and in canonical order, prints with every option to text that parses back to
/// exactly it, and that printing what the text parses to gives the text back.
fn check_round_trip(artifact: &PolicyArtifact) -> Result<(), TestCaseError> {
    for options in print_options() {
        let printed = print(artifact, &options);
        let parsed = match parse(&printed) {
            Ok(parsed) => parsed,
            Err(diagnostics) => {
                return Err(TestCaseError::fail(format!(
                    "{:?}\n{}\n---\n{}\n---\n{}",
                    options,
                    diagnostics,
                    printed,
                    serde_json::to_string(artifact).unwrap()
                )))
            }
        };
        let expected = without_defaults(artifact);
        prop_assert_eq!(
            &parsed.artifact,
            &expected,
            "{:?}\n{}\n---\n{}",
            options,
            printed,
            serde_json::to_string(artifact).unwrap()
        );
        // A fixed point on text: printing what the text parses to gives the text back.
        prop_assert_eq!(print(&parsed.artifact, &options), printed.clone(), "{:?}", options);
    }

    let printed = print_default(artifact);
    let parsed = parse_ok(&printed);
    // The text has fewer tokens than the artifact's JSON has bytes, which keeps the token limit above the text of any
    // artifact within the size limit.
    let tokens = crate::lexer::lex(&printed)
        .map(|tokens| tokens.len())
        .unwrap_or(usize::MAX);
    let json = serde_json::to_vec(artifact).unwrap().len();
    prop_assert!(tokens < json, "{} tokens, {} bytes of JSON", tokens, json);

    // Every node and edge has a span, and each node's span is where it is written: its stage, tuple, or sink, the
    // `where` that makes a drop, or for an implicit tee, the name of the stream it copies.
    for node in &artifact.nodes {
        let span = parsed.spans.node(node.id.as_str());
        prop_assert!(span.is_some(), "{} has no span", node.id);
        let written = text(&printed, span.unwrap());
        let ok = written.starts_with(node.op.as_str())
            || written.starts_with("where ")
            || written.starts_with('(')
            || written.starts_with("emit ")
            || written.starts_with("drop")
            || is_identifier(written);
        prop_assert!(ok, "{}: {:?}", node.id, written);
    }
    prop_assert!(artifact.edges.is_empty() || parsed.spans.edge(artifact.edges.len() - 1).is_some());
    prop_assert!(parsed.spans.edge(artifact.edges.len()).is_none());

    // In normal form, as the default program and its export are, the text parses to the same program once normalized
    // again, defaults and all: an unchanged print publishes as no change.
    let normal = canonical(normalize_artifact(artifact));
    let printed = print_default(&normal);
    let reparsed = parse_ok(&printed).artifact;
    prop_assert_eq!(
        canonical(normalize_artifact(&reparsed)),
        normal,
        "{}\n---\n{}",
        printed,
        serde_json::to_string(artifact).unwrap()
    );
    Ok(())
}

fn is_identifier(text: &str) -> bool {
    saluki_metric_plan::ir::is_identifier(text)
}

/// Checks that `artifact`, well-formed or not, prints with every option without panicking, and that parsing the text
/// does not panic either. A well-formed artifact parses back to itself in canonical order; any other never does, since
/// lowering makes only well-formed artifacts.
fn check_total(artifact: &PolicyArtifact) -> Result<(), TestCaseError> {
    let is_well_formed = well_formed(artifact);
    for options in print_options() {
        let printed = print(artifact, &options);
        let parsed = parse(&printed);
        let expected = without_defaults(&canonical(artifact.clone()));
        if !is_well_formed {
            prop_assert!(
                parsed.as_ref().map_or(true, |parsed| parsed.artifact != expected),
                "{:?}\n{}\n---\n{}",
                options,
                printed,
                serde_json::to_string(artifact).unwrap()
            );
            if let Ok(parsed) = &parsed {
                prop_assert!(well_formed(&parsed.artifact), "{}", printed);
            }
        } else {
            prop_assert_eq!(
                parsed.as_ref().map(|parsed| &parsed.artifact),
                Ok(&expected),
                "{:?}\n{}\n---\n{}",
                options,
                printed,
                serde_json::to_string(artifact).unwrap()
            );
        }
    }
    Ok(())
}

/// Words, literals, and punctuation of the grammar, for generating sources that are nearly valid.
const TOKENS: &[&str] = &[
    "policy",
    "p",
    "{",
    "}",
    "base",
    "blobs",
    "ir_version",
    "let",
    "as",
    "where",
    "fork",
    "emit",
    "output",
    "drop",
    "exit",
    "|",
    "!=",
    "x",
    "y",
    "dogstatsd",
    "x.true",
    "=",
    "split",
    "tee",
    "merge",
    "aggregate",
    "timestamp_split",
    "(",
    ")",
    "(x, y)",
    ".",
    "primary",
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
    "10s",
    "not",
    "and",
    "or",
    "kind",
    "in",
    "counter",
    "name",
    "host",
    "tags",
    "tags.env",
    "origin_tags",
    "is",
    "set",
    "unset",
    "==",
    "starts_with",
    "matches",
    "values",
    "prefix",
    "is_empty",
    "config",
    "config.a.b",
    "blob",
    "#",
    "\n",
    "0",
    "out",
    "ports",
    "inputs",
    "predicate",
    // Retired forms, which have their own diagnostics.
    "node",
    "->",
    "input",
    "all",
    "any",
    "prefix_in",
    "é",
    "\u{202E}",
];

/// Returns a strategy for sources that are random sequences of grammar tokens, with or without a valid opening.
fn arb_token_soup() -> impl Strategy<Value = String> {
    (any::<bool>(), vec(select(TOKENS), 0..80)).prop_map(|(opened, tokens)| {
        let soup = tokens.join(" ");
        if opened {
            format!("policy {{ {} }}", soup)
        } else {
            soup
        }
    })
}

/// Returns a strategy for printed artifacts truncated, cut, or with a token inserted, at character boundaries.
fn arb_damaged_source() -> impl Strategy<Value = String> {
    (
        prop_oneof![arb_graph().boxed(), arb_tree_artifact().boxed()],
        any::<Index>(),
        any::<Index>(),
        0..3u8,
        select(TOKENS),
    )
        .prop_map(|(artifact, at, until, damage, token)| {
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
        })
}

fn arb_source() -> impl Strategy<Value = String> {
    prop_oneof![
        any::<String>().boxed(),
        arb_token_soup().boxed(),
        arb_damaged_source().boxed()
    ]
}

/// Returns the line and column of a byte offset, computed directly.
fn line_and_column(source: &str, offset: usize) -> (usize, usize) {
    let before = &source[..offset];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |newline| newline + 1);
    (line, source[line_start..offset].chars().count() + 1)
}

/// Checks that `source` parses, and prints to a fixed point, or is diagnosed at places in it.
fn check_any_input(source: &str) -> Result<(), TestCaseError> {
    match parse(source) {
        Ok(parsed) => {
            // Lowering makes only well-formed artifacts.
            prop_assert!(
                well_formed(&parsed.artifact),
                "{}\n---\n{}",
                source,
                serde_json::to_string(&parsed.artifact).unwrap()
            );
            // Whatever parses prints to a fixed point, with the parameters equal to their default left out.
            let printed = print_default(&parsed.artifact);
            let reparsed = parse(&printed).map(|parsed| parsed.artifact);
            prop_assert_eq!(
                reparsed.as_ref(),
                Ok(&without_defaults(&parsed.artifact)),
                "{}\n---\n{}",
                source,
                printed
            );
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
                prop_assert_eq!(
                    (diagnostic.line, diagnostic.column),
                    line_and_column(source, span.start)
                );
                prop_assert!(!diagnostic.message.is_empty());
            }
        }
    }
    Ok(())
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
        let graphs = arb_graph();
        let trees = arb_tree_artifact();
        let arbitrary = arb_arbitrary_artifact();
        let sources = arb_source();
        for _ in 0..128 {
            for artifact in [
                graphs.new_tree(&mut runner).expect("graphs generate").current(),
                trees.new_tree(&mut runner).expect("artifacts generate").current(),
            ] {
                check_round_trip(&artifact).unwrap();
            }
            let artifact = arbitrary.new_tree(&mut runner).expect("artifacts generate").current();
            check_total(&artifact).unwrap();
            let source = sources.new_tree(&mut runner).expect("sources generate").current();
            check_any_input(&source).unwrap();
        }
    });
}

/// Long chains of `tee`s, each feeding the next through a `where`, and of `where`s and merges, print and parse back
/// exactly.
#[test]
fn deeply_nested_forms_print_and_parse_back_exactly() {
    let tees = (0..24).fold("  let t0 = dogstatsd | tee(ports: 2)".to_string(), |text, i| {
        format!(
            "{}\n  t{}.0 | drop\n  let t{} = t{}.1 | where name == \"a\" | tee(ports: 2)",
            text,
            i,
            i + 1,
            i
        )
    }) + "\n  t24.0 | drop\n  t24.1 | drop";
    let merges = (0..40).fold("  let m0 = (dogstatsd, otlp)".to_string(), |text, i| {
        format!(
            "{}\n  let m{} = (m{}, checks{}) | where tags.k{} is set",
            text,
            i + 1,
            i,
            i,
            i
        )
    }) + "\n  m40 | drop";
    on_small_stack(move || {
        for (source, core) in [(tees, "tee(ports: 2)"), (merges, "")] {
            let artifact = parse_ok(&policy(&source)).artifact;
            assert!(print_default(&artifact).contains(core));
            check_round_trip(&artifact).unwrap();
        }
    });
}

/// The forms of the syntax and the shapes of graph that the well-formed generators cover, each counted in the
/// printed text or the artifact, and the well-formed artifacts that the generator of arbitrary edges makes.
#[test]
fn the_generators_cover_every_form() {
    let mut runner = TestRunner::deterministic();
    let strategy = prop_oneof![arb_graph().boxed(), arb_tree_artifact().boxed()];
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for _ in 0..1000 {
        let artifact = strategy.new_tree(&mut runner).unwrap().current();
        let printed = print_default(&artifact);
        let mut seen = BTreeSet::new();
        for node in &artifact.nodes {
            seen.insert(format!("operator {}", node.op));
        }
        for (form, needle) in [
            ("let", "let "),
            ("where", "| where "),
            ("as", " as "),
            ("let named apart from its node", "_let = "),
            ("operator in parentheses", "()"),
            ("port reference", ".passthrough"),
            ("index port reference", ".1 "),
            ("emit output", "emit output "),
            ("emit recipients", "emit ["),
            ("drop", "drop"),
            ("core split", "split("),
            ("core tee", "tee(ports: "),
            ("core emit", "emit(output: "),
            ("positional", "timestamp_split(1"),
            ("binding", "config."),
            ("blob", "blob(\""),
            ("unregistered", "alpha"),
        ] {
            if printed.contains(needle) {
                seen.insert(form.to_string());
            }
        }
        let parsed = parse_ok(&printed);
        if without_defaults(&artifact) != artifact {
            seen.insert("default left out".to_string());
        }
        let program = crate::parser::parse_program(&printed, &crate::lexer::lex(&printed).unwrap()).unwrap();
        let mut refs: HashMap<(String, Option<String>), usize> = HashMap::new();
        let mut aliases = HashSet::new();
        for statement in &program.statements {
            if let crate::ast::Statement::Let { name, pipeline, .. } = statement {
                if let (crate::ast::Head::Ref(head), true) = (&pipeline.head, pipeline.stages.is_empty()) {
                    seen.insert("alias".to_string());
                    if head.port.is_some() {
                        seen.insert("alias of a port".to_string());
                    }
                    if aliases.contains(&head.name.id) {
                        seen.insert("alias of an alias".to_string());
                    }
                    aliases.insert(name.id.clone());
                }
            }
            let pipeline = match statement {
                crate::ast::Statement::Let { pipeline, .. } | crate::ast::Statement::Run { pipeline, .. } => pipeline,
            };
            let heads = match &pipeline.head {
                crate::ast::Head::Ref(reference) => vec![reference],
                crate::ast::Head::Tuple(tuple) => {
                    seen.insert("tuple".to_string());
                    tuple.items.iter().collect()
                }
            };
            for head in heads {
                let port = head.port.as_ref().map(|(port, _)| port.to_string());
                *refs.entry((head.name.id.to_string(), port)).or_default() += 1;
            }
            if matches!(statement, crate::ast::Statement::Let { .. }) && pipeline.stages.len() >= 2 {
                seen.insert("let of several stages".to_string());
            }
            if matches!(statement, crate::ast::Statement::Let { .. })
                && matches!(pipeline.head, crate::ast::Head::Tuple(_))
                && pipeline.stages.is_empty()
            {
                seen.insert("let of a tuple alone".to_string());
            }
        }
        let inputs = parsed
            .artifact
            .edges
            .iter()
            .filter_map(|edge| match &edge.from {
                ArtifactPortRef::Input(input) => Some(input.to_string()),
                _ => None,
            })
            .collect::<HashSet<_>>();
        for ((name, port), &uses) in &refs {
            if uses > 1 {
                seen.insert("reused stream".to_string());
                if port.is_none() && inputs.contains(name) {
                    seen.insert("reused source".to_string());
                }
            }
        }
        // A split that drops on `true` and not on `false`: the native polarity, written in the core form.
        for node in &parsed.artifact.nodes {
            if node.op.as_str() == "split"
                && artifact.nodes.iter().any(|n| {
                    n.id == ids::terminal(node.id.as_str(), &PortName::new("true").unwrap()) && n.op.as_str() == "drop"
                })
            {
                seen.insert("split dropping on true".to_string());
            }
            if node.id.as_str().len() == 63 {
                seen.insert("shortened ID".to_string());
            }
        }
        for form in seen {
            *counts.entry(form).or_default() += 1;
        }
    }
    let mut missing = OperatorRegistry::native()
        .iter()
        .map(|def| format!("operator {}", def.name()))
        // A `source` has no input, so no well-formed artifact has one.
        .filter(|op| op != "operator source")
        .chain(
            [
                "let",
                "let of several stages",
                "where",
                "let of a tuple alone",
                "alias",
                "alias of a port",
                "alias of an alias",
                "as",
                "let named apart from its node",
                "operator in parentheses",
                "default left out",
                "reused source",
                "tuple",
                "port reference",
                "index port reference",
                "reused stream",
                "emit output",
                "emit recipients",
                "drop",
                "core split",
                "core tee",
                "core emit",
                "positional",
                "binding",
                "blob",
                "unregistered",
                "split dropping on true",
                "shortened ID",
            ]
            .map(str::to_string),
        )
        .filter(|form| counts.get(form).is_none_or(|&count| count < 10))
        .collect::<Vec<_>>();
    missing.sort();
    assert!(
        missing.is_empty(),
        "rarely or never generated: {:?}\n{:#?}",
        missing,
        counts
    );

    let arbitrary = arb_arbitrary_artifact();
    let well_formed = (0..2000)
        .filter(|_| well_formed(&arbitrary.new_tree(&mut runner).unwrap().current()))
        .count();
    assert!(
        well_formed >= 10,
        "{} well-formed artifacts of 2000 with arbitrary edges",
        well_formed
    );
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2048, ..ProptestConfig::default() })]

    #[test]
    fn property_test_well_formed_graphs_print_and_parse_back_exactly(artifact in arb_graph()) {
        prop_assert!(well_formed(&artifact), "{}", serde_json::to_string(&artifact).unwrap());
        check_round_trip(&artifact)?;
    }

    #[test]
    fn property_test_well_formed_trees_print_and_parse_back_exactly(artifact in arb_tree_artifact()) {
        prop_assert!(well_formed(&artifact), "{}", serde_json::to_string(&artifact).unwrap());
        check_round_trip(&artifact)?;
    }

    #[test]
    fn property_test_any_artifact_prints_and_its_text_parses_without_panicking(
        artifact in prop_oneof![arb_arbitrary_artifact(), arb_graph().boxed(), arb_tree_artifact().boxed()],
    ) {
        check_total(&artifact)?;
    }

    #[test]
    fn property_test_printing_depends_only_on_the_graph(
        (artifact, nodes, edges) in prop_oneof![arb_graph().boxed(), arb_tree_artifact().boxed()]
            .prop_flat_map(|artifact| {
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
        prop_assert_eq!(parse(&printed).map(|parsed| parsed.artifact), Ok(without_defaults(&artifact)));
    }

    #[test]
    fn property_test_printing_any_graph_depends_only_on_the_graph(
        (artifact, nodes, edges) in arb_arbitrary_artifact()
            .prop_filter("node IDs and edges are distinct", |artifact| {
                // Only the order of nodes with one ID, or of edges with the same ends, tells them apart.
                let ids = artifact.nodes.iter().map(|node| &node.id).collect::<HashSet<_>>();
                let edges = artifact.edges.iter().map(|edge| (&edge.from, &edge.to)).collect::<HashSet<_>>();
                ids.len() == artifact.nodes.len() && edges.len() == artifact.edges.len()
            })
            .prop_flat_map(|artifact| {
                let nodes = Just(artifact.nodes.clone()).prop_shuffle();
                let edges = Just(artifact.edges.clone()).prop_shuffle();
                (Just(artifact), nodes, edges)
            }),
    ) {
        // Well-formed or not, an artifact prints the same text in any order.
        let mut shuffled = artifact.clone();
        shuffled.nodes = nodes;
        shuffled.edges = edges;
        prop_assert_eq!(print_default(&shuffled), print_default(&artifact));
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
            "nodes": [
                {"id": "n", "op": "frobnicate", "params": {"v": values}},
                {"id": "n_out", "op": "drop"},
            ],
            "edges": [{"from": "input.metrics", "to": "n.in"}, {"from": "n.out", "to": "n_out.in"}],
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
    fn property_test_aliases_print_and_parse_back_to_a_fixed_point(
        (depth, order) in vec(0..3usize, 1..5).prop_flat_map(|extra| {
            let depth = extra.len();
            let uses = (0..depth)
                .flat_map(|i| std::iter::repeat_n(i, extra[i] + usize::from(i + 1 == depth)))
                .collect::<Vec<_>>();
            Just(uses).prop_shuffle().prop_map(move |order| (depth, order))
        }),
    ) {
        // `let a0 = metrics | host_enrichment`, then a chain of aliases, `let a1 = a0`, ..., each used some more times
        // by a sink, the last at least once, the uses in any order: an alias used once makes no node, and one used
        // several times makes its own tee. The text parses, and printing what it parses to parses back to the same
        // artifact and prints the same again.
        let mut statements = vec!["  let a0 = metrics | host_enrichment".to_string()];
        statements.extend((1..depth).map(|i| format!("  let a{} = a{}", i, i - 1)));
        statements.extend(order.iter().map(|i| format!("  a{} | drop", i)));
        let source = policy(&statements.join("\n"));
        let parsed = parse(&source).map_err(|diagnostics| TestCaseError::fail(format!("{}\n{}", diagnostics, source)))?;
        let printed = print_default(&parsed.artifact);
        let reparsed = parse(&printed).map_err(|diagnostics| TestCaseError::fail(format!("{}\n{}", diagnostics, printed)))?;
        prop_assert_eq!(&reparsed.artifact, &parsed.artifact, "{}\n---\n{}", source, printed);
        prop_assert_eq!(print_default(&reparsed.artifact), printed);
    }

    #[test]
    fn property_test_any_input_parses_or_is_diagnosed_without_panicking(source in arb_source()) {
        check_any_input(&source)?;
    }
}
