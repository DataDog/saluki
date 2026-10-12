//! Hand-written validation fixtures: small plans, each with the findings validation must report, written down
//! independently of the implementation.

use serde_json::{json, Value};

use super::*;
use crate::{
    canonical::PlanHash,
    ir::{BindingName, InputName, OutputId, ParamType, RecipientId},
    model::{Kind, KindSet, PortType, TimestampForm},
};

/// A fixture: what it shows, the plan, and the findings validation must report, as rule and location.
type Case = (&'static str, Value, Vec<(Rule, &'static str)>);

const ZERO_HASH: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

/// The outputs the test catalog declares: ID, accepted type, and recipients. `shared_1` and `shared_2` share a
/// recipient.
fn test_outputs() -> Vec<(&'static str, Value, Vec<&'static str>)> {
    let all = json!({"kinds": "all", "timestamps": "mixed"});
    vec![
        ("a", all.clone(), vec!["out/a"]),
        ("b", all.clone(), vec!["out/b"]),
        ("shared_1", all.clone(), vec!["out/shared"]),
        ("shared_2", all.clone(), vec!["out/other", "out/shared"]),
        (
            "counters",
            json!({"kinds": ["counter", "rate", "gauge", "set"], "timestamps": "mixed"}),
            vec!["out/c"],
        ),
        (
            "plain",
            json!({"kinds": "all", "timestamps": "untimestamped"}),
            vec!["out/plain"],
        ),
        ("internal", all, vec![]),
    ]
}

fn fingerprint(id: &str) -> PlanHash {
    PlanHash::of_bytes(id.as_bytes())
}

fn test_catalog() -> MemoryCatalog {
    let mut catalog = MemoryCatalog::new();
    for (id, accepts, recipients) in test_outputs() {
        catalog = catalog.with_output(
            OutputId::new(id).unwrap(),
            CatalogOutput {
                accepts: serde_json::from_value(accepts).unwrap(),
                fingerprint: fingerprint(id),
                recipients: recipients.into_iter().map(|r| RecipientId::new(r).unwrap()).collect(),
                legacy_fingerprint: None,
            },
        );
    }
    let binding = |name: &str| BindingName::new(name).unwrap();
    catalog
        .with_binding(binding("test.flag"), ParamType::Bool, Liveness::Live)
        .with_binding(
            binding("test.names"),
            ParamType::List(&ParamType::String),
            Liveness::Live,
        )
        .with_binding(binding("test.window"), ParamType::Duration, Liveness::Startup)
        .with_binding(binding("test.live_window"), ParamType::Duration, Liveness::Live)
        .with_scope_input(
            "post".parse().unwrap(),
            InputName::new("metrics").unwrap(),
            PortType::ANY,
        )
        .with_scope_input(
            "pre".parse().unwrap(),
            InputName::new("metrics").unwrap(),
            PortType::ANY,
        )
        .with_scope_input(
            "post".parse().unwrap(),
            InputName::new("gauges").unwrap(),
            PortType {
                kinds: KindSet::of(&[Kind::Gauge]),
                timestamps: TimestampForm::Mixed,
            },
        )
}

/// A node with native provenance.
fn n(id: &str, op: &str, params: Value) -> Value {
    json!({"id": id, "op": op, "params": params, "provenance": {"native": "test"}})
}

/// A node with artifact provenance.
fn a(id: &str, op: &str, params: Value) -> Value {
    json!({"id": id, "op": op, "params": params,
           "provenance": {"artifact": {"revision": 7, "hash": ZERO_HASH}}})
}

/// A node without provenance.
fn bare(id: &str, op: &str, params: Value) -> Value {
    json!({"id": id, "op": op, "params": params})
}

fn otlp(id: &str) -> Value {
    n(id, "source", json!({"binding": "otlp"}))
}

fn dogstatsd(id: &str, timestamps: &str) -> Value {
    n(id, "source", json!({"binding": "dogstatsd", "timestamps": timestamps}))
}

fn emit(id: &str, output: &str) -> Value {
    n(id, "emit", json!({"output": output}))
}

fn drop(id: &str) -> Value {
    n(id, "drop", json!({}))
}

fn tee(id: &str, ports: u16) -> Value {
    n(id, "tee", json!({"ports": ports}))
}

fn merge(id: &str, inputs: u16) -> Value {
    n(id, "merge", json!({"inputs": inputs}))
}

fn split(id: &str, predicate: Value) -> Value {
    n(id, "split", json!({"predicate": predicate}))
}

fn aggregate(id: &str) -> Value {
    n(
        id,
        "aggregate",
        json!({
            "window": "10s", "flush_interval": "15s", "context_limit": 1000, "counter_expiry": "5m",
            "flush_open_windows": false, "histogram_aggregates": ["max", "count"], "histogram_percentiles": [95],
            "copy_to_distribution": false, "copy_to_distribution_prefix": "",
        }),
    )
}

fn output_decls(ids: &[&str]) -> Value {
    let outputs = test_outputs();
    Value::Array(
        ids.iter()
            .map(|id| {
                let (_, accepts, recipients) = outputs.iter().find(|(known, _, _)| known == id).unwrap();
                json!({"id": id, "accepts": accepts, "fingerprint": fingerprint(id).to_string(),
                       "recipients": recipients})
            })
            .collect(),
    )
}

/// Builds a plan from nodes, edges written as `"node.port -> node.port"`, and the IDs of the outputs it declares.
///
/// The plan's provenance lists artifact revision 7, which [`a`] gives its nodes.
fn plan(nodes: Vec<Value>, edges: &[&str], outputs: &[&str]) -> Value {
    let edges = edges
        .iter()
        .map(|edge| {
            let (from, to) = edge.split_once(" -> ").unwrap();
            json!({"from": from, "to": to})
        })
        .collect::<Vec<_>>();
    json!({"ir_version": 1, "provenance": {"artifacts": [{"revision": 7, "hash": ZERO_HASH}]},
           "nodes": nodes, "edges": edges, "outputs": output_decls(outputs)})
}

fn validate(value: &Value) -> ValidationReport {
    let plan = Plan::from_value(value.clone()).unwrap_or_else(|error| panic!("{}: {}", error, value));
    validate_plan(&plan)
}

fn validate_plan(plan: &Plan) -> ValidationReport {
    let registry = OperatorRegistry::native();
    let catalog = test_catalog();
    Validator::new(&registry, &catalog).validate(plan)
}

#[track_caller]
fn assert_findings(name: &str, report: &ValidationReport, expected: &[(Rule, &str)]) {
    let actual = report
        .findings()
        .iter()
        .map(|finding| (finding.rule, finding.location.to_string()))
        .collect::<Vec<_>>();
    let expected = expected
        .iter()
        .map(|(rule, location)| (*rule, location.to_string()))
        .collect::<Vec<_>>();
    assert_eq!(actual, expected, "{}:\n{}", name, report);
    for finding in report.findings() {
        assert_eq!(finding.severity, finding.rule.severity());
        assert!(!finding.message.is_empty());
    }
}

/// The predicate both splits of the complementary-split fixtures use.
fn name_prefix() -> Value {
    json!({"name": {"prefix": "a."}})
}

