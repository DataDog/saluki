//! Tests of receipt and composition.
//!
//! Artifacts are edits of the default program's export, written by hand as changes to its JSON document. Expected
//! plan IDs, errors, and diffs are written by hand from the default-program fixtures.

use std::collections::{BTreeMap, BTreeSet};

use proptest::{collection::vec, prelude::*, sample::select};
use serde_json::{json, Value};

use super::*;
use crate::{
    elaborate::elaborate_default,
    export::{emitted_output, export, export_as, unresolved, ExportBundle, ExportForm},
    install::diff::{diff, NodeChange},
    ir::{ArtifactPortRef, ArtifactRevision, Plan, PlanProvenance},
    testing::{arb_predicate, catalog_for, predicate_json, DEFAULT_PROGRAM_JSON, NO_AGGREGATION_PIPELINE_OFF_JSON},
    validate::{BoundScope, CatalogBinding, Location, MemoryCatalog, Rule, Validator},
};

/// A change made to an artifact document, for table-driven tests.
type Mutation = fn(&mut Value);

/// The default-program fixtures, with the host's catalog of each, carrying every binding's value.
fn default_programs() -> Vec<(&'static str, Plan, MemoryCatalog)> {
    [
        ("default program", DEFAULT_PROGRAM_JSON),
        ("no-aggregation pipeline off", NO_AGGREGATION_PIPELINE_OFF_JSON),
    ]
    .into_iter()
    .map(|(name, json)| {
        let plan = Plan::from_json(json).unwrap();
        let catalog = catalog_for(&plan);
        (name, plan, catalog)
    })
    .collect()
}

/// A default program as lowering writes it, the host's catalog for it, its export, and its core plan.
struct Fixture {
    plan: Plan,
    catalog: MemoryCatalog,
    export: ExportBundle,
    core: Plan,
}

impl Fixture {
    fn new(plan: Plan) -> Self {
        let catalog = catalog_for(&plan);
        let export = export(&plan, &catalog, &BTreeMap::new()).unwrap();
        let core = elaborate_default(&plan, &catalog, &OperatorRegistry::native()).plan;
        Self {
            plan,
            catalog,
            export,
            core,
        }
    }

    fn default_program() -> Self {
        Self::new(Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap())
    }

    /// Returns `true` if `id` is one of the default program's sources.
    fn is_source(&self, id: &NodeId) -> bool {
        self.plan.node(id).is_some_and(|node| node.op.as_str() == names::SOURCE)
    }

    /// Returns the export's artifact document.
    fn artifact(&self) -> Value {
        serde_json::to_value(&self.export.artifact).unwrap()
    }

    /// Receives and composes an artifact document.
    fn compose(&self, document: Value, approval: &Approval) -> Result<Composed, Refused> {
        let received = receive_artifact(document).map_err(Refused::Receipt)?;
        compose(
            &self.plan,
            &received,
            approval,
            &self.catalog,
            &OperatorRegistry::native(),
        )
        .map_err(Refused::Compose)
    }
}

/// Why an artifact did not compose.
#[derive(Debug)]
enum Refused {
    Receipt(ReceiptError),
    Compose(ComposeError),
}

pub(crate) fn approval(revision: u64) -> Approval {
    Approval {
        revision,
        approved_against: None,
        active_plan_hash: None,
        in_effect: false,
    }
}

fn hash(digit: char) -> PlanHash {
    format!("sha256:{}", digit.to_string().repeat(64)).parse().unwrap()
}

/// Returns the artifact node with the given ID.
fn node<'a>(document: &'a mut Value, id: &str) -> &'a mut Value {
    document["nodes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|node| node["id"] == id)
        .unwrap_or_else(|| panic!("the artifact has a node `{}`", id))
}

/// Removes nodes, and every edge at their ports.
fn remove_nodes(document: &mut Value, ids: &[&str]) {
    let at = |port: &Value| {
        ids.iter()
            .any(|id| port.as_str().unwrap().split('.').next() == Some(*id))
    };
    document["nodes"]
        .as_array_mut()
        .unwrap()
        .retain(|node| !ids.iter().any(|id| node["id"] == *id));
    document["edges"]
        .as_array_mut()
        .unwrap()
        .retain(|edge| !at(&edge["from"]) && !at(&edge["to"]));
}

/// Returns the edge entering `to`.
fn edge_to<'a>(document: &'a mut Value, to: &str) -> &'a mut Value {
    document["edges"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|edge| edge["to"] == to)
        .unwrap_or_else(|| panic!("the artifact has an edge into `{}`", to))
}

/// Renames a node, and the edges at its ports.
fn rename(document: &mut Value, from: &str, to: &str) {
    node(document, from)["id"] = json!(to);
    for edge in document["edges"].as_array_mut().unwrap() {
        for end in ["from", "to"] {
            let port = edge[end].as_str().unwrap().to_string();
            if let Some(rest) = port.strip_prefix(&format!("{}.", from)) {
                edge[end] = json!(format!("{}.{}", to, rest));
            }
        }
    }
}

fn push_node(document: &mut Value, node: Value) {
    document["nodes"].as_array_mut().unwrap().push(node);
}

/// Inserts nodes before the node `before`, in the artifact's order.
fn insert_nodes_before(document: &mut Value, before: &str, inserted: Vec<Value>) {
    let nodes = document["nodes"].as_array_mut().unwrap();
    let at = nodes.iter().position(|node| node["id"] == before).unwrap();
    nodes.splice(at..at, inserted);
}

fn push_edge(document: &mut Value, from: &str, to: &str) {
    document["edges"]
        .as_array_mut()
        .unwrap()
        .push(json!({"from": from, "to": to}));
}

/// Describes how the diff lists a node: `added`, `removed`, or `changed` with what changed.
fn describe(change: &NodeChange) -> String {
    match change {
        NodeChange::Unchanged => "unchanged".to_string(),
        NodeChange::Added => "added".to_string(),
        NodeChange::Removed => "removed".to_string(),
        NodeChange::Changed {
            operator,
            params,
            edges,
        } => {
            let parts = [(*operator, "operator"), (*params, "params"), (*edges, "edges")];
            let parts = parts
                .iter()
                .filter(|(changed, _)| *changed)
                .map(|(_, what)| *what)
                .collect::<Vec<_>>();
            format!("changed {}", parts.join(" "))
        }
    }
}

#[test]
fn unmodified_exports_of_the_default_programs_compose_back_to_them() {
    let registry = OperatorRegistry::native();
    for (name, plan, _) in default_programs() {
        let fixture = Fixture::new(plan);
        let received = receive_artifact(fixture.artifact()).unwrap();
        let composed = compose(&fixture.plan, &received, &approval(7), &fixture.catalog, &registry).unwrap();

        // The same nodes, edges, and outputs as the default program's core plan, in the same order: the same plan, with
        // the same hash.
        let plan = &fixture.core;
        assert_eq!(composed.plan.nodes, plan.nodes, "{}", name);
        assert_eq!(composed.plan.edges, plan.edges, "{}", name);
        assert_eq!(composed.plan.outputs, plan.outputs, "{}", name);
        assert_eq!(composed.plan.hash().unwrap(), plan.hash().unwrap(), "{}", name);
        // Only the plan's own provenance, which is not hashed, says that an artifact was composed.
        assert_eq!(
            composed.plan.provenance,
            Some(PlanProvenance {
                config_change: None,
                artifact: Some(ArtifactRevision {
                    revision: 7,
                    hash: received.hash
                }),
                native_components: Default::default(),
            }),
            "{}",
            name
        );
        assert!(!composed.stale_review, "{}", name);
        assert_eq!(composed.outcome, ComposeOutcome::Composed, "{}", name);
        // The same findings: the program's nodes are measured against the artifact limits too, and are within them.
        let native = Validator::new(&registry, &fixture.catalog).validate(&fixture.plan);
        assert_eq!(composed.report.findings(), native.findings(), "{}", name);
        assert!(
            composed
                .report
                .bounds()
                .iter()
                .any(|bound| matches!(bound.scope, BoundScope::Artifact(_))),
            "{}",
            name
        );

        // Every artifact node takes the reserved ID of the default program's node of the same local ID.
        assert_eq!(composed.ids.len(), fixture.export.artifact.nodes.len(), "{}", name);
        for artifact_node in &fixture.export.artifact.nodes {
            assert_eq!(
                composed.ids[&artifact_node.id],
                NodeId::new(artifact_node.id.to_local_id()),
                "{}",
                name
            );
        }
    }
}

/// What composing an accepted edit gives.
struct Accepted {
    /// Every artifact node the default program does not have, and the plan node it became.
    policy: &'static [(&'static str, &'static str)],

    /// Every node the diff against the default program does not list as unchanged, with how it lists it.
    diff: &'static [(&'static str, &'static str)],
}

