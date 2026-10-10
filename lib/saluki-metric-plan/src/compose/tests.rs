//! Tests of receipt and composition.
//!
//! Artifacts are edits of the default program's export, written by hand as changes to its JSON document. Expected
//! correspondences, errors, and diffs are written by hand from the default-program fixtures.

use std::collections::{BTreeMap, BTreeSet};

use proptest::{collection::vec, prelude::*, sample::select};
use serde_json::{json, Value};

use super::*;
use crate::{
    canonical::ResolvedValues,
    export::{export, ExportBundle},
    install::diff::{diff, NodeChange},
    ir::{Plan, PlanProvenance},
    testing::{
        arb_predicate, default_program_catalog, predicate_json, with_opaque_gateways, DEFAULT_PROGRAM_JSON,
        NO_AGGREGATION_PIPELINE_OFF_JSON,
    },
    validate::{CatalogBinding, Location, MemoryCatalog, Rule},
};

const ZERO_HASH: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

/// A change made to an artifact document, for table-driven tests.
type Mutation = fn(&mut Value);

/// The default-program fixtures: both configurations, each with its gateways as routing operators and as opaque hosts.
fn default_programs() -> Vec<(&'static str, Plan)> {
    let read = |value: Value| Plan::from_value(value).unwrap();
    vec![
        ("default program", Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap()),
        (
            "no-aggregation pipeline off",
            Plan::from_json(NO_AGGREGATION_PIPELINE_OFF_JSON).unwrap(),
        ),
        (
            "default program, opaque gateways",
            read(with_opaque_gateways(DEFAULT_PROGRAM_JSON)),
        ),
        (
            "no-aggregation pipeline off, opaque gateways",
            read(with_opaque_gateways(NO_AGGREGATION_PIPELINE_OFF_JSON)),
        ),
    ]
}

/// Returns the host's catalog of a default program: the fixtures' catalog, with every binding the program resolves
/// carrying the value it resolves to, as a host's catalog carries the values lowering writes.
fn catalog_for(plan: &Plan) -> MemoryCatalog {
    fn collect(value: &Value, found: &mut BTreeMap<BindingName, Value>) {
        match value {
            Value::Object(members) => {
                if let (Some(Value::String(name)), Some(resolved)) =
                    (members.get(BINDING_MEMBER), members.get(RESOLVED_MEMBER))
                {
                    found.insert(name.parse().unwrap(), resolved.clone());
                }
                members.values().for_each(|member| collect(member, found));
            }
            Value::Array(values) => values.iter().for_each(|value| collect(value, found)),
            _ => {}
        }
    }

    let mut found = BTreeMap::new();
    for node in &plan.nodes {
        node.params.iter().for_each(|(_, value)| collect(value, &mut found));
    }
    let mut catalog = default_program_catalog();
    for (name, value) in found {
        let declared = catalog
            .binding(&name)
            .expect("the catalog declares every binding")
            .clone();
        catalog = catalog.with_catalog_binding(
            name,
            CatalogBinding {
                value: Some(value),
                ..declared
            },
        );
    }
    catalog
}

/// A default program, the host's catalog for it, and the export of its `post_enrich` scope.
struct Fixture {
    plan: Plan,
    catalog: MemoryCatalog,
    export: ExportBundle,
}

impl Fixture {
    fn new(plan: Plan) -> Self {
        let catalog = catalog_for(&plan);
        let export = export(&plan, &"post_enrich".parse().unwrap(), &catalog, &BTreeMap::new()).unwrap();
        Self { plan, catalog, export }
    }

    fn default_program() -> Self {
        Self::new(Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap())
    }