/// Two copies from a tee, each through its own split on `left` and `right`, reaching output `a` by the given ports.
/// `before_left` and `before_right` are optional operators on the left and right copies before their splits.
fn complementary(
    left: Value, right: Value, left_port: &str, right_port: &str, before_left: Option<Value>,
    before_right: Option<Value>,
) -> Value {
    let other = |port: &str| if port == "true" { "false" } else { "true" };
    let mut nodes = vec![
        otlp("src"),
        tee("t", 2),
        split("s1", left),
        split("s2", right),
        emit("e1", "a"),
        emit("e2", "a"),
        drop("d1"),
        drop("d2"),
    ];
    let mut edges = vec!["src.metrics -> t.in".to_string()];
    match before_left {
        Some(mut node) => {
            node["id"] = json!("w");
            nodes.push(node);
            edges.push("t.0 -> w.in".to_string());
            edges.push("w.out -> s1.in".to_string());
        }
        None => edges.push("t.0 -> s1.in".to_string()),
    }
    match before_right {
        Some(mut node) => {
            node["id"] = json!("w2");
            nodes.push(node);
            edges.push("t.1 -> w2.in".to_string());
            edges.push("w2.out -> s2.in".to_string());
        }
        None => edges.push("t.1 -> s2.in".to_string()),
    }
    edges.extend([
        format!("s1.{} -> e1.in", left_port),
        format!("s1.{} -> d1.in", other(left_port)),
        format!("s2.{} -> e2.in", right_port),
        format!("s2.{} -> d2.in", other(right_port)),
    ]);
    let edges = edges.iter().map(String::as_str).collect::<Vec<_>>();
    plan(nodes, &edges, &["a"])
}

#[test]
fn structure_rules() {
    use Rule::*;

    let cases: Vec<Case> = vec![
        (
            "a source emitting to an output is valid",
            plan(vec![otlp("src"), emit("e", "a")], &["src.metrics -> e.in"], &["a"]),
            vec![],
        ),
        (
            "node IDs are unique",
            plan(
                vec![otlp("src"), emit("e", "a"), emit("e", "a")],
                &["src.metrics -> e.in"],
                &["a"],
            ),
            vec![(UniqueNodeIds, "node `e`")],
        ),
        (
            "operators are registered",
            plan(
                vec![otlp("src"), n("x", "regex_filter", json!({})), emit("e", "a")],
                &["src.metrics -> x.in", "x.out -> e.in"],
                &["a"],
            ),
            vec![(KnownOperator, "node `x`")],
        ),
        (
            "parameters are valid, and every problem is reported",
            plan(
                vec![
                    otlp("src"),
                    n("t", "tee", json!({"ports": 0, "bogus": 1})),
                    emit("e", "a"),
                ],
                &["src.metrics -> t.in", "t.0 -> e.in"],
                &["a"],
            ),
            vec![(ValidParameters, "node `t`"), (ValidParameters, "node `t`")],
        ),
        (
            "edges refer to existing nodes",
            plan(
                vec![otlp("src"), emit("e", "a")],
                &["src.metrics -> e.in", "src.metrics -> ghost.in"],
                &["a"],
            ),
            vec![(EdgeReferences, "edge 1"), (OneEdgePerPort, "port `src.metrics`")],
        ),
        (
            "edges refer to existing ports",
            plan(
                vec![otlp("src"), emit("e", "a"), emit("e2", "a")],
                &["src.metrics -> e.in", "src.events -> e2.in"],
                &["a"],
            ),
            vec![(EdgeReferences, "edge 1")],
        ),
        (
            "an input port has one edge: fan in with a merge",
            plan(
                vec![
                    otlp("src"),
                    n("src2", "source", json!({"binding": "checks"})),
                    emit("e", "a"),
                ],
                &["src.metrics -> e.in", "src2.metrics -> e.in"],
                &["a"],
            ),
            vec![(OneEdgePerPort, "port `e.in`")],
        ),
        (
            "an output port has one edge: fan out with a tee",
            plan(
                vec![otlp("src"), emit("e1", "a"), emit("e2", "b")],
                &["src.metrics -> e1.in", "src.metrics -> e2.in"],
                &["a", "b"],
            ),
            vec![(OneEdgePerPort, "port `src.metrics`")],
        ),
        (
            "every input port has an edge",
            plan(
                vec![otlp("src"), merge("m", 2), emit("e", "a")],
                &["src.metrics -> m.0", "m.out -> e.in"],
                &["a"],
            ),
            vec![(OneEdgePerPort, "port `m.1`")],
        ),
        (
            "every path ends in an emit or a drop",
            plan(
                vec![otlp("src"), tee("t", 2), emit("e", "a")],
                &["src.metrics -> t.in", "t.0 -> e.in"],
                &["a"],
            ),
            vec![(PathsEndInEmitOrDrop, "port `t.1`")],
        ),
        (
            "plans are acyclic",
            plan(
                vec![otlp("src"), merge("m", 2), tee("t", 2), emit("e", "a")],
                &["src.metrics -> m.0", "m.out -> t.in", "t.0 -> e.in", "t.1 -> m.1"],
                &["a"],
            ),
            vec![(Acyclic, "node `m`")],
        ),
        (
            "cycles are found from every node, including unreachable ones",
            plan(
                vec![otlp("src"), emit("e", "a"), merge("m", 1), tee("t", 2), drop("d")],
                &["src.metrics -> e.in", "m.out -> t.in", "t.0 -> m.0", "t.1 -> d.in"],
                &["a"],
            ),
            vec![
                (Acyclic, "node `m`"),
                (Reachable, "node `m`"),
                (Reachable, "node `t`"),
                (Reachable, "node `d`"),
            ],
        ),
        (
            "every node is reachable from a source",
            plan(
                vec![otlp("src"), emit("e", "a"), emit("e2", "b")],
                &["src.metrics -> e.in"],
                &["a", "b"],
            ),
            vec![(OneEdgePerPort, "port `e2.in`"), (Reachable, "node `e2`")],
        ),
        (
            "a host source enters the plan once",
            plan(
                vec![otlp("src"), otlp("src2"), merge("m", 2), emit("e", "a")],
                &["src.metrics -> m.0", "src2.metrics -> m.1", "m.out -> e.in"],
                &["a"],
            ),
            vec![(UniqueSources, "node `src2`")],
        ),
        (
            "policy-namespace nodes do not claim native provenance",
            plan(
                vec![otlp("src"), n("policy/x", "host_enrichment", json!({})), emit("e", "a")],
                &["src.metrics -> policy/x.in", "policy/x.out -> e.in"],
                &["a"],
            ),
            vec![(ProvenanceNamespace, "node `policy/x`")],
        ),
        (
            "policy-namespace nodes have the provenance of their artifact, even when lossless",
            plan(
                vec![
                    otlp("src"),
                    bare("policy/t", "tee", json!({"ports": 1})),
                    emit("e", "a"),
                ],
                &["src.metrics -> policy/t.in", "policy/t.0 -> e.in"],
                &["a"],
            ),
            vec![(ProvenanceNamespace, "node `policy/t`")],
        ),
        (
            "policy-namespace nodes with artifact provenance are valid",
            plan(
                vec![otlp("src"), a("policy/t", "tee", json!({"ports": 1})), emit("e", "a")],
                &["src.metrics -> policy/t.in", "policy/t.0 -> e.in"],
                &["a"],
            ),
            vec![],
        ),
    ];
    for (name, value, expected) in cases {
        assert_findings(name, &validate(&value), &expected);
    }

    // The message says how to fix it.
    let report = validate(&plan(
        vec![otlp("src"), otlp("src2"), merge("m", 2), emit("e", "a")],
        &["src.metrics -> m.0", "src2.metrics -> m.1", "m.out -> e.in"],
        &["a"],
    ));
    assert_eq!(
        report.findings()[0].message,
        "node `src` already binds the `otlp` source, which enters the plan once; fan its records out with a `tee` \
         instead"
    );

    let mut wrong_version = Plan::from_value(plan(
        vec![otlp("src"), emit("e", "a")],
        &["src.metrics -> e.in"],
        &["a"],
    ))
    .unwrap();
    wrong_version.ir_version = 2;
    assert_findings(
        "the IR version is checked",
        &validate_plan(&wrong_version),
        &[(IrVersion, "plan")],
    );
}