#[test]
fn edits_of_the_export_compose_with_their_plan_ids() {
    let kinds_list = json!(["counter", "rate", "gauge", "set"]);
    let kinds_blob = PlanHash::of_value(&kinds_list).unwrap();
    let cases: Vec<(&str, Mutation, Accepted)> = vec![
        ("unmodified", |_| {}, Accepted { policy: &[], diff: &[] }),
        (
            "a removed branch",
            |document| {
                remove_nodes(
                    document,
                    &[
                        "mrf_metrics_gateway",
                        "mrf_metrics_gateway_true",
                        "mrf_metrics_gateway_false",
                    ],
                );
                node(document, "metrics_fanout")["params"]["ports"] = json!(4);
                edge_to(document, "af_metrics_gateway.in")["from"] = json!("metrics_fanout.3");
            },
            Accepted {
                policy: &[],
                diff: &[
                    ("metrics_fanout", "changed params edges"),
                    ("af_metrics_gateway", "changed edges"),
                    ("mrf_metrics_gateway", "removed"),
                    ("mrf_metrics_gateway_true", "removed"),
                    ("mrf_metrics_gateway_false", "removed"),
                ],
            },
        ),
        (
            "a renamed node",
            |document| {
                rename(document, "metrics_routing_filter_0", "team_b");
                rename(document, "metrics_routing_filter_0_true", "team_b_true");
                rename(document, "metrics_routing_filter_0_false", "team_b_false");
            },
            Accepted {
                policy: &[
                    ("team_b", "team_b"),
                    ("team_b_true", "team_b_true"),
                    ("team_b_false", "team_b_false"),
                ],
                diff: &[
                    ("metrics_fanout", "changed edges"),
                    ("team_b", "added"),
                    ("team_b_true", "added"),
                    ("team_b_false", "added"),
                    ("metrics_routing_filter_0", "removed"),
                    ("metrics_routing_filter_0_true", "removed"),
                    ("metrics_routing_filter_0_false", "removed"),
                ],
            },
        ),
        (
            "a new node",
            |document| {
                insert_nodes_before(
                    document,
                    "metrics_fanout",
                    vec![
                        json!({"id": "drop_debug", "op": "split",
                               "params": {"predicate": {"name": {"prefix": "demo.debug."}}}}),
                        json!({"id": "drop_debug_true", "op": "drop"}),
                    ],
                );
                edge_to(document, "metrics_fanout.in")["from"] = json!("drop_debug.false");
                push_edge(document, "metrics.out", "drop_debug.in");
                push_edge(document, "drop_debug.true", "drop_debug_true.in");
            },
            Accepted {
                // Every drop after the new filter now drops what is left after it, but does what it did: only the new
                // nodes change. The filter composes in normal form, `split(not p)` keeping its `true` port, so its
                // drop, named for its `true` port, is renamed for its `false` port.
                policy: &[("drop_debug", "drop_debug"), ("drop_debug_false", "drop_debug_false")],
                diff: &[
                    ("metrics", "changed edges"),
                    ("drop_debug", "added"),
                    ("drop_debug_false", "added"),
                    ("metrics_fanout", "changed edges"),
                ],
            },
        ),
        (
            "the primary stream rerouted into a kept drop",
            |document| {
                // The `tee` port that fed the primary output now feeds the drop of the first routing filter, and the
                // filter's rejects go to a new drop: the kept drop discards the whole primary stream.
                remove_nodes(document, &["metrics_fanout_0"]);
                edge_to(document, "metrics_routing_filter_0_false.in")["from"] = json!("metrics_fanout.0");
                push_node(document, json!({"id": "discard", "op": "drop"}));
                push_edge(document, "metrics_routing_filter_0.false", "discard.in");
            },
            Accepted {
                policy: &[("discard", "discard")],
                diff: &[
                    ("metrics_fanout", "changed edges"),
                    ("metrics_routing_filter_0", "changed edges"),
                    ("metrics_routing_filter_0_false", "changed edges"),
                    ("discard", "added"),
                    ("metrics_fanout_0", "removed"),
                ],
            },
        ),
        (
            "the primary output and a kept drop swapped",
            |document| {
                edge_to(document, "metrics_fanout_0.in")["from"] = json!("metrics_routing_filter_0.false");
                edge_to(document, "metrics_routing_filter_0_false.in")["from"] = json!("metrics_fanout.0");
            },
            Accepted {
                // Both do what they did, whatever feeds them: only edges change.
                policy: &[],
                diff: &[
                    ("metrics_fanout", "changed edges"),
                    ("metrics_fanout_0", "changed edges"),
                    ("metrics_routing_filter_0", "changed edges"),
                    ("metrics_routing_filter_0_false", "changed edges"),
                ],
            },
        ),
        (
            "modified parameters",
            |document| {
                node(document, "af_metrics_gateway")["params"]["predicate"]["all"][0] = json!({"kind_in": ["counter"]});
            },
            Accepted {
                policy: &[],
                diff: &[("af_metrics_gateway", "changed params")],
            },
        ),
        (
            "a binding replaced by a literal",
            |document| {
                node(document, "mrf_metrics_gateway")["params"]["predicate"]["all"][1] = json!({"flag": true});
            },
            // A literal equal to the binding's value elaborates to the same core plan: it does what the binding does
            // now. (It no longer follows the signal, which only a later composition shows.)
            Accepted { policy: &[], diff: &[] },
        ),
        (
            "a binding replaced by another value",
            |document| {
                node(document, "mrf_metrics_gateway")["params"]["predicate"]["all"][1] = json!({"flag": false});
            },
            Accepted {
                policy: &[],
                diff: &[("mrf_metrics_gateway", "changed params")],
            },
        ),
        (
            "parameters written in another order",
            |document| {
                node(document, "af_metrics_gateway")["params"]["predicate"]["all"][0] =
                    json!({"kind_in": ["set", "gauge", "rate", "counter"]});
            },
            Accepted { policy: &[], diff: &[] },
        ),
    ];

    let fixture = Fixture::default_program();
    let registry = OperatorRegistry::native();
    let mut all_cases = cases
        .into_iter()
        .map(|(what, mutate, accepted)| {
            let mut document = fixture.artifact();
            mutate(&mut document);
            (what, document, accepted)
        })
        .collect::<Vec<_>>();

    // A parameter written as a reference to a blob holding the default program's value is unmodified.
    let mut with_blob = fixture.artifact();
    node(&mut with_blob, "af_metrics_gateway")["params"]["predicate"]["all"][0] =
        json!({"kind_in": {"blob": kinds_blob.to_string()}});
    with_blob["blobs"] = json!({kinds_blob.to_string(): kinds_list});
    all_cases.push(("a blob reference", with_blob, Accepted { policy: &[], diff: &[] }));

    for (what, document, expected) in all_cases {
        let received = receive_artifact(document).unwrap_or_else(|e| panic!("{}: {}", what, e));
        let composed = compose(&fixture.plan, &received, &approval(3), &fixture.catalog, &registry)
            .unwrap_or_else(|e| panic!("{}: {}", what, e));
        assert!(composed.report.is_valid(), "{}:\n{}", what, composed.report);
        assert!(!composed.stale_review, "{}", what);

        // Every artifact node's plan ID is its name; those the default program does not have are the artifact's own.
        let policy = composed
            .ids
            .iter()
            .filter(|(_, id)| fixture.plan.node(id).is_none())
            .map(|(local, id)| (local.to_string(), id.to_string()))
            .collect::<Vec<_>>();
        let mut expected_policy = expected
            .policy
            .iter()
            .map(|(local, id)| (local.to_string(), id.to_string()))
            .collect::<Vec<_>>();
        expected_policy.sort();
        assert_eq!(policy, expected_policy, "{}", what);
        for (local, id) in &composed.ids {
            assert_eq!(*id, NodeId::new(local.to_local_id()), "{}", what);
        }
        assert_eq!(
            composed.plan.provenance.as_ref().unwrap().artifact,
            Some(ArtifactRevision {
                revision: 3,
                hash: received.hash,
            }),
            "{}",
            what
        );

        let diff = diff(&fixture.core, &composed.plan, &registry);
        // In node ID order: the diff lists nodes in the composed plan's canonical order.
        let mut listed = diff
            .changes()
            .map(|node| (node.id.to_string(), describe(&node.change)))
            .collect::<Vec<_>>();
        let mut expected_diff = expected
            .diff
            .iter()
            .map(|(id, change)| (id.to_string(), change.to_string()))
            .collect::<Vec<_>>();
        listed.sort();
        expected_diff.sort();
        assert_eq!(listed, expected_diff, "{}", what);
    }
}