    /// Returns `true` if `id` is a reserved node of the default program's scope.
    fn is_scope_node(&self, id: &NodeId) -> bool {
        id.is_reserved()
            && self
                .export
                .artifact
                .nodes
                .iter()
                .any(|node| node.id.as_str() == id.local().as_str())
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

fn approval(revision: u64) -> Approval {
    Approval {
        revision,
        approved_against: None,
        active_plan_hash: None,
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
            provenance,
            edges,
        } => {
            let parts = [
                (*operator, "operator"),
                (*params, "params"),
                (*provenance, "provenance"),
                (*edges, "edges"),
            ];
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
    for (name, plan) in default_programs() {
        let fixture = Fixture::new(plan);
        let received = receive_artifact(fixture.artifact()).unwrap();
        let composed = compose(&fixture.plan, &received, &approval(7), &fixture.catalog, &registry).unwrap();

        // The same nodes, edges, outputs, and scopes, in the same order: the same plan, with the same hash.
        let plan = &fixture.plan;
        assert_eq!(composed.plan.nodes, plan.nodes, "{}", name);
        assert_eq!(composed.plan.edges, plan.edges, "{}", name);
        assert_eq!(composed.plan.outputs, plan.outputs, "{}", name);
        assert_eq!(composed.plan.scopes, plan.scopes, "{}", name);
        assert_eq!(
            composed.plan.hash(ResolvedValues::Include).unwrap(),
            plan.hash(ResolvedValues::Include).unwrap(),
            "{}",
            name
        );
        // Only the plan's own provenance, which is not hashed, says that an artifact was composed.
        assert_eq!(
            composed.plan.provenance,
            Some(PlanProvenance {
                config_change: None,
                artifacts: vec![ArtifactRevision {
                    revision: 7,
                    hash: received.hash
                }],
            }),
            "{}",
            name
        );
        assert!(!composed.stale_base && !composed.stale_review, "{}", name);
        assert_eq!(
            composed.report,
            Validator::new(&registry, &fixture.catalog).validate(plan),
            "{}",
            name
        );

        // Every artifact node is the default program's node of the same ID, unmodified.
        for artifact_node in &fixture.export.artifact.nodes {
            let correspondence = composed.correspondence.get(&artifact_node.id).unwrap();
            assert_eq!(correspondence.kind, CorrespondenceKind::Unmodified, "{}", name);
            assert_eq!(
                correspondence.node.to_string(),
                artifact_node.id.to_string(),
                "{}",
                name
            );
        }
        assert_eq!(
            composed.correspondence.iter().count(),
            fixture.export.artifact.nodes.len()
        );
        assert!(composed.correspondence.removed().is_empty(), "{}", name);
    }
}

/// What composing an accepted edit gives.
struct Accepted {
    /// Every artifact node that is not the default program's node unmodified: its ID, the plan node it became, and
    /// how it relates to the default program.
    changed: &'static [(&'static str, &'static str, CorrespondenceKind)],

    /// The reserved nodes of the scope the artifact leaves out.
    removed: &'static [&'static str],

    /// Every node the diff against the default program does not list as unchanged, with how it lists it.
    diff: &'static [(&'static str, &'static str)],

    /// Whether the base is stale.
    stale_base: bool,
}

#[test]
fn edits_of_the_export_compose_with_their_correspondence_and_provenance() {
    use CorrespondenceKind::{Added, Modified};

    let kinds_list = json!(["counter", "rate", "gauge", "set"]);
    let kinds_blob = PlanHash::of_value(&kinds_list).unwrap();

    let cases: Vec<(&str, Mutation, Accepted)> = vec![
        (
            "unmodified",
            |_| {},
            Accepted {
                changed: &[],
                removed: &[],
                diff: &[],
                stale_base: false,
            },
        ),
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
                node(document, "metrics_policy")["params"]["ports"] = json!(4);
                edge_to(document, "af_metrics_gateway.in")["from"] = json!("metrics_policy.3");
            },
            Accepted {
                // A `tee` copies what enters it to every port, however many it has, so the drops of the other branches
                // are fed as before, and keep their native provenance.
                changed: &[("metrics_policy", "metrics_policy", Modified)],
                removed: &[
                    "mrf_metrics_gateway",
                    "mrf_metrics_gateway_true",
                    "mrf_metrics_gateway_false",
                ],
                diff: &[
                    ("metrics_policy", "changed params provenance edges"),
                    ("af_metrics_gateway", "changed edges"),
                    ("mrf_metrics_gateway", "removed"),
                    ("mrf_metrics_gateway_true", "removed"),
                    ("mrf_metrics_gateway_false", "removed"),
                ],
                stale_base: false,
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
                changed: &[
                    ("team_b", "policy/team_b", Added),
                    ("team_b_true", "policy/team_b_true", Added),
                    ("team_b_false", "policy/team_b_false", Added),
                ],
                removed: &[
                    "metrics_routing_filter_0",
                    "metrics_routing_filter_0_true",
                    "metrics_routing_filter_0_false",
                ],
                diff: &[
                    ("metrics_policy", "changed edges"),
                    ("policy/team_b", "added"),
                    ("policy/team_b_true", "added"),
                    ("policy/team_b_false", "added"),
                    ("metrics_routing_filter_0", "removed"),
                    ("metrics_routing_filter_0_true", "removed"),
                    ("metrics_routing_filter_0_false", "removed"),
                ],
                stale_base: false,
            },
        ),
        (
            "a new node",
            |document| {
                let nodes = document["nodes"].as_array_mut().unwrap();
                nodes.insert(
                    0,
                    json!({"id": "drop_debug", "op": "split",
                           "params": {"predicate": {"name": {"prefix": "demo.debug."}}}}),
                );
                nodes.insert(1, json!({"id": "drop_debug_true", "op": "drop"}));
                edge_to(document, "metrics_policy.in")["from"] = json!("drop_debug.false");
                push_edge(document, "input.metrics", "drop_debug.in");
                push_edge(document, "drop_debug.true", "drop_debug_true.in");
            },
            Accepted {
                // Every drop of the scope now drops what is left after the new filter, which native configuration did
                // not express: their loss is the artifact's.
                changed: &[
                    ("drop_debug", "policy/drop_debug", Added),
                    ("drop_debug_true", "policy/drop_debug_true", Added),
                    (
                        "metrics_routing_filter_0_false",
                        "metrics_routing_filter_0_false",
                        Modified,
                    ),
                    (
                        "metrics_routing_filter_1_false",
                        "metrics_routing_filter_1_false",
                        Modified,
                    ),
                    ("mrf_metrics_gateway_false", "mrf_metrics_gateway_false", Modified),
                    ("af_metrics_gateway_false", "af_metrics_gateway_false", Modified),
                ],
                removed: &[],
                // The node feeding the scope's input is outside the scope, but the edge at its output port changes.
                diff: &[
                    ("metrics_enrich_host_tags", "changed edges"),
                    ("policy/drop_debug", "added"),
                    ("policy/drop_debug_true", "added"),
                    ("metrics_policy", "changed edges"),
                    ("metrics_routing_filter_0_false", "changed provenance"),
                    ("metrics_routing_filter_1_false", "changed provenance"),
                    ("mrf_metrics_gateway_false", "changed provenance"),
                    ("af_metrics_gateway_false", "changed provenance"),
                ],
                stale_base: false,
            },
        ),
        (
            "the primary stream rerouted into a kept drop",
            |document| {
                // The `tee` port that fed the primary output now feeds the drop of the first routing filter, and the
                // filter's rejects go to a new drop: the kept drop discards the whole primary stream.
                remove_nodes(document, &["metrics_policy_0"]);
                edge_to(document, "metrics_routing_filter_0_false.in")["from"] = json!("metrics_policy.0");
                push_node(document, json!({"id": "discard", "op": "drop"}));
                push_edge(document, "metrics_routing_filter_0.false", "discard.in");
            },
            Accepted {
                changed: &[
                    (
                        "metrics_routing_filter_0_false",
                        "metrics_routing_filter_0_false",
                        Modified,
                    ),
                    ("discard", "policy/discard", Added),
                ],
                removed: &["metrics_policy_0"],
                diff: &[
                    ("metrics_policy", "changed edges"),
                    ("metrics_routing_filter_0", "changed edges"),
                    ("metrics_routing_filter_0_false", "changed provenance edges"),
                    ("policy/discard", "added"),
                    ("metrics_policy_0", "removed"),
                ],
                stale_base: false,
            },
        ),
        (
            "the primary output and a kept drop swapped",
            |document| {
                edge_to(document, "metrics_policy_0.in")["from"] = json!("metrics_routing_filter_0.false");
                edge_to(document, "metrics_routing_filter_0_false.in")["from"] = json!("metrics_policy.0");
            },
            Accepted {
                // The `emit` loses nothing, so it keeps its native provenance whatever feeds it; the drop does not.
                changed: &[(
                    "metrics_routing_filter_0_false",
                    "metrics_routing_filter_0_false",
                    Modified,
                )],
                removed: &[],
                diff: &[
                    ("metrics_policy", "changed edges"),
                    ("metrics_policy_0", "changed edges"),
                    ("metrics_routing_filter_0", "changed edges"),
                    ("metrics_routing_filter_0_false", "changed provenance edges"),
                ],
                stale_base: false,
            },
        ),
        (
            "modified parameters",
            |document| {
                node(document, "af_metrics_gateway")["params"]["predicate"]["all"][0] = json!({"kind_in": ["counter"]});
            },
            Accepted {
                // The gateway's drop now drops what the new predicate rejects.
                changed: &[
                    ("af_metrics_gateway", "af_metrics_gateway", Modified),
                    ("af_metrics_gateway_false", "af_metrics_gateway_false", Modified),
                ],
                removed: &[],
                diff: &[
                    ("af_metrics_gateway", "changed params provenance"),
                    ("af_metrics_gateway_false", "changed provenance"),
                ],
                stale_base: false,
            },
        ),
        (
            "a binding replaced by a literal",
            |document| {
                node(document, "mrf_metrics_gateway")["params"]["predicate"]["all"][1] = json!({"flag": true});
            },
            Accepted {
                changed: &[
                    ("mrf_metrics_gateway", "mrf_metrics_gateway", Modified),
                    ("mrf_metrics_gateway_false", "mrf_metrics_gateway_false", Modified),
                ],
                removed: &[],
                diff: &[
                    ("mrf_metrics_gateway", "changed params provenance"),
                    ("mrf_metrics_gateway_false", "changed provenance"),
                ],
                stale_base: false,
            },
        ),
        (
            "parameters written in another order",
            |document| {
                node(document, "af_metrics_gateway")["params"]["predicate"]["all"][0] =
                    json!({"kind_in": ["set", "gauge", "rate", "counter"]});
            },
            Accepted {
                changed: &[],
                removed: &[],
                diff: &[],
                stale_base: false,
            },
        ),
        (
            "a stale base",
            |document| document["base"]["program_hash"] = json!(ZERO_HASH),
            Accepted {
                changed: &[],
                removed: &[],
                diff: &[],
                stale_base: true,
            },
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
    all_cases.push((
        "a blob reference",
        with_blob,
        Accepted {
            changed: &[],
            removed: &[],
            diff: &[],
            stale_base: false,
        },
    ));

    for (what, document, expected) in all_cases {
        let received = receive_artifact(document).unwrap_or_else(|e| panic!("{}: {}", what, e));
        let composed = compose(&fixture.plan, &received, &approval(3), &fixture.catalog, &registry)
            .unwrap_or_else(|e| panic!("{}: {}", what, e));
        assert!(composed.report.is_valid(), "{}:\n{}", what, composed.report);
        assert_eq!(composed.stale_base, expected.stale_base, "{}", what);
        assert!(!composed.stale_review, "{}", what);

        let revision = ArtifactRevision {
            revision: 3,
            hash: received.hash,
        };
        let changed = composed
            .correspondence
            .iter()
            .filter(|(_, correspondence)| correspondence.kind != CorrespondenceKind::Unmodified)
            .map(|(id, correspondence)| {
                (
                    id.to_string(),
                    correspondence.node.to_string(),
                    format!("{:?}", correspondence.kind),
                )
            })
            .collect::<Vec<_>>();
        let mut expected_changed = expected
            .changed
            .iter()
            .map(|(id, node, kind)| (id.to_string(), node.to_string(), format!("{:?}", kind)))
            .collect::<Vec<_>>();
        expected_changed.sort();
        assert_eq!(changed, expected_changed, "{}", what);
        let removed = composed
            .correspondence
            .removed()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert_eq!(removed, expected.removed, "{}", what);

        // Unmodified nodes are the default program's own; every other artifact node has the artifact's provenance,
        // which the plan lists.
        for (_, correspondence) in composed.correspondence.iter() {
            let node = composed.plan.node(&correspondence.node).unwrap();
            match correspondence.kind {
                CorrespondenceKind::Unmodified => {
                    assert_eq!(Some(node), fixture.plan.node(&correspondence.node), "{}", what)
                }
                _ => assert_eq!(
                    node.provenance,
                    Some(Provenance::Artifact(revision.clone())),
                    "{}",
                    what
                ),
            }
        }
        assert_eq!(
            composed.plan.provenance.as_ref().unwrap().artifacts,
            [revision],
            "{}",
            what
        );

        let diff = diff(&fixture.plan, &composed.plan, &registry);
        let listed = diff
            .changes()
            .map(|node| (node.id.to_string(), describe(&node.change)))
            .collect::<Vec<_>>();
        let expected_diff = expected
            .diff
            .iter()
            .map(|(id, change)| (id.to_string(), change.to_string()))
            .collect::<Vec<_>>();
        assert_eq!(listed, expected_diff, "{}", what);
    }
}

#[test]
fn kept_host_components_keep_native_authority_only_while_fed_as_in_the_default_program() {
    use CorrespondenceKind::{Modified, Unmodified};

    const GATEWAYS: [&str; 4] = [
        "metrics_routing_filter_0",
        "metrics_routing_filter_1",
        "mrf_metrics_gateway",
        "af_metrics_gateway",
    ];
    let fixture = Fixture::new(Plan::from_value(with_opaque_gateways(DEFAULT_PROGRAM_JSON)).unwrap());
    let kind = |composed: &Composed, id: &str| composed.correspondence.get(&id.parse().unwrap()).unwrap().kind;
    let trusted_host_errors = |composed: &Composed| {
        composed
            .report
            .errors()
            .filter(|finding| finding.rule == Rule::TrustedHost)
            .map(|finding| match &finding.location {
                crate::validate::Location::Node { id, .. } => id.to_string(),
                other => panic!("a host component's finding is at its node, not {:?}", other),
            })
            .collect::<Vec<_>>()
    };

    // Without the multi-region branch, the other gateways still get a copy of everything: they stay native.
    let mut document = fixture.artifact();
    remove_nodes(&mut document, &["mrf_metrics_gateway", "mrf_metrics_gateway_out"]);
    node(&mut document, "metrics_policy")["params"]["ports"] = json!(4);
    edge_to(&mut document, "af_metrics_gateway.in")["from"] = json!("metrics_policy.3");
    let composed = fixture.compose(document, &approval(1)).unwrap();
    assert!(composed.report.is_valid(), "{}", composed.report);
    assert_eq!(kind(&composed, "metrics_policy"), Modified);
    for gateway in GATEWAYS.iter().filter(|gateway| **gateway != "mrf_metrics_gateway") {
        assert_eq!(kind(&composed, gateway), Unmodified, "{}", gateway);
    }

    // Behind a new filter, each gateway gets another stream than native configuration placed it on, so it would run
    // with the artifact's authority, which cannot place host components. Their `emit`s lose nothing, and stay native.
    let mut document = fixture.artifact();
    let nodes = document["nodes"].as_array_mut().unwrap();
    nodes.insert(
        0,
        json!({"id": "drop_debug", "op": "split", "params": {"predicate": {"name": {"prefix": "demo.debug."}}}}),
    );
    nodes.insert(1, json!({"id": "drop_debug_true", "op": "drop"}));
    edge_to(&mut document, "metrics_policy.in")["from"] = json!("drop_debug.false");
    push_edge(&mut document, "input.metrics", "drop_debug.in");
    push_edge(&mut document, "drop_debug.true", "drop_debug_true.in");
    let composed = fixture.compose(document, &approval(1)).unwrap();
    assert_eq!(trusted_host_errors(&composed), GATEWAYS);
    for gateway in GATEWAYS {
        assert_eq!(kind(&composed, gateway), Modified, "{}", gateway);
        assert_eq!(kind(&composed, &format!("{}_out", gateway)), Unmodified, "{}", gateway);
    }
}

#[test]
fn artifact_nodes_take_the_place_of_the_scope_in_the_default_programs_order() {
    let fixture = Fixture::default_program();
    let mut document = fixture.artifact();
    let nodes = document["nodes"].as_array_mut().unwrap();
    let split =
        json!({"id": "drop_debug", "op": "split", "params": {"predicate": {"name": {"prefix": "demo.debug."}}}});
    nodes.insert(0, split);
    nodes.insert(1, json!({"id": "drop_debug_true", "op": "drop"}));
    // The first edge now leaves the new split, and the two new edges are the artifact's last.
    edge_to(&mut document, "metrics_policy.in")["from"] = json!("drop_debug.false");
    push_edge(&mut document, "input.metrics", "drop_debug.in");
    push_edge(&mut document, "drop_debug.true", "drop_debug_true.in");
    let composed = fixture.compose(document, &approval(1)).unwrap();

    // The scope is the end of the default program, so the artifact's nodes follow everything before it, in the
    // artifact's order, and so do its edges.
    let ids = composed
        .plan
        .nodes
        .iter()
        .map(|node| node.id.to_string())
        .collect::<Vec<_>>();
    let before = ids.iter().position(|id| id == "policy/drop_debug").unwrap();
    assert_eq!(ids[before - 1], "metrics_enrich_host_tags");
    assert_eq!(
        ids[before..before + 4],
        [
            "policy/drop_debug",
            "policy/drop_debug_true",
            "metrics_policy",
            "metrics_policy_0"
        ]
    );
    assert_eq!(ids.len(), fixture.plan.nodes.len() + 2);
    let edges = composed
        .plan
        .edges
        .iter()
        .map(|edge| format!("{} -> {}", edge.from, edge.to))
        .collect::<Vec<_>>();
    assert_eq!(
        edges[edges.len() - 3..],
        [
            "af_metrics_gateway.false -> af_metrics_gateway_false.in",
            "metrics_enrich_host_tags.out -> policy/drop_debug.in",
            "policy/drop_debug.true -> policy/drop_debug_true.in",
        ]
    );
    let first = edges
        .iter()
        .position(|edge| edge == "policy/drop_debug.false -> metrics_policy.in")
        .unwrap();
    assert_eq!(edges[first - 1], "metrics_enrich.out -> metrics_enrich_host_tags.in");
    assert_eq!(edges.len() - first, 16);
    assert_eq!(edges.len(), fixture.plan.edges.len() + 2);
}

#[test]
fn splicing_fills_the_regions_places_in_order_and_puts_the_rest_after_the_last() {
    let region = |item: &&str| item.starts_with('r');
    // (default, replacement, spliced)
    let cases: &[(&[&str], &[&str], &[&str])] = &[
        (&["a", "r1", "r2", "b"], &["r1", "r2"], &["a", "r1", "r2", "b"]),
        (
            &["a", "r1", "b", "r2", "c"],
            &["x", "y", "z"],
            &["a", "x", "b", "y", "z", "c"],
        ),
        (&["a", "r1", "b", "r2"], &["x"], &["a", "x", "b"]),
        (&["a", "r1", "r2"], &[], &["a"]),
        (&["a", "b"], &["x", "y"], &["a", "b", "x", "y"]),
    ];
    for (default, replacement, expected) in cases {
        assert_eq!(
            splice(default, region, replacement.to_vec()),
            *expected,
            "{:?}",
            default
        );
    }
}

#[test]
fn modified_and_added_nodes_have_their_bindings_resolved_from_the_catalog() {
    let fixture = Fixture::default_program();
    let mut document = fixture.artifact();
    // A different kind filter, and a binding the default program does not use in this scope.
    node(&mut document, "af_metrics_gateway")["params"]["predicate"] = json!({"all": [
        {"kind_in": ["counter"]},
        {"name": {"matches": {"values": {"config": "autoscaling_failover.metrics"},
                              "prefix": {"config": "dogstatsd.metric_filter.match_prefix"}}}}]});
    let composed = fixture.compose(document, &approval(2)).unwrap();
    assert!(composed.report.is_valid(), "{}", composed.report);

    let node = composed.plan.node(&"af_metrics_gateway".parse().unwrap()).unwrap();
    assert_eq!(
        node.params.get("predicate").unwrap(),
        &json!({"all": [
            {"kind_in": ["counter"]},
            {"name": {"matches": {
                "values": {"config": "autoscaling_failover.metrics", "resolved": ["requests.count"]},
                "prefix": {"config": "dogstatsd.metric_filter.match_prefix", "resolved": false}}}}]})
    );
}

#[test]
fn edits_that_cannot_be_composed_are_refused_with_the_reason() {
    let fixture = Fixture::default_program();
    let id = |id: &str| id.parse::<ArtifactNodeId>().unwrap();
    let output = |id: &str| id.parse::<OutputId>().unwrap();
    let post_enrich = "post_enrich".parse::<ScopeName>().unwrap();

    let stateful: Mutation = |document| {
        let plan = Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap();
        let params = unresolved(&plan.node(&"dsd_agg".parse().unwrap()).unwrap().params);
        push_node(document, json!({"id": "agg", "op": "aggregate", "params": params}));
        edge_to(document, "metrics_policy.in")["from"] = json!("agg.out");
        push_edge(document, "input.metrics", "agg.in");
    };
    // (what, mutation, expected error)
    let cases: Vec<(&str, Mutation, ComposeError)> = vec![
        (
            "a stateful operator",
            stateful,
            ComposeError::Stateful {
                node: id("agg"),
                op: "aggregate".parse().unwrap(),
            },
        ),
        (
            "a stateful operator with parameters it does not accept",
            |document| {
                push_node(document, json!({"id": "agg", "op": "aggregate", "params": {}}));
                edge_to(document, "metrics_policy.in")["from"] = json!("agg.out");
                push_edge(document, "input.metrics", "agg.in");
            },
            ComposeError::Stateful {
                node: id("agg"),
                op: "aggregate".parse().unwrap(),
            },
        ),
        (
            "an unknown output",
            |document| {
                node(document, "metrics_policy_0")["params"]["output"] = json!("nowhere");
                document["base"]["outputs"]["nowhere"] = json!(ZERO_HASH);
            },
            ComposeError::RemovedOutput {
                output: output("nowhere"),
                declared: false,
            },
        ),
        (
            "an output the base does not record",
            |document| node(document, "metrics_policy_0")["params"]["output"] = json!("dsd_client_telemetry"),
            ComposeError::UnrecordedOutput {
                node: id("metrics_policy_0"),
                output: output("dsd_client_telemetry"),
            },
        ),
        (
            "an output neither the base records nor the host declares",
            |document| node(document, "metrics_policy_0")["params"]["output"] = json!("metrics_routing_encode_99"),
            ComposeError::UndeclaredOutput {
                node: id("metrics_policy_0"),
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
            "a fingerprint mismatch",
            |document| document["base"]["outputs"]["dd_metrics_encode"] = json!(ZERO_HASH),
            ComposeError::RemovedOutput {
                output: output("dd_metrics_encode"),
                declared: true,
            },
        ),
        (
            "a reserved ID outside the scope",
            |document| rename(document, "metrics_policy_0", "dsd_agg"),
            ComposeError::ReservedOutsideScope {
                node: id("dsd_agg"),
                reserved: "dsd_agg".parse().unwrap(),
                scope: post_enrich.clone(),
            },
        ),
        (
            "an unknown scope",
            |document| document["scope"] = json!("pre_aggregation"),
            ComposeError::UnknownScope {
                scope: "pre_aggregation".parse().unwrap(),
            },
        ),
        (
            "an edge from an undeclared input",
            |document| edge_to(document, "metrics_policy.in")["from"] = json!("input.traces"),
            ComposeError::Malformed {
                message: "edge 0 leaves `input.traces`, but the scope has no input `traces`".to_string(),
            },
        ),
        (
            "an edge into an input",
            |document| push_edge(document, "metrics_policy_0.in", "input.metrics"),
            ComposeError::Malformed {
                message: "edge 14 enters `input.metrics`, which is a scope input; edges leave a scope's inputs"
                    .to_string(),
            },
        ),
        (
            "an edge to an unknown node",
            |document| edge_to(document, "metrics_policy_0.in")["to"] = json!("ghost.in"),
            ComposeError::Malformed {
                message: "edge 1 enters `ghost.in`, but the artifact has no node `ghost`".to_string(),
            },
        ),
        (
            "two nodes with one ID",
            |document| push_node(document, json!({"id": "metrics_policy_0", "op": "drop"})),
            ComposeError::Malformed {
                message: "more than one node has the ID `metrics_policy_0`; give each node its own ID".to_string(),
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
fn errors_about_outputs_and_state_say_what_the_author_can_do() {
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
            ComposeError::UnrecordedOutput {
                node: id("route"),
                output: output("main"),
            },
            "Artifact node `route` emits to output `main`, which the artifact's base does not record. Add the \
             output's fingerprint from the export to `base.outputs`.",
        ),
        (
            ComposeError::Stateful {
                node: id("rollup"),
                op: "aggregate".parse().unwrap(),
            },
            "Artifact node `rollup` runs `aggregate`, which keeps state: artifacts cannot contain stateful operators \
             yet. Remove the node, or keep the processing in native configuration.",
        ),
    ];
    for (error, message) in cases {
        assert_eq!(error.to_string(), message);
    }
}

#[test]
fn the_operators_refused_as_stateful_whatever_their_parameters_are_those_that_keep_keyed_state() {
    // Every node of the default programs whose operator keeps keyed state, as its contract says, runs an operator that
    // composition refuses whatever its parameters, and every other operator there keeps no keyed state.
    let registry = OperatorRegistry::native();
    let mut keyed = Vec::new();
    for (name, plan) in default_programs() {
        for node in &plan.nodes {
            let configured = registry.configure(node.op.as_str(), &node.params).unwrap();
            let is_keyed = matches!(configured.contract().state, StateContract::Keyed(_));
            assert_eq!(keeps_keyed_state(&node.op), is_keyed, "{}: {}", name, node.id);
            if is_keyed && !keyed.contains(&node.op) {
                keyed.push(node.op.clone());
            }
        }
    }
    assert_eq!(keyed, ["aggregate".parse::<OperatorName>().unwrap()]);
    assert!(registry.get(names::AGGREGATE).is_some());
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

    // Kept unmodified, the node is the default program's, with its resolved value.
    assert!(fixture.compose(fixture.artifact(), &approval(1)).is_ok());

    // Modified, its bindings resolve from the catalog, which has no value for one.
    let mut document = fixture.artifact();
    node(&mut document, "mrf_metrics_gateway")["params"]["predicate"]["all"][0] = json!({"flag": true});
    match fixture.compose(document, &approval(1)) {
        Err(Refused::Compose(error)) => assert_eq!(
            error,
            ComposeError::UnresolvedBinding {
                node: "mrf_metrics_gateway".parse().unwrap(),
                binding: name,
            }
        ),
        Err(Refused::Receipt(error)) => panic!("refused on receipt: {}", error),
        Ok(composed) => panic!("expected an unresolved binding, got:\n{}", composed.report),
    }
}

#[test]
fn composed_plans_that_break_a_validation_rule_are_composed_with_the_finding() {
    let fixture = Fixture::default_program();
    let wide_tee: Mutation = |document| {
        node(document, "metrics_policy")["params"]["ports"] = json!(40);
        for port in 5..40 {
            push_node(document, json!({"id": format!("extra_{}", port), "op": "drop"}));
            push_edge(
                document,
                &format!("metrics_policy.{}", port),
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
    // (what, the mapper's interner bytes, the memory the error says the artifact declares, if it is invalid)
    let cases: [(&str, u64, Option<u64>); 4] = [
        ("the native default interner", 64 * 1024, None),
        ("an interner at the limit", 64 * 1024 * 1024, None),
        (
            "an interner of a pebibyte",
            1_125_899_906_842_624,
            Some(1_125_899_906_842_624),
        ),
        (
            "an interner just over the limit",
            64 * 1024 * 1024 + 1,
            Some(67_108_865),
        ),
    ];
    for (what, interner, expected) in cases {
        let mut document = fixture.artifact();
        edge_to(&mut document, "metrics_policy.in")["to"] = json!("mapper.in");
        push_edge(&mut document, "mapper.out", "metrics_policy.in");
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
        // The mapper is the artifact's only node that declares memory, so it declares all of it.
        let expected = expected
            .map(|bytes| {
                (
                    Location::Node {
                        id: "policy/mapper".parse().unwrap(),
                        port: None,
                    },
                    format!(
                        "the nodes of artifact revision 1 ({}) declare {} bytes of memory, more than the limit of \
                         67108864, and this node declares {} of them; reduce its cache and interner sizes",
                        received.hash, bytes, bytes
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
        };
        let composed = fixture.compose(fixture.artifact(), &approval).unwrap();
        assert_eq!(composed.stale_review, stale, "{:?}", approval);
        assert!(!composed.stale_base);
    }
}

#[test]
fn artifacts_beyond_the_limits_or_carrying_resolved_values_are_refused_on_receipt() {
    let fixture = Fixture::default_program();
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
    for i in 0..243 {
        push_node(&mut many, json!({"id": format!("extra_{}", i), "op": "drop"}));
    }
    cases.push(("too many nodes", many, |error| {
        matches!(error, ReceiptError::TooManyNodes { nodes: 257, limit: 256 })
    }));

    // The export's predicates have ten leaves: two per routing filter, four in the multi-region gateway, two in the
    // autoscaling one.
    let mut leaves = fixture.artifact();
    push_node(
        &mut leaves,
        json!({"id": "wide", "op": "split", "params": {"predicate": consts(1015)}}),
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
    let predicate = consts(1015);
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
    let smuggled = json!({"config": "autoscaling_failover.metrics", "resolved": ["smuggled"]});
    let smuggled_hash = PlanHash::of_value(&smuggled).unwrap();
    node(&mut resolved_blob, "af_metrics_gateway")["params"]["predicate"]["all"][1]["name"]["matches"]["values"] =
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
    for i in 0..240 {
        push_node(&mut at_limits, json!({"id": format!("extra_{}", i), "op": "drop"}));
    }
    push_node(
        &mut at_limits,
        json!({"id": "wide", "op": "split", "params": {"predicate": consts(1014)}}),
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

/// The outputs a generated tree delivers to: every output of the scope that accepts every kind.
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

/// The nodes and edges written for a tree, and the gateways it keeps, each with whether a `split` lies between it and
/// the scope's input.
#[derive(Default)]
struct Written {
    nodes: Vec<Value>,
    edges: Vec<Value>,
    kept: Vec<(usize, bool)>,
}

/// Writes a tree as an artifact's nodes and edges, fed from `from`, `filtered` if a `split` lies between `from` and the
/// scope's input.
///
/// Each gateway is kept at most once, since its nodes have fixed IDs; a second occurrence is written as a drop.
fn write_tree(tree: &Tree, from: String, filtered: bool, export: &PolicyArtifact, out: &mut Written) {
    let id = format!("n{}", out.nodes.len());
    let edge_in = |out: &mut Written, to: &str| out.edges.push(json!({"from": from, "to": format!("{}.in", to)}));
    match tree {
        Tree::Kept(gateway) if !out.kept.iter().any(|(kept, _)| kept == gateway) => {
            out.kept.push((*gateway, filtered));
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
            write_tree(yes, format!("{}.true", id), true, export, out);
            write_tree(no, format!("{}.false", id), true, export, out);
        }
        Tree::Tee(branches) => {
            out.nodes
                .push(json!({"id": id, "op": "tee", "params": {"ports": branches.len()}}));
            edge_in(out, &id);
            for (port, branch) in branches.iter().enumerate() {
                write_tree(branch, format!("{}.{}", id, port), filtered, export, out);
            }
        }
    }
}

/// Returns the artifact document of a tree, with the export's base, and the gateways it keeps, each with whether a
/// `split` lies between it and the scope's input.
fn tree_artifact(tree: &Tree, export: &PolicyArtifact) -> (Value, Vec<(usize, bool)>) {
    let mut out = Written::default();
    write_tree(tree, "input.metrics".to_string(), false, export, &mut out);
    let document = json!({
        "ir_version": 1,
        "scope": "post_enrich",
        "base": serde_json::to_value(&export.base).unwrap(),
        "nodes": out.nodes,
        "edges": out.edges,
    });
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
        "metrics_policy",
        "metrics_policy_0",
        "af_metrics_gateway",
        "dsd_agg",
        "metrics_enrich",
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
/// The export is a tree whose every port has one edge, and no edit adds an edge from a port, so a removal leaves a
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
/// base and catalog say what the error says of them.
fn explains(error: &ComposeError, artifact: &PolicyArtifact, fixture: &Fixture) -> Option<&'static str> {
    let node = |id: &ArtifactNodeId| artifact.nodes.iter().find(|node| node.id == *id);
    let emits_to = |output: &OutputId| {
        artifact
            .nodes
            .iter()
            .any(|node| node.op.as_str() == names::EMIT && emitted_output(&node.params).as_ref() == Some(output))
    };
    match error {
        ComposeError::ReservedOutsideScope {
            node: id,
            reserved,
            scope,
        } => {
            let outside = fixture.plan.node(reserved).is_some() && !fixture.is_scope_node(reserved);
            (node(id).is_some()
                && *reserved == NodeId::reserved(id.to_local_id())
                && outside
                && *scope == artifact.scope)
                .then_some("a node has the ID of a reserved node outside the scope")
        }
        ComposeError::Stateful { node: id, op } => node(id)
            .filter(|node| node.op == *op && op.as_str() == names::AGGREGATE)
            .map(|_| "a node aggregates"),
        ComposeError::UnrecordedOutput { node: id, output } => node(id)
            .filter(|node| node.op.as_str() == names::EMIT && emitted_output(&node.params).as_ref() == Some(output))
            .filter(|_| !artifact.base.outputs.contains_key(output) && fixture.catalog.output(output).is_some())
            .map(|_| "an emit delivers to an output the base does not record"),
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
                .filter(|_| {
                    !artifact.base.outputs.contains_key(output)
                        && fixture.catalog.output(output).is_none()
                        && *declared == all_declared
                })
                .map(|_| "an emit delivers to an output the host does not declare")
        }
        ComposeError::RemovedOutput { output, declared } => {
            let recorded = artifact.base.outputs.get(output);
            let current = fixture.catalog.output(output).map(|declared| declared.fingerprint);
            let removed = match (recorded, current) {
                (Some(_), None) => !declared,
                (Some(recorded), Some(current)) => *declared && *recorded != current,
                (None, _) => false,
            };
            (emits_to(output) && removed)
                .then_some("an emit delivers to an output the host no longer declares as it was")
        }
        ComposeError::Malformed { .. } => {
            let ids = artifact.nodes.iter().map(|node| &node.id).collect::<BTreeSet<_>>();
            let unknown_end = artifact.edges.iter().any(|edge| {
                [&edge.from, &edge.to].into_iter().any(|end| match end {
                    ArtifactPortRef::Node { node, .. } => !ids.contains(node),
                    ArtifactPortRef::Input(input) => input.as_str() != "metrics",
                }) || matches!(edge.to, ArtifactPortRef::Input(_))
            });
            (ids.len() < artifact.nodes.len() || unknown_end).then_some("two nodes share an ID, or an edge has no end")
        }
        ComposeError::UnknownScope { .. }
        | ComposeError::DefaultProgram { .. }
        | ComposeError::UnresolvedBinding { .. } => None,
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
        prop_assert!(!composed.stale_base);

        // Kept gateways are the default program's nodes, but a kept drop behind a `split` of the artifact's, which
        // drops another stream than native configuration does, has the artifact's provenance. Every other artifact
        // node is the artifact's own, and every reserved node of the scope not kept is removed.
        let kept_nodes = kept
            .iter()
            .flat_map(|(gateway, _)| {
                ["", "_true", "_false"].map(|suffix| format!("{}{}", GATEWAYS[*gateway].0, suffix))
            })
            .collect::<Vec<_>>();
        let refed_drops = kept
            .iter()
            .filter(|(_, filtered)| *filtered)
            .map(|(gateway, _)| format!("{}_false", GATEWAYS[*gateway].0))
            .collect::<Vec<_>>();
        for node in &received.artifact.nodes {
            let correspondence = composed.correspondence.get(&node.id).unwrap();
            if refed_drops.contains(&node.id.to_string()) {
                prop_assert_eq!(correspondence.kind, CorrespondenceKind::Modified);
                prop_assert_eq!(correspondence.node.to_string(), node.id.to_string());
                let composed_node = composed.plan.node(&correspondence.node).unwrap();
                prop_assert!(matches!(composed_node.provenance, Some(Provenance::Artifact(_))));
            } else if kept_nodes.contains(&node.id.to_string()) {
                prop_assert_eq!(correspondence.kind, CorrespondenceKind::Unmodified);
                prop_assert_eq!(composed.plan.node(&correspondence.node), fixture.plan.node(&correspondence.node));
            } else {
                prop_assert_eq!(correspondence.kind, CorrespondenceKind::Added);
                prop_assert_eq!(correspondence.node.to_string(), format!("policy/{}", node.id));
            }
        }
        let removed = composed.correspondence.removed().iter().map(ToString::to_string).collect::<Vec<_>>();
        let expected_removed = fixture
            .export
            .artifact
            .nodes
            .iter()
            .map(|node| node.id.to_string())
            .filter(|id| !kept_nodes.contains(id))
            .collect::<Vec<_>>();
        prop_assert_eq!(removed, expected_removed);

        // Nothing outside the scope changes, but the edge leaving the port that feeds the scope's input.
        let diff = diff(&fixture.plan, &composed.plan, &registry);
        for change in diff.changes() {
            if change.id.is_reserved() && !fixture.is_scope_node(&change.id) {
                prop_assert_eq!(change.id.to_string(), "metrics_enrich_host_tags");
                prop_assert_eq!(
                    &change.change,
                    &NodeChange::Changed { operator: false, params: false, provenance: false, edges: true }
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
        // Nodes that are always refused: one with the ID of a reserved node outside the scope, and one that
        // aggregates, which no node of the scope does, so it has the artifact's provenance.
        let reserved_outside = artifact.nodes.iter().any(|node| {
            let reserved = NodeId::reserved(node.id.to_local_id());
            fixture.plan.node(&reserved).is_some() && !fixture.is_scope_node(&reserved)
        });
        let aggregates = artifact.nodes.iter().any(|node| node.op.as_str() == names::AGGREGATE);
        match compose(&fixture.plan, &received, &approval(4), &fixture.catalog, &registry) {
            Err(error) => {
                prop_assert!(explains(&error, artifact, &fixture).is_some(), "{:?}: {}", edits, error);
                if reserved_outside {
                    prop_assert!(
                        matches!(error, ComposeError::ReservedOutsideScope { .. } | ComposeError::Malformed { .. }),
                        "{:?}: {}",
                        edits,
                        error
                    );
                }
            }
            Ok(composed) => {
                prop_assert!(!reserved_outside && !aggregates, "{:?}", edits);
                // A node or an edge removed leaves a port without its edge, which no other edit restores.
                if removed {
                    prop_assert!(!composed.report.is_valid(), "{:?}", edits);
                }
                // Every artifact node became one plan node, with the provenance its correspondence says.
                prop_assert_eq!(composed.correspondence.iter().count(), received.artifact.nodes.len());
                for (_, correspondence) in composed.correspondence.iter() {
                    let node = composed.plan.node(&correspondence.node).unwrap();
                    match correspondence.kind {
                        CorrespondenceKind::Unmodified => {
                            prop_assert_eq!(Some(node), fixture.plan.node(&correspondence.node))
                        }
                        CorrespondenceKind::Modified | CorrespondenceKind::Added => {
                            prop_assert!(matches!(node.provenance, Some(Provenance::Artifact(_))))
                        }
                    }
                }
                // Every node outside the scope is kept as it is.
                for node in &fixture.plan.nodes {
                    if !fixture.is_scope_node(&node.id) {
                        prop_assert_eq!(composed.plan.node(&node.id), Some(node));
                    }
                }
                // Artifacts reference native outputs only: every emit names an output the base records with the
                // fingerprint the host declares it with.
                for node in artifact.nodes.iter().filter(|node| node.op.as_str() == names::EMIT) {
                    let Some(output) = emitted_output(&node.params) else { continue };
                    let declared = fixture.catalog.output(&output).map(|declared| declared.fingerprint);
                    prop_assert!(declared.is_some(), "{:?}: `{}` emits to `{}`", edits, node.id, output);
                    prop_assert_eq!(artifact.base.outputs.get(&output).copied(), declared, "{:?}", edits);
                }
            }
        }
    }
}