#[test]
fn type_rules() {
    use Rule::*;

    let gateway = json!({"all": [{"kind_in": ["counter", "rate", "gauge", "set"]}, {"name": {"in": ["x"]}}]});
    let cases: Vec<Case> = vec![
        (
            "an operator that cannot receive its input is rejected",
            plan(
                vec![
                    dogstatsd("src", "mixed"),
                    n(
                        "o",
                        "opaque_host",
                        json!({"component": "gateway", "input": {"kinds": ["gauge"], "timestamps": "mixed"},
                               "outputs": {"out": {"kinds": ["gauge"], "timestamps": "mixed"}}}),
                    ),
                    emit("e", "a"),
                ],
                &["src.metrics -> o.in", "o.out -> e.in"],
                &["a"],
            ),
            vec![(PortTypes, "port `o.in`")],
        ),
        (
            "an emit whose output does not accept its input is rejected",
            plan(
                vec![dogstatsd("src", "mixed"), emit("e", "counters")],
                &["src.metrics -> e.in"],
                &["counters"],
            ),
            vec![(OutputAccepts, "node `e`")],
        ),
        (
            "a split narrows the kinds on its ports",
            plan(
                vec![
                    dogstatsd("src", "mixed"),
                    split("s", gateway.clone()),
                    emit("e", "counters"),
                    drop("d"),
                ],
                &["src.metrics -> s.in", "s.true -> e.in", "s.false -> d.in"],
                &["counters"],
            ),
            vec![],
        ),
        (
            "a predicate that can never match its input is a warning",
            plan(
                vec![
                    dogstatsd("src", "untimestamped"),
                    aggregate("agg"),
                    split("s", json!({"kind_in": ["counter"]})),
                    emit("e", "a"),
                    drop("d"),
                ],
                &[
                    "src.metrics -> agg.in",
                    "agg.out -> s.in",
                    "s.true -> e.in",
                    "s.false -> d.in",
                ],
                &["a"],
            ),
            vec![(PredicateCanMatch, "node `s`")],
        ),
        (
            "a port that can never carry a record is a warning",
            plan(
                vec![
                    dogstatsd("src", "untimestamped"),
                    n("ts", "timestamp_split", json!({"window": "10s"})),
                    emit("e", "a"),
                    drop("d"),
                ],
                &["src.metrics -> ts.in", "ts.default -> e.in", "ts.passthrough -> d.in"],
                &["a"],
            ),
            vec![(PortCanCarry, "port `ts.passthrough`")],
        ),
        (
            "a port that carries nothing constrains no timestamp form",
            // Untimestamped input leaves the passthrough port empty, so the merge carries only untimestamped points,
            // which the output accepts.
            plan(
                vec![
                    dogstatsd("src", "untimestamped"),
                    n("ts", "timestamp_split", json!({"window": "10s"})),
                    merge("m", 2),
                    emit("e", "plain"),
                ],
                &[
                    "src.metrics -> ts.in",
                    "ts.default -> m.0",
                    "ts.passthrough -> m.1",
                    "m.out -> e.in",
                ],
                &["plain"],
            ),
            vec![(PortCanCarry, "port `ts.passthrough`")],
        ),
    ];
    for (name, value, expected) in cases {
        let report = validate(&value);
        assert_findings(name, &report, &expected);
    }

    // Warnings do not make a plan invalid.
    let warned = validate(&plan(
        vec![
            dogstatsd("src", "untimestamped"),
            n("ts", "timestamp_split", json!({"window": "10s"})),
            emit("e", "a"),
            drop("d"),
        ],
        &["src.metrics -> ts.in", "ts.default -> e.in", "ts.passthrough -> d.in"],
        &["a"],
    ));
    assert!(warned.is_valid());
    assert_eq!(warned.warnings().count(), 1);
    assert_eq!(warned.errors().count(), 0);
}

#[test]
fn reference_rules() {
    use Rule::*;

    let flag_split = |flag: Value| {
        plan(
            vec![
                otlp("src"),
                split("s", json!({"flag": flag})),
                emit("e", "a"),
                drop("d"),
            ],
            &["src.metrics -> s.in", "s.true -> e.in", "s.false -> d.in"],
            &["a"],
        )
    };
    let mut wrong_fingerprint = plan(vec![otlp("src"), emit("e", "a")], &["src.metrics -> e.in"], &["a"]);
    wrong_fingerprint["outputs"][0]["fingerprint"] = json!(ZERO_HASH);
    let mut wrong_recipients = plan(vec![otlp("src"), emit("e", "a")], &["src.metrics -> e.in"], &["a"]);
    wrong_recipients["outputs"][0]["recipients"] = json!(["out/a", "out/b"]);
    let mut wrong_accepts = plan(
        vec![n("src", "source", json!({"binding": "liveness"})), emit("e", "a")],
        &["src.metrics -> e.in"],
        &["a"],
    );
    wrong_accepts["outputs"][0]["accepts"] = json!({"kinds": "all", "timestamps": "timestamped"});
    let mut reordered_recipients = plan(
        vec![otlp("src"), emit("e", "shared_2")],
        &["src.metrics -> e.in"],
        &["shared_2"],
    );
    reordered_recipients["outputs"][0]["recipients"] = json!(["out/shared", "out/other"]);
    let mut undeclared = plan(vec![otlp("src"), emit("e", "a")], &["src.metrics -> e.in"], &["a"]);
    undeclared["outputs"][0]["id"] = json!("z");
    undeclared["nodes"][1]["params"]["output"] = json!("z");
    let mut duplicated = plan(vec![otlp("src"), emit("e", "a")], &["src.metrics -> e.in"], &["a", "a"]);
    duplicated["outputs"][1]["recipients"] = json!(["out/a"]);
    let scoped = |scope: &str, input: &str, port: &str| {
        let mut value = plan(vec![otlp("src"), emit("e", "a")], &["src.metrics -> e.in"], &["a"]);
        value["scopes"] = json!({scope: {"inputs": {input: port}}});
        value
    };
    let names = |resolved: Value| {
        json!({"name": {"matches": {
            "values": {"config": "test.names", "resolved": resolved},
            "prefix": false}}})
    };

    let cases: Vec<Case> = vec![
        (
            "emits deliver to declared outputs",
            plan(vec![otlp("src"), emit("e", "b")], &["src.metrics -> e.in"], &["a"]),
            vec![(DeclaredOutput, "node `e`"), (UnusedOutput, "output `a`")],
        ),
        (
            "declared outputs are declared by the host",
            undeclared,
            vec![(CatalogOutput, "output `z`")],
        ),
        (
            "declared outputs match the host's binding",
            wrong_fingerprint,
            vec![(CatalogOutput, "output `a`")],
        ),
        (
            "declared outputs have the host's recipients",
            wrong_recipients,
            vec![(CatalogOutput, "output `a`")],
        ),
        (
            "declared outputs accept what the host's output accepts",
            wrong_accepts,
            vec![(CatalogOutput, "output `a`")],
        ),
        ("recipients are compared as a set", reordered_recipients, vec![]),
        (
            "output IDs are unique",
            duplicated,
            vec![(UniqueOutputIds, "output `a`")],
        ),
        (
            "an output nothing emits to is a warning",
            plan(vec![otlp("src"), emit("e", "a")], &["src.metrics -> e.in"], &["a", "b"]),
            vec![(UnusedOutput, "output `b`")],
        ),
        (
            "bindings are declared by the host",
            flag_split(json!({"config": "test.nope"})),
            vec![(KnownBinding, "node `s`")],
        ),
        (
            "a binding's type is admitted by its parameter",
            flag_split(json!({"config": "test.names"})),
            vec![(BindingType, "node `s`")],
        ),
        (
            "a declared binding of the right type is valid",
            flag_split(json!({"config": "test.flag"})),
            vec![],
        ),
        (
            "every use of a binding resolves to the same value",
            plan(
                vec![
                    otlp("src"),
                    tee("t", 2),
                    split("s1", names(json!(["x"]))),
                    split("s2", names(json!(["y"]))),
                    emit("e1", "a"),
                    emit("e2", "b"),
                    drop("d1"),
                    drop("d2"),
                ],
                &[
                    "src.metrics -> t.in",
                    "t.0 -> s1.in",
                    "t.1 -> s2.in",
                    "s1.true -> e1.in",
                    "s1.false -> d1.in",
                    "s2.true -> e2.in",
                    "s2.false -> d2.in",
                ],
                &["a", "b"],
            ),
            vec![(ConsistentBinding, "node `s2`")],
        ),
        (
            // Aggregation was the one native host with bindable parameters; as a kernel in its isolation slot, its
            // per-call parameters follow each generation, so a live binding is no host warning.
            "a state owner that runs as a kernel applies live configuration",
            {
                let mut node = aggregate("agg");
                node["params"]["window"] = json!({"config": "test.live_window", "resolved": "10s"});
                plan(
                    vec![dogstatsd("src", "untimestamped"), node, emit("e", "a")],
                    &["src.metrics -> agg.in", "agg.out -> e.in"],
                    &["a"],
                )
            },
            vec![],
        ),
        (
            "a scope input refers to a declared input of the right type",
            scoped("post", "metrics", "src.metrics"),
            vec![],
        ),
        (
            "a scope input refers to an output port",
            scoped("post", "metrics", "e.out"),
            vec![(ScopeInputs, "scope `post`")],
        ),
        (
            "a scope input is declared by the host",
            scoped("other", "metrics", "src.metrics"),
            vec![(ScopeInputs, "scope `other`")],
        ),
        (
            "a scope input carries what the host declares",
            scoped("post", "gauges", "src.metrics"),
            vec![(ScopeInputs, "scope `post`")],
        ),
    ];
    for (name, value, expected) in cases {
        assert_findings(name, &validate(&value), &expected);
    }
}