#[test]
fn modified_and_added_nodes_have_their_bindings_resolved_from_the_catalog() {
    let fixture = Fixture::default_program();
    let mut document = fixture.artifact();
    // A different kind filter, and a binding the default program does not use in this node.
    node(&mut document, "af_metrics_gateway")["params"]["predicate"] = json!({"all": [
        {"kind_in": ["counter"]},
        {"flag": {"config": "autoscaling_failover.enabled"}},
        {"flag": {"config": "multi_region_failover.enabled"}}]});
    let composed = fixture.compose(document, &approval(2)).unwrap();
    assert!(composed.report.is_valid(), "{}", composed.report);

    // The core plan writes each binding as its value, which normal form then folds: both flags are on.
    let gateway = "af_metrics_gateway".parse().unwrap();
    let node = composed.plan.node(&gateway).unwrap();
    assert_eq!(node.params.get("predicate").unwrap(), &json!({"kind_in": ["counter"]}));
    let bindings = composed
        .bindings
        .of(&gateway)
        .iter()
        .map(|found| (found.path.0.as_str(), found.binding.as_str(), found.value.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        bindings,
        [
            ("/predicate/all/1/flag", "autoscaling_failover.enabled", json!(true)),
            ("/predicate/all/2/flag", "multi_region_failover.enabled", json!(true)),
        ]
    );
}

#[test]
fn edits_that_cannot_be_composed_are_refused_with_the_reason() {
    let fixture = Fixture::default_program();
    let id = |id: &str| id.parse::<ArtifactNodeId>().unwrap();
    let output = |id: &str| id.parse::<OutputId>().unwrap();
    let source = |name: &str| name.parse::<InputName>().unwrap();

    // (what, mutation, expected error)
    let cases: Vec<(&str, Mutation, ComposeError)> = vec![
        (
            "an output the host does not declare",
            |document| node(document, "metrics_fanout_0")["params"]["output"] = json!("metrics_routing_encode_99"),
            ComposeError::UndeclaredOutput {
                node: id("metrics_fanout_0"),
                output: output("metrics_routing_encode_99"),
                declared: [
                    "dsd_client_telemetry",
                    "dd_metrics_encode",
                    "metrics_routing_encode_0",
                    "metrics_routing_encode_1",
                    "mrf_metrics_encode",
                    "af_metrics_encode",
                ]
                .map(output)
                .to_vec(),
            },
        ),
        (
            "a source's ID",
            |document| rename(document, "metrics_fanout_0", "dsd_in"),
            ComposeError::ReservedSource {
                node: id("dsd_in"),
                reserved: "dsd_in".parse().unwrap(),
            },
        ),
        (
            "an edge from a source the host does not have",
            |document| edge_to(document, "metrics_in.1")["from"] = json!("input.traces"),
            ComposeError::UnknownSource {
                name: source("traces"),
                known: ["checks", "dogstatsd", "liveness", "otlp", "trace_sampler"]
                    .map(source)
                    .to_vec(),
            },
        ),
        (
            "an edge into a source",
            |document| push_edge(document, "metrics_fanout_0.in", "input.dogstatsd"),
            ComposeError::Malformed {
                message: "edge 34 enters `input.dogstatsd`, which is a source; edges leave the host's sources"
                    .to_string(),
            },
        ),
        (
            "an edge to an unknown node",
            |document| edge_to(document, "metrics_fanout_0.in")["to"] = json!("ghost.in"),
            ComposeError::Malformed {
                message: "edge 21 enters `ghost.in`, but the artifact has no node `ghost`".to_string(),
            },
        ),
        (
            "two nodes with one ID",
            |document| push_node(document, json!({"id": "metrics_fanout_0", "op": "drop"})),
            ComposeError::Malformed {
                message: "more than one node has the ID `metrics_fanout_0`; give each node its own ID".to_string(),
            },
        ),
    ];
    for (what, mutate, expected) in cases {
        let mut document = fixture.artifact();
        mutate(&mut document);
        match fixture.compose(document, &approval(1)) {
            Err(Refused::Compose(error)) => assert_eq!(error, expected, "{}", what),
            Err(Refused::Receipt(error)) => panic!("{}: refused on receipt: {}", what, error),
            Ok(composed) => panic!("{}: expected {:?}, got:\n{}", what, expected, composed.report),
        }
    }
}

#[test]
fn an_input_no_source_binds_is_refused_naming_the_sources() {
    let fixture = Fixture::default_program();
    let mut document = fixture.artifact();
    edge_to(&mut document, "metrics_in.1")["from"] = json!("input.traces");
    let Err(Refused::Compose(error)) = fixture.compose(document, &approval(1)) else {
        panic!("an input no source binds composed");
    };
    assert_eq!(
        error.to_string(),
        "The artifact reads `input.traces`, and the host has no source `traces`. Read one of its sources: \
         `input.checks`, `input.dogstatsd`, `input.liveness`, `input.otlp`, `input.trace_sampler`."
    );

    // A host without sources says so.
    let error = ComposeError::UnknownSource {
        name: "traces".parse().unwrap(),
        known: Vec::new(),
    };
    assert_eq!(
        error.to_string(),
        "The artifact reads `input.traces`, and the host has no source `traces`. The host has no sources."
    );
}

#[test]
fn a_state_owner_composes_with_the_artifacts_provenance_whatever_its_parameters() {
    // A state owner is no composition error: whether the frame can run it where the artifact puts it is placement's to
    // decide, and installing a change to one is a cut. So a second aggregation composes, with or without parameters
    // its operator accepts, for validation and placement to judge.
    let fixture = Fixture::default_program();
    let params = unresolved(&fixture.plan.node(&"dsd_agg".parse().unwrap()).unwrap().params);
    for params in [json!(params), json!({})] {
        let mut document = fixture.artifact();
        push_node(&mut document, json!({"id": "agg", "op": "aggregate", "params": params}));
        edge_to(&mut document, "metrics_fanout.in")["from"] = json!("agg.out");
        push_edge(&mut document, "metrics.out", "agg.in");
        let composed = fixture.compose(document, &approval(1)).unwrap();
        let id = &composed.ids[&"agg".parse::<ArtifactNodeId>().unwrap()];
        assert_eq!(id.to_string(), "agg");
        assert!(composed.plan.node(id).is_some());
    }
}

#[test]
fn errors_about_outputs_and_sources_say_what_the_author_can_do() {
    let id = |id: &str| id.parse::<ArtifactNodeId>().unwrap();
    let output = |id: &str| id.parse::<OutputId>().unwrap();
    // (error, its message)
    let cases = [
        (
            ComposeError::UndeclaredOutput {
                node: id("route"),
                output: output("nowhere"),
                declared: vec![output("main"), output("routed")],
            },
            "Artifact node `route` emits to output `nowhere`, which the host does not declare. Emit to one of the \
             outputs it declares: `main`, `routed`.",
        ),
        (
            ComposeError::UndeclaredOutput {
                node: id("route"),
                output: output("nowhere"),
                declared: Vec::new(),
            },
            "Artifact node `route` emits to output `nowhere`, which the host does not declare. The host declares no \
             outputs.",
        ),
        (
            ComposeError::ReservedSource {
                node: id("dsd_in"),
                reserved: "dsd_in".parse().unwrap(),
            },
            "Artifact node `dsd_in` has the ID of source `dsd_in`, which the host places. Rename the node.",
        ),
    ];
    for (error, message) in cases {
        assert_eq!(error.to_string(), message);
    }
}

#[test]
fn a_binding_the_host_declares_without_a_value_cannot_be_composed() {
    let mut fixture = Fixture::default_program();
    let name = "multi_region_failover.metric_mirroring.enabled"
        .parse::<BindingName>()
        .unwrap();
    let declared = fixture.catalog.binding(&name).unwrap().clone();
    fixture.catalog = fixture.catalog.clone().with_catalog_binding(
        name.clone(),
        CatalogBinding {
            value: None,
            ..declared
        },
    );

    // Every node of the program resolves its bindings from the catalog, kept as the default program has it or not, and
    // the catalog has no value for one.
    let mut modified = fixture.artifact();
    node(&mut modified, "mrf_metrics_gateway")["params"]["predicate"]["all"][0] = json!({"flag": true});
    for document in [fixture.artifact(), modified] {
        match fixture.compose(document, &approval(1)) {
            Err(Refused::Compose(error)) => assert_eq!(
                error,
                ComposeError::UnresolvedBinding {
                    node: "mrf_metrics_gateway".parse().unwrap(),
                    binding: name.clone(),
                }
            ),
            Err(Refused::Receipt(error)) => panic!("refused on receipt: {}", error),
            Ok(composed) => panic!("expected an unresolved binding, got:\n{}", composed.report),
        }
    }
}

#[test]
fn composed_plans_that_break_a_validation_rule_are_composed_with_the_finding() {
    let fixture = Fixture::default_program();
    let wide_tee: Mutation = |document| {
        node(document, "metrics_fanout")["params"]["ports"] = json!(40);
        for port in 5..40 {
            push_node(document, json!({"id": format!("extra_{}", port), "op": "drop"}));
            push_edge(
                document,
                &format!("metrics_fanout.{}", port),
                &format!("extra_{}.in", port),
            );
        }
    };
    // (what, mutation, the rule of an error the report has)
    let cases: Vec<(&str, Mutation, Rule)> = vec![
        (
            "an unknown binding",
            |document| {
                node(document, "mrf_metrics_gateway")["params"]["predicate"]["all"][0] =
                    json!({"flag": {"config": "not.declared"}});
            },
            Rule::KnownBinding,
        ),
        (
            "a host-owned operator",
            |document| {
                let all = json!({"kinds": "all", "timestamps": "mixed"});
                let gateway = node(document, "metrics_routing_filter_0");
                gateway["op"] = json!("opaque_host");
                gateway["params"] =
                    json!({"component": "metrics_routing_filter_0", "input": all, "outputs": {"out": all}});
            },
            Rule::TrustedHost,
        ),
        (
            "a duplicate contribution",
            |document| node(document, "metrics_routing_filter_0_true")["params"]["output"] = json!("dd_metrics_encode"),
            Rule::DuplicateContribution,
        ),
        ("a tee wider than the artifact limit", wide_tee, Rule::TeeFanOut),
    ];
    for (what, mutate, rule) in cases {
        let mut document = fixture.artifact();
        mutate(&mut document);
        let composed = fixture
            .compose(document, &approval(1))
            .unwrap_or_else(|e| panic!("{}: {:?}", what, e));
        assert!(
            composed.report.errors().any(|finding| finding.rule == rule),
            "{}: expected a {} error:\n{}",
            what,
            rule,
            composed.report
        );
    }
}

#[test]
fn an_artifact_whose_nodes_declare_more_memory_than_the_limit_is_invalid() {
    let fixture = Fixture::default_program();
    // The program's DogStatsD mapper, kept from the default program, declares this much of its memory.
    const KEPT: u64 = 6_547_112;
    // (what, the added mapper's interner bytes, the memory the error says the artifact declares, if it is invalid)
    let cases: [(&str, u64, Option<u64>); 4] = [
        ("the native default interner", 64 * 1024, None),
        (
            "an interner that brings the program to the limit",
            64 * 1024 * 1024 - KEPT,
            None,
        ),
        (
            "an interner of a pebibyte",
            1_125_899_906_842_624,
            Some(1_125_899_906_842_624 + KEPT),
        ),
        (
            "an interner that brings the program just over the limit",
            64 * 1024 * 1024 - KEPT + 1,
            Some(67_108_865),
        ),
    ];
    for (what, interner, expected) in cases {
        let mut document = fixture.artifact();
        edge_to(&mut document, "metrics_fanout.in")["to"] = json!("mapper.in");
        push_edge(&mut document, "mapper.out", "metrics_fanout.in");
        push_node(
            &mut document,
            json!({"id": "mapper", "op": "dogstatsd_mapper", "params": {
                "profiles": [], "cache_size": 0, "string_interner_bytes": interner
            }}),
        );
        let received = receive_artifact(document.clone()).unwrap();
        let composed = fixture
            .compose(document, &approval(1))
            .unwrap_or_else(|e| panic!("{}: {:?}", what, e));
        let findings = composed
            .report
            .errors()
            .filter(|finding| finding.rule == Rule::ArtifactMemory)
            .map(|finding| (finding.location.clone(), finding.message.clone()))
            .collect::<Vec<_>>();
        let expected = expected
            .map(|bytes| {
                (
                    Location::Node {
                        id: "mapper".parse().unwrap(),
                        port: None,
                    },
                    format!(
                        "the nodes of artifact revision 1 ({}) declare {} bytes of memory, more than the limit of \
                         67108864, and this node declares {} of them; reduce its cache and interner sizes",
                        received.hash, bytes, interner
                    ),
                )
            })
            .into_iter()
            .collect::<Vec<_>>();
        assert_eq!(findings, expected, "{}", what);
    }
}

#[test]
fn stale_reviews_are_flagged_when_approval_was_against_another_active_plan() {
    let fixture = Fixture::default_program();
    // (approved against, active plan, stale review)
    let cases = [
        (None, None, false),
        (None, Some(hash('1')), false),
        (Some(hash('1')), Some(hash('1')), false),
        (Some(hash('1')), Some(hash('2')), true),
        (Some(hash('1')), None, true),
    ];
    for (approved_against, active_plan_hash, stale) in cases {
        let approval = Approval {
            revision: 1,
            approved_against,
            active_plan_hash,
            in_effect: false,
        };
        let composed = fixture.compose(fixture.artifact(), &approval).unwrap();
        assert_eq!(composed.stale_review, stale, "{:?}", approval);
    }
}

#[test]
fn artifacts_beyond_the_limits_or_carrying_resolved_values_are_refused_on_receipt() {
    let fixture = Fixture::default_program();
    let exported = fixture.export.artifact.nodes.len();
    let exported_leaves = artifact_predicate_leaves(&fixture.export.artifact, &OperatorRegistry::native()) as usize;
    let consts = |count: usize| json!({"any": vec![json!({"const": true}); count]});
    // (what, document, check)
    type Check = fn(&ReceiptError) -> bool;
    let mut cases: Vec<(&str, Value, Check)> = Vec::new();

    let mut oversized = fixture.artifact();
    let names = (0..20_000)
        .map(|i| format!("demo.debug.metric_{:06}", i))
        .collect::<Vec<_>>();
    push_node(
        &mut oversized,
        json!({"id": "big", "op": "split", "params": {"predicate": {"name": {"in": names}}}}),
    );
    cases.push((
        "an oversized artifact",
        oversized,
        |error| matches!(error, ReceiptError::TooLarge { size, limit: 262_144 } if *size > 262_144),
    ));

    let mut many = fixture.artifact();
    for i in 0..257 - exported {
        push_node(&mut many, json!({"id": format!("extra_{}", i), "op": "drop"}));
    }
    cases.push(("too many nodes", many, |error| {
        matches!(error, ReceiptError::TooManyNodes { nodes: 257, limit: 256 })
    }));

    let mut leaves = fixture.artifact();
    push_node(
        &mut leaves,
        json!({"id": "wide", "op": "split", "params": {"predicate": consts(1025 - exported_leaves)}}),
    );
    cases.push(("too many predicate leaves", leaves, |error| {
        matches!(
            error,
            ReceiptError::TooManyPredicateLeaves {
                leaves: 1025,
                limit: 1024
            }
        )
    }));

    // Leaves count with blob references replaced by their content.
    let mut blob_leaves = fixture.artifact();
    let predicate = consts(1025 - exported_leaves);
    let predicate_hash = PlanHash::of_value(&predicate).unwrap();
    push_node(
        &mut blob_leaves,
        json!({"id": "wide", "op": "split", "params": {"predicate": {"blob": predicate_hash.to_string()}}}),
    );
    blob_leaves["blobs"] = json!({predicate_hash.to_string(): predicate});
    cases.push(("too many predicate leaves in a blob", blob_leaves, |error| {
        matches!(error, ReceiptError::TooManyPredicateLeaves { leaves: 1025, .. })
    }));

    let mut big_blob = fixture.artifact();
    let list = (0..4_000)
        .map(|i| format!("demo.debug.metric_{:06}", i))
        .collect::<Vec<_>>();
    let list_hash = PlanHash::of_value(&json!(list)).unwrap();
    push_node(
        &mut big_blob,
        json!({"id": "big", "op": "split", "params": {"predicate": {"name": {"in": {"blob": list_hash.to_string()}}}}}),
    );
    big_blob["blobs"] = json!({list_hash.to_string(): list});
    cases.push((
        "an oversized blob",
        big_blob,
        |error| matches!(error, ReceiptError::BlobTooLarge { size, limit: 65_536, .. } if *size > 65_536),
    ));

    let mut resolved = fixture.artifact();
    node(&mut resolved, "mrf_metrics_gateway")["params"]["predicate"]["all"][0]["flag"]["resolved"] = json!(true);
    cases.push(("resolved values", resolved, |error| {
        matches!(
            error,
            ReceiptError::Document {
                source: IrError::ResolvedInArtifact { .. }
            }
        )
    }));

    // A blob is part of the artifact: it carries no resolved value either.
    let mut resolved_blob = fixture.artifact();
    let smuggled = json!({"config": "autoscaling_failover.enabled", "resolved": false});
    let smuggled_hash = PlanHash::of_value(&smuggled).unwrap();
    node(&mut resolved_blob, "af_metrics_gateway")["params"]["predicate"]["all"][1]["flag"] =
        json!({"blob": smuggled_hash.to_string()});
    resolved_blob["blobs"] = json!({smuggled_hash.to_string(): smuggled});
    cases.push(("resolved values inside a blob", resolved_blob, |error| {
        matches!(error, ReceiptError::ResolvedInBlob { .. })
    }));

    let mut version = fixture.artifact();
    version["ir_version"] = json!(2);
    cases.push(("an unsupported IR version", version, |error| {
        matches!(
            error,
            ReceiptError::Document {
                source: IrError::UnsupportedVersion { .. }
            }
        )
    }));

    for (what, document, check) in cases {
        match receive_artifact(document) {
            Err(error) => assert!(check(&error), "{}: {}", what, error),
            Ok(_) => panic!("{}: the artifact should be refused", what),
        }
    }

    // A blob counts once for every reference to it, since composition copies it into each place: the artifact's size
    // is the size it has written without blobs.
    let names = (0..2_200)
        .map(|i| format!("demo.debug.metric_{:06}", i))
        .collect::<Vec<_>>();
    let names_hash = PlanHash::of_value(&json!(names)).unwrap();
    let referring = |references: usize, to: Value| {
        let mut document = fixture.artifact();
        let operands = vec![json!({"name": {"in": to}}); references];
        push_node(
            &mut document,
            json!({"id": "big", "op": "split", "params": {"predicate": {"any": operands}}}),
        );
        document
    };
    let written_out = |references: usize| {
        let document = referring(references, json!(names));
        to_canonical_string(&document).unwrap().len() as u64
    };
    let with_blob = |references: usize| {
        let mut document = referring(references, json!({"blob": names_hash.to_string()}));
        document["blobs"] = json!({names_hash.to_string(): names});
        document
    };
    let document = with_blob(5);
    assert!(to_canonical_string(&document).unwrap().len() < 80_000);
    assert!(written_out(5) > 262_144);
    match receive_artifact(document) {
        Err(ReceiptError::TooLargeInlined { size, limit }) => {
            assert_eq!((size, limit), (written_out(5), 262_144))
        }
        other => panic!(
            "a blob referenced five times should be refused as too large, not {:?}",
            other
        ),
    }
    // Referenced twice, it fits.
    assert!(written_out(2) < 262_144);
    receive_artifact(with_blob(2)).unwrap();

    // At the limits, an artifact is received.
    let mut at_limits = fixture.artifact();
    for i in 0..255 - exported - 1 {
        push_node(&mut at_limits, json!({"id": format!("extra_{}", i), "op": "drop"}));
    }
    push_node(
        &mut at_limits,
        json!({"id": "wide", "op": "split", "params": {"predicate": consts(1024 - exported_leaves)}}),
    );
    let received = receive_artifact(at_limits).unwrap();
    assert_eq!(received.artifact.nodes.len(), 255);
}

/// A policy tree, which composes into a valid plan by construction.
#[derive(Clone, Debug)]
enum Tree {
    /// Delivers to one of [`OUTPUTS`].
    Emit(usize),

    /// Drops.
    Drop,

    /// Splits by a predicate; both sides may deliver to the same outputs, since a split sends each point one way.
    Split(Value, Box<Tree>, Box<Tree>),

    /// Copies to every branch; branches deliver to disjoint outputs, so no contribution is delivered twice.
    Tee(Vec<Tree>),

    /// Keeps one of [`GATEWAYS`] of the default program, unmodified.
    Kept(usize),
}

/// The outputs a generated tree delivers to: every output after the default program's enrichment that accepts every
/// kind.
const OUTPUTS: [&str; 4] = [
    "dd_metrics_encode",
    "metrics_routing_encode_0",
    "metrics_routing_encode_1",
    "mrf_metrics_encode",
];

/// The gateways of the default program a generated tree may keep, with the index of the output each delivers to.
const GATEWAYS: [(&str, usize); 3] = [
    ("metrics_routing_filter_0", 1),
    ("metrics_routing_filter_1", 2),
    ("mrf_metrics_gateway", 3),
];

/// The node of the default program's export whose output a generated tree reads, and the nodes after it, which the
/// tree takes the place of.
const ENRICHED: &str = "metrics";

/// Returns the default program's nodes after its enrichment ([`ENRICHED`]), which a generated tree takes the place of.
fn after_enrichment(export: &PolicyArtifact) -> Vec<String> {
    let index = export
        .nodes
        .iter()
        .position(|node| node.id.as_str() == ENRICHED)
        .unwrap();
    export.nodes[index + 1..]
        .iter()
        .map(|node| node.id.to_string())
        .collect()
}

/// Returns a strategy for trees that deliver only to the outputs in `outputs`.
fn arb_tree(outputs: Vec<usize>, depth: u32) -> BoxedStrategy<Tree> {
    let mut leaves = vec![Just(Tree::Drop).boxed()];
    if !outputs.is_empty() {
        leaves.push(select(outputs.clone()).prop_map(Tree::Emit).boxed());
        let kept = GATEWAYS
            .iter()
            .enumerate()
            .filter(|(_, (_, output))| outputs.contains(output))
            .map(|(gateway, _)| gateway)
            .collect::<Vec<_>>();
        if !kept.is_empty() {
            leaves.push(select(kept).prop_map(Tree::Kept).boxed());
        }
    }
    let leaf = proptest::strategy::Union::new(leaves).boxed();
    if depth == 0 {
        return leaf;
    }
    let split = (
        arb_predicate(),
        arb_tree(outputs.clone(), depth - 1),
        arb_tree(outputs.clone(), depth - 1),
    )
        .prop_map(|(predicate, yes, no)| Tree::Split(predicate_json(&predicate), Box::new(yes), Box::new(no)));
    // A tee gives each output to one branch at most.
    let tee = (2usize..=3, vec(0usize..3, outputs.len())).prop_flat_map(move |(branches, owners)| {
        let shares = (0..branches)
            .map(|branch| {
                let share = outputs
                    .iter()
                    .zip(&owners)
                    .filter(|(_, owner)| **owner % branches == branch)
                    .map(|(output, _)| *output)
                    .collect::<Vec<_>>();
                arb_tree(share, depth - 1)
            })
            .collect::<Vec<_>>();
        shares.prop_map(Tree::Tee)
    });
    prop_oneof![2 => leaf, 2 => split, 1 => tee].boxed()
}

/// The nodes and edges written for a tree, and the gateways it keeps.
#[derive(Default)]
struct Written {
    nodes: Vec<Value>,
    edges: Vec<Value>,
    kept: Vec<usize>,
}

/// Writes a tree as an artifact's nodes and edges, fed from `from`.
///
/// Each gateway is kept at most once, since its nodes have fixed IDs; a second occurrence is written as a drop.
fn write_tree(tree: &Tree, from: String, export: &PolicyArtifact, out: &mut Written) {
    let id = format!("n{}", out.nodes.len());
    let edge_in = |out: &mut Written, to: &str| out.edges.push(json!({"from": from, "to": format!("{}.in", to)}));
    match tree {
        Tree::Kept(gateway) if !out.kept.contains(gateway) => {
            out.kept.push(*gateway);
            let name = GATEWAYS[*gateway].0;
            for suffix in ["", "_true", "_false"] {
                let id = format!("{}{}", name, suffix);
                let node = export.nodes.iter().find(|node| node.id.as_str() == id).unwrap();
                out.nodes.push(serde_json::to_value(node).unwrap());
            }
            edge_in(out, name);
            for port in ["true", "false"] {
                out.edges
                    .push(json!({"from": format!("{}.{}", name, port), "to": format!("{}_{}.in", name, port)}));
            }
        }
        Tree::Kept(_) | Tree::Drop => {
            out.nodes.push(json!({"id": id, "op": "drop"}));
            edge_in(out, &id);
        }
        Tree::Emit(output) => {
            out.nodes
                .push(json!({"id": id, "op": "emit", "params": {"output": OUTPUTS[*output]}}));
            edge_in(out, &id);
        }
        Tree::Split(predicate, yes, no) => {
            out.nodes
                .push(json!({"id": id, "op": "split", "params": {"predicate": predicate}}));
            edge_in(out, &id);
            write_tree(yes, format!("{}.true", id), export, out);
            write_tree(no, format!("{}.false", id), export, out);
        }
        Tree::Tee(branches) => {
            out.nodes
                .push(json!({"id": id, "op": "tee", "params": {"ports": branches.len()}}));
            edge_in(out, &id);
            for (port, branch) in branches.iter().enumerate() {
                write_tree(branch, format!("{}.{}", id, port), export, out);
            }
        }
    }
}

/// Returns the artifact document of the default program's export with a tree in place of what follows its enrichment,
/// and the gateways the tree keeps.
fn tree_artifact(tree: &Tree, export: &PolicyArtifact) -> (Value, Vec<usize>) {
    let replaced = after_enrichment(export);
    let mut document = serde_json::to_value(export).unwrap();
    remove_nodes(&mut document, &replaced.iter().map(String::as_str).collect::<Vec<_>>());
    let mut out = Written::default();
    write_tree(tree, format!("{}.out", ENRICHED), export, &mut out);
    document["nodes"].as_array_mut().unwrap().extend(out.nodes);
    document["edges"].as_array_mut().unwrap().extend(out.edges);
    (document, out.kept)
}

/// One change to an artifact document, which may make it invalid or impossible to compose.
#[derive(Clone, Debug)]
enum Edit {
    RemoveNode(usize),
    RemoveEdge(usize),
    Rename(usize, &'static str),
    Retarget(usize, usize),
    Operator(usize, &'static str),
    Output(usize, &'static str),
}

fn arb_edit() -> impl Strategy<Value = Edit> {
    let names = select(vec![
        "metrics_fanout",
        "metrics_fanout_0",
        "af_metrics_gateway",
        "dsd_agg",
        "dsd_in",
        "fresh",
        "input",
    ]);
    let operators = select(vec![
        "drop",
        "emit",
        "split",
        "tee",
        "merge",
        "aggregate",
        "opaque_host",
        "unknown",
    ]);
    let outputs = select(vec![
        "dd_metrics_encode",
        "dsd_client_telemetry",
        "nowhere",
        "af_metrics_encode",
    ]);
    prop_oneof![
        any::<usize>().prop_map(Edit::RemoveNode),
        any::<usize>().prop_map(Edit::RemoveEdge),
        (any::<usize>(), names).prop_map(|(node, name)| Edit::Rename(node, name)),
        (any::<usize>(), any::<usize>()).prop_map(|(edge, node)| Edit::Retarget(edge, node)),
        (any::<usize>(), operators).prop_map(|(node, op)| Edit::Operator(node, op)),
        (any::<usize>(), outputs).prop_map(|(node, output)| Edit::Output(node, output)),
    ]
}

/// Applies an edit, and returns `true` if it removed a node or an edge.
///
/// The export is a graph whose every port has one edge, and no edit adds an edge from a port, so a removal leaves a
/// port without its edge that no later edit restores: the composed plan cannot be valid.
fn apply(document: &mut Value, edit: &Edit) -> bool {
    let nodes = document["nodes"].as_array().unwrap().len().max(1);
    let edges = document["edges"].as_array().unwrap().len().max(1);
    let id = |document: &Value, index: usize| document["nodes"][index % nodes]["id"].as_str().map(str::to_string);
    match edit {
        Edit::RemoveNode(index) => {
            if let Some(id) = id(document, *index) {
                remove_nodes(document, &[&id]);
                return true;
            }
        }
        Edit::RemoveEdge(index) => {
            let edges = document["edges"].as_array_mut().unwrap();
            if !edges.is_empty() {
                edges.remove(index % edges.len());
                return true;
            }
        }
        Edit::Rename(index, name) => {
            if let Some(id) = id(document, *index) {
                rename(document, &id, name);
            }
        }
        Edit::Retarget(edge, index) => {
            if let Some(id) = id(document, *index) {
                if let Some(edge) = document["edges"].get_mut(edge % edges) {
                    edge["to"] = json!(format!("{}.in", id));
                }
            }
        }
        Edit::Operator(index, op) => {
            if let Some(node) = document["nodes"].get_mut(index % nodes) {
                node["op"] = json!(op);
            }
        }
        Edit::Output(index, output) => {
            if let Some(node) = document["nodes"].get_mut(index % nodes) {
                node["params"] = json!({"output": output});
            }
        }
    }
    false
}

/// Returns why the artifact explains a composition error, or `None` if nothing in it does.
///
/// Each error is checked against the artifact itself: the node it names exists and does what the error says, and the
/// default program and catalog say what the error says of them.
fn explains(error: &ComposeError, artifact: &PolicyArtifact, fixture: &Fixture) -> Option<&'static str> {
    let node = |id: &ArtifactNodeId| artifact.nodes.iter().find(|node| node.id == *id);
    let known = sources(&fixture.plan);
    match error {
        ComposeError::ReservedSource { node: id, reserved } => (node(id).is_some()
            && *reserved == NodeId::new(id.to_local_id())
            && fixture.is_source(reserved))
        .then_some("a node has the ID of a source"),
        ComposeError::UnknownSource { name, known: listed } => {
            let reads = artifact
                .edges
                .iter()
                .any(|edge| edge.from == ArtifactPortRef::Input(name.clone()));
            (reads && !known.contains_key(name) && listed.iter().eq(known.keys()))
                .then_some("an edge reads a source the host does not have")
        }
        ComposeError::UndeclaredOutput {
            node: id,
            output,
            declared,
        } => {
            let all_declared = fixture
                .plan
                .outputs
                .iter()
                .map(|declared| declared.id.clone())
                .filter(|id| fixture.catalog.output(id).is_some())
                .collect::<Vec<_>>();
            node(id)
                .filter(|node| node.op.as_str() == names::EMIT && emitted_output(&node.params).as_ref() == Some(output))
                .filter(|_| fixture.catalog.output(output).is_none() && *declared == all_declared)
                .map(|_| "an emit delivers to an output the host does not declare")
        }
        ComposeError::Malformed { .. } => {
            let ids = artifact.nodes.iter().map(|node| &node.id).collect::<BTreeSet<_>>();
            let unknown_end = artifact.edges.iter().any(|edge| {
                [&edge.from, &edge.to].into_iter().any(|end| match end {
                    ArtifactPortRef::Node { node, .. } => !ids.contains(node),
                    ArtifactPortRef::Input(_) => false,
                }) || matches!(edge.to, ArtifactPortRef::Input(_))
            });
            (ids.len() < artifact.nodes.len() || unknown_end).then_some("two nodes share an ID, or an edge has no end")
        }
        ComposeError::DefaultProgram { .. }
        | ComposeError::UnresolvedBinding { .. }
        // The edits never address emits to recipients.
        | ComposeError::UnknownRecipient { .. }
        | ComposeError::PendingRecipients { .. }
        | ComposeError::RecipientsOnTwoHosts { .. }
        | ComposeError::NoOutputFor { .. } => None,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn property_test_valid_random_artifacts_compose_into_valid_plans(tree in arb_tree((0..4).collect(), 3)) {
        let fixture = Fixture::default_program();
        let registry = OperatorRegistry::native();
        let (document, kept) = tree_artifact(&tree, &fixture.export.artifact);
        let received = receive_artifact(document).unwrap();
        let composed = compose(&fixture.plan, &received, &approval(9), &fixture.catalog, &registry).unwrap();
        prop_assert!(composed.report.is_valid(), "{:?}\n{}", tree, composed.report);

        // Kept nodes are the default program's, and so is a kept drop behind a `split` of the artifact's, which drops
        // another stream than native configuration does: a node is what it does, whatever feeds it. Every other node of
        // the tree is the artifact's own.
        let replaced = after_enrichment(&fixture.export.artifact);
        let kept_nodes = kept
            .iter()
            .flat_map(|gateway| ["", "_true", "_false"].map(|suffix| format!("{}{}", GATEWAYS[*gateway].0, suffix)))
            .collect::<Vec<_>>();
        let before = fixture
            .export
            .artifact
            .nodes
            .iter()
            .map(|node| node.id.to_string())
            .filter(|id| !replaced.contains(id))
            .collect::<Vec<_>>();
        for node in &received.artifact.nodes {
            let id = &composed.ids[&node.id];
            if kept_nodes.contains(&node.id.to_string()) || before.contains(&node.id.to_string()) {
                prop_assert_eq!(id, &NodeId::new(node.id.to_local_id()));
                prop_assert_eq!(composed.plan.node(id), fixture.core.node(id));
            } else {
                prop_assert_eq!(id.to_string(), format!("{}", node.id));
            }
        }

        // Nothing before the tree changes, but the edge leaving the port that feeds it.
        let diff = diff(&fixture.core, &composed.plan, &registry);
        for change in diff.changes() {
            let local = change.id.local().to_string();
            if fixture.core.node(&change.id).is_some() && !replaced.contains(&local) {
                prop_assert_eq!(local, ENRICHED);
                prop_assert_eq!(
                    &change.change,
                    &NodeChange::Changed { operator: false, params: false, edges: true }
                );
            }
        }
    }

    #[test]
    fn property_test_edited_artifacts_compose_or_are_refused_cleanly(edits in vec(arb_edit(), 1..5)) {
        let fixture = Fixture::default_program();
        let registry = OperatorRegistry::native();
        let mut document = fixture.artifact();
        let mut removed = false;
        for edit in &edits {
            removed |= apply(&mut document, edit);
        }
        let Ok(received) = receive_artifact(document) else {
            return Ok(());
        };
        let artifact = &received.artifact;
        // A node with the ID of a source is always refused.
        let reserved_source = artifact
            .nodes
            .iter()
            .any(|node| fixture.is_source(&NodeId::new(node.id.to_local_id())));
        match compose(&fixture.plan, &received, &approval(4), &fixture.catalog, &registry) {
            Err(error) => {
                prop_assert!(explains(&error, artifact, &fixture).is_some(), "{:?}: {}", edits, error);
                if reserved_source {
                    prop_assert!(
                        matches!(error, ComposeError::ReservedSource { .. } | ComposeError::Malformed { .. }),
                        "{:?}: {}",
                        edits,
                        error
                    );
                }
            }
            Ok(composed) => {
                prop_assert!(!reserved_source, "{:?}", edits);
                // Every node but the sources is the program's, whatever it does.
                let program = crate::export::program_nodes(&composed.plan)
                    .into_iter()
                    .flat_map(|(_, nodes)| nodes)
                    .collect::<BTreeSet<_>>();
                for node in &composed.plan.nodes {
                    prop_assert_eq!(program.contains(&node.id), !fixture.is_source(&node.id), "{:?}", edits);
                }
                // A node or an edge removed leaves a port without its edge, which no other edit restores.
                if removed {
                    prop_assert!(!composed.report.is_valid(), "{:?}", edits);
                }
                // Every artifact node became one plan node, as its plan ID says.
                prop_assert_eq!(composed.ids.len(), received.artifact.nodes.len());
                for id in composed.ids.values() {
                    prop_assert!(composed.plan.node(id).is_some());
                }
                // Every source is kept as it is.
                for node in fixture.plan.nodes.iter().filter(|node| fixture.is_source(&node.id)) {
                    prop_assert_eq!(composed.plan.node(&node.id), Some(node));
                }
                // Artifacts reference native outputs only: every emit names an output the host declares.
                for node in artifact.nodes.iter().filter(|node| node.op.as_str() == names::EMIT) {
                    let Some(output) = emitted_output(&node.params) else { continue };
                    prop_assert!(
                        fixture.catalog.output(&output).is_some(),
                        "{:?}: `{}` emits to `{}`",
                        edits,
                        node.id,
                        output
                    );
                }
            }
        }
    }
}

/// Returns an artifact as received, with the hash of its document, without reading it from the document.
fn received_directly(artifact: PolicyArtifact) -> ReceivedArtifact {
    let hash = PlanHash::of_value(&serde_json::to_value(&artifact).unwrap()).unwrap();
    ReceivedArtifact { artifact, hash }
}

/// Returns the detached export of a default program.
fn detached(plan: &Plan, catalog: &MemoryCatalog) -> ExportBundle {
    export_as(
        plan,
        catalog,
        &BTreeMap::new(),
        ExportForm::Detached,
        &OperatorRegistry::native(),
    )
    .unwrap()
}

/// Returns every binding a parameter value binds.
fn bindings_in(value: &Value, found: &mut BTreeSet<String>) {
    match value {
        Value::Object(members) => {
            if let Some(Value::String(name)) = members.get("config") {
                found.insert(name.clone());
            }
            members.values().for_each(|member| bindings_in(member, found));
        }
        Value::Array(values) => values.iter().for_each(|value| bindings_in(value, found)),
        _ => {}
    }
}

#[test]
fn the_detached_export_keeps_facts_and_signals_bound() {
    for (name, plan, catalog) in default_programs() {
        let detached = detached(&plan, &catalog);
        assert_eq!(detached.form, ExportForm::Detached, "{}", name);

        let mut found = BTreeSet::new();
        for node in &detached.artifact.nodes {
            node.params.iter().for_each(|(_, value)| bindings_in(value, &mut found));
        }
        for kept in [
            "host.tags",
            "host.tags_deadline",
            "multi_region_failover.metric_mirroring.enabled",
        ] {
            assert!(found.contains(kept), "{}: `{}` is not bound: {:?}", name, kept, found);
        }

        // The failover gateway mirrors every metric while its flags hold: the default program's normal form has no
        // emptiness test of its empty mirroring list.
        let gateway = detached
            .artifact
            .nodes
            .iter()
            .find(|node| node.id.as_str() == "mrf_metrics_gateway")
            .unwrap();
        let predicate = gateway.params.get("predicate").unwrap().to_string();
        assert!(!predicate.contains("is_empty"), "{}: {}", name, predicate);
        assert!(
            predicate.contains("multi_region_failover.metric_mirroring.enabled"),
            "{}: {}",
            name,
            predicate
        );

        // It reads back as a bundle in the detached form.
        let read = serde_json::from_value::<ExportBundle>(serde_json::to_value(&detached).unwrap()).unwrap();
        assert_eq!(read, detached, "{}", name);
    }
}

#[test]
fn the_unmodified_detached_export_composes_to_what_the_default_program_does_in_its_own_words() {
    let registry = OperatorRegistry::native();
    for (name, plan, catalog) in default_programs() {
        let received = received_directly(detached(&plan, &catalog).artifact);
        let composed = compose(&plan, &received, &approval(1), &catalog, &registry).unwrap();
        // It does what the default program does, without what does nothing as configured.
        assert_eq!(does(&composed.plan), does(&plan), "{}", name);
        assert!(
            composed
                .ids
                .iter()
                .all(|(local, id)| *id == NodeId::new(local.to_local_id())),
            "{}",
            name
        );
    }
}

#[test]
fn a_native_policy_change_after_publishing_leaves_the_program_its_own_value() {
    let registry = OperatorRegistry::native();
    let (_, plan, catalog) = default_programs().remove(0);
    let received = received_directly(detached(&plan, &catalog).artifact);

    // The metric filterlist changes natively: the default program is lowered again with the new list, and the
    // published program composed again against it.
    let mut changed = serde_json::to_value(&plan).unwrap();
    for node in changed["nodes"].as_array_mut().unwrap() {
        if node["id"] == "dsd_prefix_filter" {
            node["params"]["predicate"] = json!({"not": {"intake_name": {"eq": "tenant.raw.other"}}});
        }
    }
    let changed = Plan::from_value(changed).unwrap();
    let changed_catalog = catalog_for(&changed);
    let composed = compose(&changed, &received, &approval(1), &changed_catalog, &registry).unwrap();

    // The program owns its filterlist: the node is the program's, with the value it was published with, as the
    // detached export writes it in normal form.
    let filter = &composed.ids[&"dsd_prefix_filter".parse::<ArtifactNodeId>().unwrap()];
    let node = composed.plan.node(filter).unwrap();
    assert_eq!(
        node.params.get("predicate").unwrap(),
        &json!({"not": {"intake_name": {"eq": "tenant.raw.blocked"}}})
    );

    // Status names the node whose native settings the program no longer follows, and only it; before the change,
    // nothing differed. The nodes with parameters, as lowering writes them from native settings.
    let every = |plan: &Plan| {
        plan.nodes
            .iter()
            .filter(|node| node.params.iter().next().is_some())
            .map(|node| node.id.clone())
            .collect::<BTreeSet<_>>()
    };
    // Status compares core plans: the default program's, and the one running.
    let changed_core = elaborate_default(&changed, &changed_catalog, &registry).plan;
    let differs = crate::export::node_differences(&changed_core, &composed.plan, &every(&changed_core), &registry);
    assert_eq!(differs, ["dsd_prefix_filter".parse::<NodeId>().unwrap()]);
    let before = compose(&plan, &received, &approval(1), &catalog, &registry).unwrap();
    let core = elaborate_default(&plan, &catalog, &registry).plan;
    assert!(crate::export::node_differences(&core, &before.plan, &every(&core), &registry).is_empty());
}

/// The default program's catalog with a recipient directory: `primary`, `mrf`, `cluster_agent`, and two additional
/// endpoints named by their URLs, the second with two API keys.
fn with_directory(catalog: MemoryCatalog) -> MemoryCatalog {
    let entry = |recipients: &[&str], host: &str, endpoint: &str| crate::validate::CatalogRecipient {
        recipients: recipients.iter().map(|id| id.parse().unwrap()).collect(),
        host: host.parse().unwrap(),
        endpoint: endpoint.to_string(),
    };
    [
        (
            "primary",
            entry(&["dd_out/primary/0"], "dd_out", "https://app.example.com"),
        ),
        (
            "https://a.example.com",
            entry(&["dd_out/additional_0/0"], "dd_out", "https://a.example.com"),
        ),
        (
            "https://b.example.com",
            entry(
                &["dd_out/additional_1/0", "dd_out/additional_1/1"],
                "dd_out",
                "https://b.example.com",
            ),
        ),
        (
            "mrf",
            entry(&["mrf_dd_out/primary/0"], "mrf_dd_out", "https://mrf.example.com"),
        ),
        (
            "cluster_agent",
            entry(
                &["cluster_agent_out/primary/0"],
                "cluster_agent_out",
                "https://cluster-agent:5005",
            ),
        ),
    ]
    .into_iter()
    .fold(catalog, |catalog, (name, entry)| {
        catalog.with_recipient(name.parse().unwrap(), entry)
    })
}

/// Returns what each `emit` node of an artifact names: its output, or `to` and its recipient names.
fn emits(artifact: &PolicyArtifact) -> Vec<(String, Value)> {
    artifact
        .nodes
        .iter()
        .filter(|node| node.op.as_str() == names::EMIT)
        .map(|node| (node.id.to_string(), serde_json::to_value(&node.params).unwrap()))
        .collect()
}

#[test]
fn the_detached_export_addresses_native_outputs_to_their_recipients_and_composes_back_to_them() {
    let registry = OperatorRegistry::native();
    let (_, plan, catalog) = default_programs().remove(0);
    let catalog = with_directory(catalog);
    let detached = detached(&plan, &catalog);
    assert_eq!(
        emits(&detached.artifact),
        [
            // The client telemetry tap has no recipients: it stays addressed to its output.
            (
                "dsd_post_agg_filter_fanout_0".to_string(),
                json!({"output": "dsd_client_telemetry"})
            ),
            ("metrics_fanout_0".to_string(), json!({"to": ["primary"]})),
            (
                "metrics_routing_filter_0_true".to_string(),
                json!({"to": ["https://a.example.com"]})
            ),
            // A URL stands for every API key's recipient of its endpoint.
            (
                "metrics_routing_filter_1_true".to_string(),
                json!({"to": ["https://b.example.com"]})
            ),
            ("mrf_metrics_gateway_true".to_string(), json!({"to": ["mrf"]})),
            ("af_metrics_gateway_true".to_string(), json!({"to": ["cluster_agent"]})),
        ]
    );

    // Composed back, each emit delivers to the output it was written from: the default program, with its hash.
    let received = received_directly(detached.artifact.clone());
    let composed = compose(&plan, &received, &approval(1), &catalog, &registry).unwrap();
    assert_eq!(does(&composed.plan), does(&plan));

    // The names' order and spelling do not matter, only the recipients they stand for: two names whose recipients
    // together are an output's deliver to it.
    let demand = crate::compose::demanded_outputs(&detached.artifact, &catalog).unwrap();
    assert_eq!(demand.len(), 5);
    assert!(demand
        .iter()
        .all(|demand| catalog.output_for(&demand.recipients).is_some()));
}

#[test]
fn emits_to_recipients_wait_for_unknown_names_and_outputs_and_are_refused_across_hosts() {
    let registry = OperatorRegistry::native();
    let (_, plan, catalog) = default_programs().remove(0);
    let catalog = with_directory(catalog);
    let detached = detached(&plan, &catalog);
    let with_main_to = |to: Value| {
        let mut document = serde_json::to_value(&detached.artifact).unwrap();
        node(&mut document, "metrics_fanout_0")["params"] = json!({"to": to});
        received_directly(PolicyArtifact::from_value(document).unwrap())
    };
    let composing = |artifact: &ReceivedArtifact, in_effect: bool| {
        let approval = Approval {
            in_effect,
            ..approval(1)
        };
        compose(&plan, artifact, &approval, &catalog, &registry)
    };

    // A name the directory does not have: a new artifact waits for it.
    let unknown = with_main_to(json!(["team_z"]));
    let error = composing(&unknown, false).unwrap_err();
    assert!(matches!(&error, ComposeError::UnknownRecipient { recipient, .. } if recipient.as_str() == "team_z"));
    assert!(
        error.to_string().starts_with("Waiting for recipient `team_z`"),
        "{}",
        error
    );

    // Recipients no output delivers to exactly: it waits for the host to add one, which its demand asks for.
    let unserved = with_main_to(json!(["primary", "https://a.example.com"]));
    let error = composing(&unserved, false).unwrap_err();
    assert!(matches!(error, ComposeError::PendingRecipients { .. }), "{}", error);
    assert_eq!(
        error.to_string(),
        "Waiting for an output to [primary, \"https://a.example.com\"]: artifact node `metrics_fanout_0` emits to these \
         recipients, and no output delivers to exactly them yet. The host adds one; whatever runs keeps running."
    );
    let demand = crate::compose::demanded_outputs(&unserved.artifact, &catalog).unwrap();
    let wanted = demand
        .iter()
        .find(|demand| demand.recipients.len() == 2 && demand.host.as_str() == "dd_out")
        .unwrap();
    assert_eq!(wanted.emit.as_str(), "metrics_fanout_0");

    // The artifact in effect keeps running: the emit becomes a counted drop, and it runs degraded.
    let composed = composing(&unserved, true).unwrap();
    assert_eq!(
        composed
            .plan
            .node(&"metrics_fanout_0".parse().unwrap())
            .unwrap()
            .op
            .as_str(),
        names::DROP
    );
    assert!(composed.outcome.is_degraded());

    // Recipients of two hosts: refused, naming each host.
    let error = composing(&with_main_to(json!(["primary", "mrf"])), false).unwrap_err();
    assert_eq!(
        error.to_string(),
        "Artifact node `metrics_fanout_0` emits to [primary, mrf], recipients of more than one delivery host (`primary` \
         on `dd_out`, `mrf` on `mrf_dd_out`). An output is one encoder stream on one host: emit to each host's \
         recipients from a branch of its own."
    );
}

#[test]
fn an_emit_in_effect_delivers_to_its_recipients_that_remain_and_counts_what_the_others_would_have_received() {
    let registry = OperatorRegistry::native();
    let (_, plan, catalog) = default_programs().remove(0);
    let catalog = with_directory(catalog);
    let detached = detached(&plan, &catalog);
    let mut document = serde_json::to_value(&detached.artifact).unwrap();
    // The main emit names the primary endpoint and an endpoint the configuration no longer has.
    node(&mut document, "metrics_fanout_0")["params"] = json!({"to": ["primary", "https://gone.example.com"]});
    let artifact = received_directly(PolicyArtifact::from_value(document).unwrap());
    let composing = |in_effect: bool| {
        let approval = Approval {
            in_effect,
            ..approval(1)
        };
        compose(&plan, &artifact, &approval, &catalog, &registry)
    };

    // A new artifact waits for the recipient it names that the directory does not have.
    let error = composing(false).unwrap_err();
    assert!(
        matches!(
            &error,
            ComposeError::UnknownRecipient { recipient, .. } if recipient.as_str() == "https://gone.example.com"
        ),
        "{}",
        error
    );

    // In effect, it goes on delivering to the primary endpoint, through the output with exactly its recipient, and
    // counts what the other would have received at a drop beside it.
    let composed = composing(true).unwrap();
    let id = |id: &str| id.parse::<NodeId>().unwrap();
    let op = |node: &str| composed.plan.node(&id(node)).unwrap().op.as_str().to_string();
    assert_eq!(op("metrics_fanout_0"), names::TEE);
    assert_eq!(op("metrics_fanout_0_delivered"), names::EMIT);
    assert_eq!(op("metrics_fanout_0_missing"), names::DROP);
    assert_eq!(
        composed
            .plan
            .node(&id("metrics_fanout_0_delivered"))
            .unwrap()
            .params
            .get("output"),
        Some(&json!("dd_metrics_encode"))
    );
    let edges = composed
        .plan
        .edges
        .iter()
        .map(|edge| format!("{} -> {}", edge.from, edge.to))
        .collect::<Vec<_>>();
    for edge in [
        "metrics_fanout_0.0 -> metrics_fanout_0_delivered.in",
        "metrics_fanout_0.1 -> metrics_fanout_0_missing.in",
    ] {
        assert!(edges.iter().any(|found| found == edge), "{} in {:?}", edge, edges);
    }
    assert!(composed.report.is_valid(), "{:?}", composed.report.findings());
    let ComposeOutcome::Degraded(dropped) = &composed.outcome else {
        panic!("the artifact in effect runs degraded")
    };
    assert_eq!(
        dropped.iter().map(ToString::to_string).collect::<Vec<_>>(),
        [
            "the emits to [\"https://gone.example.com\"] are dropped until they are available again: the host has no \
          recipient `https://gone.example.com`; the `emit` still delivers to [primary]"
        ]
    );
    assert_eq!(dropped[0].nodes, [id("metrics_fanout_0_missing")]);
    // The artifact's emit's plan ID is the `tee` that took its place.
    assert_eq!(
        composed.ids[&"metrics_fanout_0".parse::<ArtifactNodeId>().unwrap()],
        id("metrics_fanout_0")
    );

    // Its demand is the set that remains, for which a host adds an output when none delivers to exactly it.
    let demand = crate::compose::demanded_outputs(&artifact.artifact, &catalog).unwrap();
    assert!(demand
        .iter()
        .any(|demand| demand.recipients == ["dd_out/primary/0".parse().unwrap()]));

    // With none of its recipients left, the emit in effect is a counted drop, as before.
    let mut document = serde_json::to_value(&detached.artifact).unwrap();
    node(&mut document, "metrics_fanout_0")["params"] = json!({"to": ["https://gone.example.com"]});
    let gone = received_directly(PolicyArtifact::from_value(document).unwrap());
    let approval = Approval {
        in_effect: true,
        ..approval(1)
    };
    let composed = compose(&plan, &gone, &approval, &catalog, &registry).unwrap();
    assert_eq!(
        composed.plan.node(&id("metrics_fanout_0")).unwrap().op.as_str(),
        names::DROP
    );
}

/// Returns what `plan` does with every record: the hash of its core plan without what does nothing as configured, as
/// the detached export leaves it out.
fn does(plan: &Plan) -> crate::canonical::PlanHash {
    let registry = OperatorRegistry::native();
    let core = elaborate_default(plan, &catalog_for(plan), &registry).plan;
    crate::export::eliminate(&core, &registry).plan.hash().unwrap()
}

#[test]
fn recipient_names_are_quoted_with_their_quotes_and_backslashes_escaped() {
    let names = ["primary", r#"https://a"b\c.example.com"#]
        .iter()
        .map(|name| name.parse::<RecipientName>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(describe_names(&names), r#"[primary, "https://a\"b\\c.example.com"]"#);
}