#[test]
fn scopes_are_closed() {
    use Rule::ScopeClosed;

    let with_scopes = |mut value: Value, scopes: Value| {
        value["scopes"] = scopes;
        value
    };
    let enrich = |id: &str| n(id, "host_enrichment", json!({}));
    let chain = || {
        plan(
            vec![otlp("src"), enrich("x"), enrich("y"), emit("e", "a")],
            &["src.metrics -> x.in", "x.out -> y.in", "y.out -> e.in"],
            &["a"],
        )
    };
    let merged = || {
        plan(
            vec![
                otlp("src"),
                n("src2", "source", json!({"binding": "liveness"})),
                merge("m", 2),
                emit("e", "a"),
            ],
            &["src.metrics -> m.0", "src2.metrics -> m.1", "m.out -> e.in"],
            &["a"],
        )
    };
    let rejoined = || {
        plan(
            vec![
                otlp("src"),
                split("s", name_prefix()),
                enrich("x"),
                merge("m", 2),
                emit("e", "a"),
            ],
            &[
                "src.metrics -> s.in",
                "s.true -> m.0",
                "s.false -> x.in",
                "x.out -> m.1",
                "m.out -> e.in",
            ],
            &["a"],
        )
    };

    let cases: Vec<Case> = vec![
        (
            "a scope without exits extends to the ends of its paths",
            with_scopes(chain(), json!({"post": {"inputs": {"metrics": "src.metrics"}}})),
            vec![],
        ),
        (
            "a scope with an exit ends there",
            with_scopes(
                chain(),
                json!({"post": {"inputs": {"metrics": "src.metrics"}, "exits": {"enriched": "y.in"}}}),
            ),
            vec![],
        ),
        (
            "nothing outside a scope feeds it except through its inputs",
            with_scopes(merged(), json!({"post": {"inputs": {"metrics": "src.metrics"}}})),
            vec![(ScopeClosed, "scope `post`")],
        ),
        (
            "a scope may have several inputs",
            with_scopes(
                merged(),
                json!({"post": {"inputs": {"metrics": "src.metrics", "gauges": "src2.metrics"}}}),
            ),
            vec![],
        ),
        (
            "an exit refers to an input port",
            with_scopes(
                chain(),
                json!({"post": {"inputs": {"metrics": "src.metrics"}, "exits": {"enriched": "y.out"}}}),
            ),
            vec![(ScopeClosed, "scope `post`")],
        ),
        (
            "an exit is fed from inside its scope",
            with_scopes(
                merged(),
                json!({"post": {"inputs": {"metrics": "src.metrics"}, "exits": {"merged": "m.0", "other": "m.1"}}}),
            ),
            vec![(ScopeClosed, "scope `post`")],
        ),
        (
            "an exit leads out of its scope",
            with_scopes(
                rejoined(),
                json!({"post": {"inputs": {"metrics": "src.metrics"}, "exits": {"back": "m.1"}}}),
            ),
            vec![(ScopeClosed, "scope `post`")],
        ),
        (
            "scopes do not overlap",
            with_scopes(
                chain(),
                json!({"post": {"inputs": {"metrics": "src.metrics"}}, "pre": {"inputs": {"metrics": "x.out"}}}),
            ),
            vec![(ScopeClosed, "scope `pre`"), (ScopeClosed, "scope `pre`")],
        ),
        (
            "scopes that meet at an exit do not overlap",
            with_scopes(
                chain(),
                json!({"post": {"inputs": {"metrics": "x.out"}},
                       "pre": {"inputs": {"metrics": "src.metrics"}, "exits": {"enriched": "y.in"}}}),
            ),
            vec![],
        ),
        (
            "an exit behind a node whose parameters are wrong is not reported unfed as well",
            with_scopes(
                plan(
                    vec![
                        otlp("src"),
                        n("x", "host_enrichment", json!({"extra": 1})),
                        enrich("y"),
                        emit("e", "a"),
                    ],
                    &["src.metrics -> x.in", "x.out -> y.in", "y.out -> e.in"],
                    &["a"],
                ),
                json!({"pre": {"inputs": {"metrics": "src.metrics"}, "exits": {"enriched": "y.in"}}}),
            ),
            vec![(Rule::ValidParameters, "node `x`")],
        ),
    ];
    for (name, value, expected) in cases {
        assert_findings(name, &validate(&value), &expected);
    }

    let report = validate(&with_scopes(
        merged(),
        json!({"post": {"inputs": {"metrics": "src.metrics"}}}),
    ));
    assert!(
        report.findings()[0]
            .message
            .contains("`m.1` is inside the scope, but is fed from `src2.metrics`, outside it"),
        "{}",
        report
    );
}

#[test]
fn duplicate_analysis_follows_the_specification() {
    use Rule::{CatalogOutput, DuplicateContribution};

    let host_tags = n("w", "host_tags", json!({"tags": ["zone:a"], "deadline": "1700000300s"}));
    let prefix = n(
        "w",
        "namespace_prefix",
        json!({"namespace": "tenant", "exclusions": []}),
    );
    let opaque = n(
        "o",
        "opaque_host",
        json!({"component": "fanout", "input": {"kinds": "all", "timestamps": "mixed"},
               "outputs": {"x": {"kinds": "all", "timestamps": "mixed"},
                           "y": {"kinds": "all", "timestamps": "mixed"}}}),
    );

    let cases: Vec<Case> = vec![
        // Divergence at a tee, or at outputs declared as copies: rejected.
        (
            "a tee into two emits of one output is rejected",
            plan(
                vec![otlp("src"), tee("t", 2), emit("e1", "a"), emit("e2", "a")],
                &["src.metrics -> t.in", "t.0 -> e1.in", "t.1 -> e2.in"],
                &["a"],
            ),
            vec![(DuplicateContribution, "node `t`")],
        ),
        (
            "outputs that share a recipient count as one",
            plan(
                vec![otlp("src"), tee("t", 2), emit("e1", "shared_1"), emit("e2", "shared_2")],
                &["src.metrics -> t.in", "t.0 -> e1.in", "t.1 -> e2.in"],
                &["shared_1", "shared_2"],
            ),
            vec![(DuplicateContribution, "node `t`")],
        ),
        (
            "a plan cannot hide a shared recipient from the analysis: the catalog's recipients are checked",
            {
                let mut value = plan(
                    vec![otlp("src"), tee("t", 2), emit("e1", "shared_1"), emit("e2", "shared_2")],
                    &["src.metrics -> t.in", "t.0 -> e1.in", "t.1 -> e2.in"],
                    &["shared_1", "shared_2"],
                );
                // The catalog says `shared_2` delivers to `out/other` and `out/shared`; the plan leaves the shared one
                // out, so the two outputs no longer look like one to the duplicate analysis.
                value["outputs"][1]["recipients"] = json!(["out/other"]);
                value
            },
            vec![(CatalogOutput, "output `shared_2`")],
        ),
        (
            "a tee into outputs with distinct recipients is valid",
            plan(
                vec![otlp("src"), tee("t", 2), emit("e1", "a"), emit("e2", "b")],
                &["src.metrics -> t.in", "t.0 -> e1.in", "t.1 -> e2.in"],
                &["a", "b"],
            ),
            vec![],
        ),
        (
            "copies that reconverge through a merge are rejected",
            plan(
                vec![otlp("src"), tee("t", 2), merge("m", 2), emit("e", "a")],
                &["src.metrics -> t.in", "t.0 -> m.0", "t.1 -> m.1", "m.out -> e.in"],
                &["a"],
            ),
            vec![(DuplicateContribution, "node `t`")],
        ),
        (
            "an opaque host's outputs are copies",
            plan(
                vec![otlp("src"), opaque, merge("m", 2), emit("e", "a")],
                &["src.metrics -> o.in", "o.x -> m.0", "o.y -> m.1", "m.out -> e.in"],
                &["a"],
            ),
            vec![(DuplicateContribution, "node `o`")],
        ),
        (
            "a tee where one copy is dropped is valid",
            plan(
                vec![otlp("src"), tee("t", 2), emit("e", "a"), drop("d")],
                &["src.metrics -> t.in", "t.0 -> e.in", "t.1 -> d.in"],
                &["a"],
            ),
            vec![],
        ),
        (
            "nested tees are rejected at the divergence of the duplicate paths",
            plan(
                vec![
                    otlp("src"),
                    tee("t1", 2),
                    tee("t2", 2),
                    emit("e1", "a"),
                    emit("e2", "b"),
                    emit("e3", "a"),
                ],
                &[
                    "src.metrics -> t1.in",
                    "t1.0 -> e1.in",
                    "t1.1 -> t2.in",
                    "t2.0 -> e2.in",
                    "t2.1 -> e3.in",
                ],
                &["a", "b"],
            ),
            vec![(DuplicateContribution, "node `t1`")],
        ),
        // Divergence at opposite ports of one split, at different sources, or at point-disjoint ports: disjoint.
        (
            "opposite ports of one split are disjoint",
            plan(
                vec![otlp("src"), split("s", name_prefix()), merge("m", 2), emit("e", "a")],
                &[
                    "src.metrics -> s.in",
                    "s.true -> m.0",
                    "s.false -> m.1",
                    "m.out -> e.in",
                ],
                &["a"],
            ),
            vec![],
        ),
        (
            "different sources are disjoint",
            plan(
                vec![
                    otlp("src"),
                    n("src2", "source", json!({"binding": "checks"})),
                    merge("m", 2),
                    emit("e", "a"),
                ],
                &["src.metrics -> m.0", "src2.metrics -> m.1", "m.out -> e.in"],
                &["a"],
            ),
            vec![],
        ),
        (
            "point-disjoint ports of a timestamp split are disjoint",
            plan(
                vec![
                    dogstatsd("src", "mixed"),
                    n("ts", "timestamp_split", json!({"window": "10s"})),
                    merge("m", 2),
                    emit("e", "a"),
                ],
                &[
                    "src.metrics -> ts.in",
                    "ts.default -> m.0",
                    "ts.passthrough -> m.1",
                    "m.out -> e.in",
                ],
                &["a"],
            ),
            vec![],
        ),
        // The exception: complementary splits on equal predicates, with no intervening writer of their reads.
        (
            "complementary splits on equal predicates separate the copies",
            complementary(name_prefix(), name_prefix(), "true", "false", None, None),
            vec![],
        ),
        (
            "predicates are compared in canonical form",
            complementary(
                json!({"any": [{"has_tag": "env"}, {"any": [{"name": {"in": ["y", "x"]}}]}]}),
                json!({"any": [{"name": {"in": ["x", "y", "x"]}}, {"has_tag": "env"}]}),
                "true",
                "false",
                None,
                None,
            ),
            vec![],
        ),
        (
            "splits taking the same port do not separate the copies",
            complementary(name_prefix(), name_prefix(), "true", "true", None, None),
            vec![(DuplicateContribution, "node `t`")],
        ),
        (
            "splits on different predicates do not separate the copies",
            complementary(
                name_prefix(),
                json!({"name": {"prefix": "b."}}),
                "true",
                "false",
                None,
                None,
            ),
            vec![(DuplicateContribution, "node `t`")],
        ),
        (
            "an operator that writes a field the predicate reads voids the exception",
            complementary(
                name_prefix(),
                name_prefix(),
                "true",
                "false",
                Some(prefix.clone()),
                None,
            ),
            vec![(DuplicateContribution, "node `t`")],
        ),
        (
            "an operator that writes the predicate's reads on the other copy voids the exception too",
            complementary(
                name_prefix(),
                name_prefix(),
                "true",
                "false",
                None,
                Some(prefix.clone()),
            ),
            vec![(DuplicateContribution, "node `t`")],
        ),
        (
            "an operator that writes only other fields keeps the exception",
            complementary(
                name_prefix(),
                name_prefix(),
                "true",
                "false",
                Some(host_tags.clone()),
                None,
            ),
            vec![],
        ),
        (
            "an operator that writes only other fields on the other copy keeps the exception",
            complementary(name_prefix(), name_prefix(), "true", "false", None, Some(host_tags)),
            vec![],
        ),
        (
            "aggregation writes the kind of counters, which voids an exception on kind",
            {
                let mut value = complementary(
                    json!({"kind_in": ["gauge"]}),
                    json!({"kind_in": ["gauge"]}),
                    "true",
                    "false",
                    Some(aggregate("w")),
                    None,
                );
                value["nodes"][0] = dogstatsd("src", "untimestamped");
                value
            },
            vec![(DuplicateContribution, "node `t`")],
        ),
        (
            "aggregation does not write the kind of gauges, so the exception holds for gauges alone",
            {
                let mut value = complementary(
                    json!({"kind_in": ["gauge"]}),
                    json!({"kind_in": ["gauge"]}),
                    "true",
                    "false",
                    Some(aggregate("w")),
                    None,
                );
                value["nodes"][0] = n("src", "source", json!({"binding": "liveness"}));
                value
            },
            vec![],
        ),
        (
            "copies that reconverge before one split are not separated by it",
            plan(
                vec![
                    otlp("src"),
                    tee("t", 2),
                    merge("m", 2),
                    split("s", name_prefix()),
                    merge("m2", 2),
                    emit("e", "a"),
                ],
                &[
                    "src.metrics -> t.in",
                    "t.0 -> m.0",
                    "t.1 -> m.1",
                    "m.out -> s.in",
                    "s.true -> m2.0",
                    "s.false -> m2.1",
                    "m2.out -> e.in",
                ],
                &["a"],
            ),
            vec![(DuplicateContribution, "node `t`")],
        ),
        // Contribution is tracked through stateful operators.
        (
            "two copies aggregated into one output are rejected",
            plan(
                vec![
                    dogstatsd("src", "untimestamped"),
                    tee("t", 2),
                    merge("m", 2),
                    aggregate("agg"),
                    emit("e", "a"),
                ],
                &[
                    "src.metrics -> t.in",
                    "t.0 -> m.0",
                    "t.1 -> m.1",
                    "m.out -> agg.in",
                    "agg.out -> e.in",
                ],
                &["a"],
            ),
            vec![(DuplicateContribution, "node `t`")],
        ),
        (
            "a tee into fine-grained and renamed aggregations of the same metrics is rejected by this analysis",
            plan(
                vec![
                    dogstatsd("src", "untimestamped"),
                    tee("t", 2),
                    aggregate("fine"),
                    n(
                        "rename",
                        "namespace_prefix",
                        json!({"namespace": "wide", "exclusions": []}),
                    ),
                    aggregate("wide"),
                    merge("m", 2),
                    emit("e", "a"),
                ],
                &[
                    "src.metrics -> t.in",
                    "t.0 -> fine.in",
                    "t.1 -> rename.in",
                    "rename.out -> wide.in",
                    "fine.out -> m.0",
                    "wide.out -> m.1",
                    "m.out -> e.in",
                ],
                &["a"],
            ),
            vec![(DuplicateContribution, "node `t`")],
        ),
    ];
    for (name, value, expected) in cases {
        assert_findings(name, &validate(&value), &expected);
    }
}

#[test]
fn duplicate_findings_name_both_paths_and_the_divergence() {
    let report = validate(&complementary(name_prefix(), name_prefix(), "true", "true", None, None));
    let finding = report.findings().first().unwrap();
    assert_eq!(finding.rule, Rule::DuplicateContribution);
    assert_eq!(finding.location.node().map(ToString::to_string).as_deref(), Some("t"));
    for fragment in [
        "output `a`",
        "`src.metrics -> t.0 -> s1.true -> e1`",
        "`src.metrics -> t.1 -> s2.true -> e2`",
        "at `tee`, whose output ports `0` and `1` carry copies",
    ] {
        assert!(finding.message.contains(fragment), "{}: {}", fragment, finding.message);
    }

    let shared = validate(&plan(
        vec![otlp("src"), tee("t", 2), emit("e1", "shared_1"), emit("e2", "shared_2")],
        &["src.metrics -> t.in", "t.0 -> e1.in", "t.1 -> e2.in"],
        &["shared_1", "shared_2"],
    ));
    assert!(
        shared.findings()[0]
            .message
            .contains("outputs `shared_1`, `shared_2`, which share a recipient,"),
        "{}",
        shared
    );
}

/// Builds a chain of `stages` tee-then-merge pairs, each doubling the number of paths, from a source to `last`.
fn doubling_chain(stages: usize, node: fn(&str, &str, Value) -> Value, last: Value) -> Value {
    let mut nodes = vec![otlp("src")];
    let mut edges = Vec::new();
    let mut previous = "src.metrics".to_string();
    for stage in 0..stages {
        let (t, m) = (format!("t{}", stage), format!("m{}", stage));
        nodes.push(node(&t, "tee", json!({"ports": 2})));
        nodes.push(node(&m, "merge", json!({"inputs": 2})));
        edges.push(format!("{} -> {}.in", previous, t));
        edges.push(format!("{}.0 -> {}.0", t, m));
        edges.push(format!("{}.1 -> {}.1", t, m));
        previous = format!("{}.out", m);
    }
    let last_id = last["id"].as_str().unwrap().to_string();
    nodes.push(last);
    edges.push(format!("{} -> {}.in", previous, last_id));
    let edges = edges.iter().map(String::as_str).collect::<Vec<_>>();
    plan(nodes, &edges, &["a"])
}

#[test]
fn duplicate_analysis_is_bounded_by_the_work_limit() {
    // 13 doublings give 8,192 paths to the output, more than the limit of 4,096.
    let report = validate(&doubling_chain(13, n, emit("e", "a")));
    assert_findings(
        "too many paths",
        &report,
        &[(Rule::DuplicateAnalysisLimit, "output `a`")],
    );

    // 12 doublings give 4,096 paths: the analysis completes, and rejects the copies at the first tee.
    let report = validate(&doubling_chain(12, n, emit("e", "a")));
    assert_eq!(report.findings()[0].rule, Rule::DuplicateContribution);
    assert_eq!(report.findings()[0].location.to_string(), "node `t0`");
}

/// A tee whose two copies take opposite ports of splits on one predicate, then each pass `stages` diamonds of a split
/// on a long predicate rejoined by a merge, before an emit to output `a`: `2 * 2^stages` paths reach the output.
fn complementary_diamonds(stages: usize) -> Value {
    // Long predicates whose canonical forms share a long prefix, about 10 KB each.
    let long = |stage: usize| {
        let values = (0..40)
            .map(|value| format!("{}_{}_{}", "x".repeat(240), stage, value))
            .collect::<Vec<_>>();
        json!({"name": {"in": values}})
    };
    let mut nodes = vec![otlp("src"), tee("t", 2)];
    let mut edges = vec!["src.metrics -> t.in".to_string()];
    for (side, port, kept) in [("l", 0, "true"), ("r", 1, "false")] {
        let dropped = if kept == "true" { "false" } else { "true" };
        nodes.push(split(&format!("{}s", side), name_prefix()));
        nodes.push(drop(&format!("{}d", side)));
        edges.push(format!("t.{} -> {}s.in", port, side));
        edges.push(format!("{}s.{} -> {}d.in", side, dropped, side));
        let mut previous = format!("{}s.{}", side, kept);
        for stage in 0..stages {
            let (q, m) = (format!("{}q{}", side, stage), format!("{}m{}", side, stage));
            nodes.push(split(&q, long(stage)));
            nodes.push(merge(&m, 2));
            edges.push(format!("{} -> {}.in", previous, q));
            edges.push(format!("{}.true -> {}.0", q, m));
            edges.push(format!("{}.false -> {}.1", q, m));
            previous = format!("{}.out", m);
        }
        nodes.push(emit(&format!("{}e", side), "a"));
        edges.push(format!("{} -> {}e.in", previous, side));
    }
    let edges = edges.iter().map(String::as_str).collect::<Vec<_>>();
    plan(nodes, &edges, &["a"])
}

#[test]
fn duplicate_analysis_stays_fast_with_long_predicates() {
    // A review probe of the first implementation: 4,096 paths, each pair compared by the predicates' canonical text,
    // took about 5 seconds in a release build (and ten times that in a debug build) for a 234 KB document within every
    // artifact limit. Predicates are now numbered once, and copies separated by one complementary pair of splits are
    // accepted without comparing their paths pair by pair.
    let value = complementary_diamonds(11);
    let started = std::time::Instant::now();
    let report = validate(&value);
    let elapsed = started.elapsed();
    assert_findings("complementary diamonds", &report, &[]);
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "validation took {:?}",
        elapsed
    );
}

/// Two copies that complementary splits separate pair by pair, but by no single predicate: each copy crosses `p` and
/// `q`, and every pair of paths crosses one of them by opposite ports.
fn separated_pair_by_pair() -> Value {
    let q = json!({"name": {"prefix": "q."}});
    let mut nodes = vec![otlp("src"), tee("t", 2)];
    let mut edges = vec!["src.metrics -> t.in".to_string()];
    // The left copy crosses (p true, q true) or (p false, q false); the right one (p true, q false) or (p false, q
    // true).
    for (side, port, q_on_true, q_on_false) in [("l", 0, "true", "false"), ("r", 1, "false", "true")] {
        let other = |kept: &str| if kept == "true" { "false" } else { "true" };
        nodes.push(split(&format!("{}p", side), name_prefix()));
        nodes.push(merge(&format!("{}m", side), 2));
        edges.push(format!("t.{} -> {}p.in", port, side));
        for (branch, kept, input) in [("true", q_on_true, 0), ("false", q_on_false, 1)] {
            let id = format!("{}q{}", side, branch);
            nodes.push(split(&id, q.clone()));
            nodes.push(drop(&format!("{}_d", id)));
            edges.push(format!("{}p.{} -> {}.in", side, branch, id));
            edges.push(format!("{}.{} -> {}m.{}", id, kept, side, input));
            edges.push(format!("{}.{} -> {}_d.in", id, other(kept), id));
        }
        nodes.push(emit(&format!("{}e", side), "a"));
        edges.push(format!("{}m.out -> {}e.in", side, side));
    }
    let edges = edges.iter().map(String::as_str).collect::<Vec<_>>();
    plan(nodes, &edges, &["a"])
}

#[test]
fn duplicate_analysis_charges_pair_comparisons_against_its_work_limit() {
    let value = separated_pair_by_pair();
    assert_findings("separated pair by pair", &validate(&value), &[]);

    // Each copy has two distinct crossing sets, so the analysis compares four pairs. With a budget of three, it gives
    // up and says so; with four, it completes.
    let plan = Plan::from_value(value).unwrap();
    let registry = OperatorRegistry::native();
    let analyze = |budget: u64| {
        let mut report = ValidationReport::default();
        let graph = graph::Graph::build(&plan, &registry, &mut report);
        let types = types::check(&graph, &plan, &mut report);
        assert!(report.findings().is_empty(), "{}", report);
        duplicates::check_within(&graph, &types, &plan, budget, &mut report);
        report
    };
    assert_findings(
        "a budget of 3",
        &analyze(3),
        &[(Rule::DuplicateAnalysisLimit, "output `a`")],
    );
    assert!(analyze(3).findings()[0].message.contains("within its work limit"));
    assert_findings("a budget of 4", &analyze(4), &[]);
}

#[test]
fn bounds_apply_to_artifact_nodes_only() {
    use Rule::*;

    // A chain of 257 artifact nodes is one more than the limit of 256.
    let chain = |count: usize, node: fn(&str, &str, Value) -> Value| {
        let mut nodes = vec![otlp("src")];
        let mut edges = Vec::new();
        let mut previous = "src.metrics".to_string();
        for i in 0..count - 1 {
            let id = format!("x{}", i);
            nodes.push(node(&id, "host_enrichment", json!({})));
            edges.push(format!("{} -> {}.in", previous, id));
            previous = format!("{}.out", id);
        }
        nodes.push(node("e", "emit", json!({"output": "a"})));
        edges.push(format!("{} -> e.in", previous));
        let edges = edges.iter().map(String::as_str).collect::<Vec<_>>();
        plan(nodes, &edges, &["a"])
    };
    assert_findings(
        "257 artifact nodes",
        &validate(&chain(257, a)),
        &[(ArtifactNodes, "plan")],
    );
    assert_findings("256 artifact nodes", &validate(&chain(256, a)), &[]);
    assert_findings("257 native nodes", &validate(&chain(257, n)), &[]);

    // 1,025 predicate leaves are one more than the limit of 1,024.
    let leaves = |count: usize, node: fn(&str, &str, Value) -> Value| {
        let operands = (0..count)
            .map(|i| json!({"has_tag": format!("k{}", i)}))
            .collect::<Vec<_>>();
        plan(
            vec![
                otlp("src"),
                node("s", "split", json!({"predicate": {"any": operands}})),
                node("e", "emit", json!({"output": "a"})),
                node("d", "drop", json!({})),
            ],
            &["src.metrics -> s.in", "s.true -> e.in", "s.false -> d.in"],
            &["a"],
        )
    };
    assert_findings(
        "1,025 leaves",
        &validate(&leaves(1025, a)),
        &[(PredicateLeaves, "plan")],
    );
    assert_findings("1,024 leaves", &validate(&leaves(1024, a)), &[]);
    assert_findings("1,025 native leaves", &validate(&leaves(1025, n)), &[]);

    // A tee with 33 ports is one more than the limit of 32.
    let fan_out = |ports: usize, node: fn(&str, &str, Value) -> Value| {
        let mut nodes = vec![otlp("src"), node("t", "tee", json!({"ports": ports}))];
        let mut edges = vec!["src.metrics -> t.in".to_string()];
        for port in 0..ports {
            nodes.push(node(&format!("d{}", port), "drop", json!({})));
            edges.push(format!("t.{} -> d{}.in", port, port));
        }
        let edges = edges.iter().map(String::as_str).collect::<Vec<_>>();
        plan(nodes, &edges, &[])
    };
    assert_findings("33 ports", &validate(&fan_out(33, a)), &[(TeeFanOut, "node `t`")]);
    assert_findings("32 ports", &validate(&fan_out(32, a)), &[]);
    assert_findings("33 native ports", &validate(&fan_out(33, n)), &[]);

    // Each tee-then-merge stage ending in a drop costs `3 + 2 * (cost of the rest)` visits, starting from 5 for the
    // last stage: 10 stages cost 4,093 visits, within the limit of 4,096, and 11 stages cost 8,189.
    let ending_in_drop = |stages: usize, node: fn(&str, &str, Value) -> Value| {
        let mut value = doubling_chain(stages, node, node("d", "drop", json!({})));
        value["outputs"] = json!([]);
        value
    };
    let report = validate(&ending_in_drop(11, a));
    assert_findings("11 stages", &report, &[(WorkPerInput, "node `t0`")]);
    assert!(report.findings()[0].message.contains("8189 node visits"), "{}", report);
    assert_findings("10 stages", &validate(&ending_in_drop(10, a)), &[]);
    assert_findings("11 native stages", &validate(&ending_in_drop(11, n)), &[]);
}

/// An aggregation with native provenance and the given window, which may be a binding.
fn aggregate_with_window(id: &str, window: Value) -> Value {
    let mut node = aggregate(id);
    node["params"]["window"] = window;
    node
}

#[test]
fn state_rules() {
    use Rule::*;

    // Two state owners fed by one source, each delivering to its own output.
    let two_owners = |first: Value, second: Value| {
        plan(
            vec![
                otlp("src"),
                tee("t", 2),
                aggregate_with_window("agg_1", first),
                aggregate_with_window("agg_2", second),
                emit("e1", "a"),
                emit("e2", "b"),
            ],
            &[
                "src.metrics -> t.in",
                "t.0 -> agg_1.in",
                "t.1 -> agg_2.in",
                "agg_1.out -> e1.in",
                "agg_2.out -> e2.in",
            ],
            &["a", "b"],
        )
    };
    // A timestamp split whose untimestamped points go to an aggregation, through a map, and whose timestamped points
    // bypass it.
    let split_into = |split_window: Value, aggregation_window: Value| {
        plan(
            vec![
                dogstatsd("src", "mixed"),
                n("ts", "timestamp_split", json!({"window": split_window})),
                n("hosts", "host_enrichment", json!({})),
                aggregate_with_window("agg", aggregation_window),
                emit("e1", "a"),
                emit("e2", "b"),
            ],
            &[
                "src.metrics -> ts.in",
                "ts.default -> hosts.in",
                "hosts.out -> agg.in",
                "agg.out -> e1.in",
                "ts.passthrough -> e2.in",
            ],
            &["a", "b"],
        )
    };
    let bound = json!({"config": "test.window"});
    let cases: Vec<Case> = vec![
        (
            "widths up to sixty seconds",
            two_owners(json!("30s"), json!("1m")),
            vec![],
        ),
        (
            "one width above sixty seconds",
            two_owners(json!("2m"), json!("1m")),
            vec![],
        ),
        (
            "one width above sixty seconds, twice",
            two_owners(json!("5m"), json!("5m")),
            vec![],
        ),
        (
            "two widths above sixty seconds",
            two_owners(json!("2m"), json!("3m")),
            vec![(WidthsAboveSixty, "node `agg_2`")],
        ),
        // A width bound to configuration and not resolved is unknown, so nothing is compared.
        ("an unknown width", two_owners(json!("2m"), bound.clone()), vec![]),
        (
            "a split with its aggregation's window",
            split_into(json!("10s"), json!("10s")),
            vec![],
        ),
        (
            "a split with another window than its aggregation's",
            split_into(json!("10s"), json!("30s")),
            vec![(SplitWindow, "node `ts`")],
        ),
        // One binding has one value, whatever it is.
        (
            "a split bound to its aggregation's window",
            split_into(bound.clone(), bound.clone()),
            vec![],
        ),
        (
            "a split bound to the resolved value of its aggregation's window",
            split_into(json!({"config": "test.window", "resolved": "10s"}), json!("10s")),
            vec![],
        ),
        (
            "a split bound to another resolved value",
            split_into(json!({"config": "test.window", "resolved": "20s"}), json!("10s")),
            vec![(SplitWindow, "node `ts`")],
        ),
        (
            "a split whose window is not known",
            split_into(bound, json!("10s")),
            vec![],
        ),
    ];
    for (name, value, expected) in &cases {
        assert_findings(name, &validate(value), expected);
    }

    let report = validate(&two_owners(json!("2m"), json!("3m")));
    assert_eq!(
        report.findings()[0].message,
        "the node buckets by 180 s, and `agg_1` by 120 s: a plan uses at most one bucket width above 60 seconds, so \
         that its widths line up at least every max(60 s, largest width); use 120 s here too, or a width of at most 60 \
         seconds"
    );
    let report = validate(&split_into(json!("10s"), json!("30s")));
    assert!(
        report.findings()[0]
            .message
            .starts_with("the split gives timestamped counters a rate interval of 10 s, but `agg`"),
        "{}",
        report
    );
}

#[test]
fn an_artifacts_context_limit_above_the_hosts_state_budget_is_a_warning() {
    use Rule::*;

    // Aggregation behind an isolation fence, which artifacts may place; its context limit is 1,000.
    let registry = crate::operators::OperatorRegistry::native();
    let with_owner = |node: fn(&str, &str, Value) -> Value| {
        let mut owner = aggregate("agg");
        owner = node("agg", "aggregate", owner["params"].clone());
        plan(
            vec![otlp("src"), owner, emit("e", "a")],
            &["src.metrics -> agg.in", "agg.out -> e.in"],
            &["a"],
        )
    };
    let validate_with = |value: &Value, catalog: &MemoryCatalog| {
        Validator::new(&registry, catalog).validate(&Plan::from_value(value.clone()).unwrap())
    };

    let tight = test_catalog().with_state_budget(999);
    let report = validate_with(&with_owner(a), &tight);
    assert_findings(
        "an artifact's limit above the budget",
        &report,
        &[(StateBudget, "node `agg`")],
    );
    assert!(report.is_valid(), "a warning leaves the plan valid");
    assert_eq!(
        report.findings()[0].message,
        "the node's context limit is 1000, but the host's state owners hold at most 999 contexts together, so it holds \
         at most that many, and fewer while other state owners hold some; lower the limit to at most 999"
    );

    let cases = [
        (
            "an artifact's limit at the budget",
            with_owner(a),
            test_catalog().with_state_budget(1000),
        ),
        // The budget is the default program's limit, so a native node is never above it, and the rule is about
        // artifacts.
        (
            "a native limit above the budget",
            with_owner(n),
            test_catalog().with_state_budget(999),
        ),
        ("no budget", with_owner(a), test_catalog()),
    ];
    for (name, value, catalog) in &cases {
        assert_findings(name, &validate_with(value, catalog), &[]);
    }
}

#[test]
fn authority_rules() {
    use Rule::*;

    let cases: Vec<Case> = vec![
        (
            "a drop needs provenance",
            plan(
                vec![otlp("src"), bare("d", "drop", json!({}))],
                &["src.metrics -> d.in"],
                &[],
            ),
            vec![(LossAuthority, "node `d`")],
        ),
        (
            "a tag projection needs provenance",
            plan(
                vec![
                    otlp("src"),
                    bare(
                        "tags",
                        "tag_projection",
                        json!({"rules": [], "value_rules": [], "cache_capacity": 1}),
                    ),
                    emit("e", "a"),
                ],
                &["src.metrics -> tags.in", "tags.out -> e.in"],
                &["a"],
            ),
            vec![(LossAuthority, "node `tags`")],
        ),
        (
            "a lossless node needs no provenance",
            plan(
                vec![
                    otlp("src"),
                    bare("s", "split", json!({"predicate": {"const": true}})),
                    emit("e", "a"),
                    drop("d"),
                ],
                &["src.metrics -> s.in", "s.true -> e.in", "s.false -> d.in"],
                &["a"],
            ),
            vec![],
        ),
        (
            "an artifact drop has authority",
            plan(
                vec![otlp("src"), a("d", "drop", json!({}))],
                &["src.metrics -> d.in"],
                &[],
            ),
            vec![],
        ),
        (
            "an artifact revision a node names is one the plan lists",
            {
                let mut value = plan(
                    vec![otlp("src"), a("d", "drop", json!({}))],
                    &["src.metrics -> d.in"],
                    &[],
                );
                value["nodes"][1]["provenance"]["artifact"]["revision"] = json!(999);
                value
            },
            vec![(ProvenanceConsistency, "node `d`")],
        ),
        (
            "a plan without provenance lists no artifact revision",
            {
                let mut value = plan(
                    vec![otlp("src"), a("d", "drop", json!({}))],
                    &["src.metrics -> d.in"],
                    &[],
                );
                value.as_object_mut().unwrap().remove("provenance");
                value
            },
            vec![(ProvenanceConsistency, "node `d`")],
        ),
        (
            "an artifact cannot add a source",
            plan(
                vec![a("src", "source", json!({"binding": "otlp"})), emit("e", "a")],
                &["src.metrics -> e.in"],
                &["a"],
            ),
            vec![(TrustedHost, "node `src`")],
        ),
        (
            "an artifact cannot add an opaque host",
            plan(
                vec![
                    otlp("src"),
                    a(
                        "o",
                        "opaque_host",
                        json!({"component": "x", "input": {"kinds": "all", "timestamps": "mixed"},
                               "outputs": {"out": {"kinds": "all", "timestamps": "mixed"}}}),
                    ),
                    emit("e", "a"),
                ],
                &["src.metrics -> o.in", "o.out -> e.in"],
                &["a"],
            ),
            vec![(TrustedHost, "node `o`")],
        ),
    ];
    for (name, value, expected) in cases {
        assert_findings(name, &validate(&value), &expected);
    }
}

#[test]
fn every_finding_is_reported_not_only_the_first() {
    use Rule::*;

    let report = validate(&plan(
        vec![
            otlp("src"),
            tee("t", 2),
            emit("e1", "a"),
            emit("e2", "a"),
            bare("d", "drop", json!({})),
            n("x", "regex_filter", json!({})),
        ],
        &["src.metrics -> t.in", "t.0 -> e1.in", "t.1 -> e2.in", "x.out -> d.in"],
        &["a", "b"],
    ));
    assert_findings(
        "several problems",
        &report,
        &[
            (KnownOperator, "node `x`"),
            (Reachable, "node `d`"),
            (Reachable, "node `x`"),
            (UnusedOutput, "output `b`"),
            (DuplicateContribution, "node `t`"),
            (LossAuthority, "node `d`"),
        ],
    );
    assert!(!report.is_valid());
    assert_eq!(report.errors().count(), 5);
    assert_eq!(report.warnings().count(), 1);
    assert!(report
        .to_string()
        .starts_with("error[known_operator] node `x`: Unknown operator `regex_filter`"));
}

#[test]
fn every_rule_has_a_stable_name() {
    let names = Rule::ALL
        .iter()
        .map(|rule| rule.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(names.len(), Rule::ALL.len());
    for rule in Rule::ALL {
        assert!(crate::ir::is_identifier(rule.as_str()), "{}", rule);
        assert_eq!(rule.to_string(), rule.as_str());
    }
    let warnings = Rule::ALL
        .iter()
        .filter(|rule| rule.severity() == Severity::Warning)
        .map(|rule| rule.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        warnings,
        vec![
            "predicate_can_match",
            "port_can_carry",
            "unused_output",
            "live_binding_on_host",
            "state_budget"
        ]
    );
}
