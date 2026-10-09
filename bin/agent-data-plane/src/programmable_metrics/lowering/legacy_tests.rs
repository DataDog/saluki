//! Structural comparison of lowered plans with the legacy topology blueprint.
//!
//! For every configuration of the matrix, the legacy topology is built by `run.rs`'s own `create_topology`, with the
//! local configuration as the authority and an environment provider that needs no Core Agent, and its blueprint
//! snapshot is compared with the lowered plan through the mapping below.
//!
//! # Mapping
//!
//! | Lowered node | Legacy counterpart |
//! |---|---|
//! | `source` nodes | the source component (`trace_sampler` is a transform); port `metrics` is its `metrics` output |
//! | `emit` nodes | the component of the output they name: its encoder, or the tap `dsd_client_telemetry_out` |
//! | `opaque_host` nodes (the gateways) | the transform their `component` parameter names |
//! | `dsd_enrich` (mapper kernel) | the `dsd_enrich` chain |
//! | `dsd_prefix_filter` (prefix), `dsd_prefix_filter_blocklist` (split) and its `drop` | `dsd_prefix_filter` |
//! | `dsd_no_agg_split` (timestamp split) | `dsd_no_agg_split`; port `passthrough` is its `passthrough` output |
//! | `dsd_tag_filterlist` (tag projection) | `dsd_tag_filterlist` |
//! | `dsd_agg` (aggregation, opaque) | `dsd_agg` |
//! | `dsd_post_agg_filter_in` (merge) | fan-in at the input of `dsd_post_agg_filter` |
//! | `dsd_post_agg_filter` (split) with its `drop` | `dsd_post_agg_filter` |
//! | `dsd_post_agg_filter_fanout` (tee) | fan-out of `dsd_post_agg_filter`'s output |
//! | `metrics_enrich_in` (merge) | fan-in at the input of `metrics_enrich` |
//! | `metrics_enrich` (host enrichment) and `metrics_enrich_host_tags` (host tags) | the `metrics_enrich` chain |
//! | `metrics_policy` (tee) | fan-out of `metrics_enrich`'s output |
//!
//! Emits and opaque hosts are mapped by their parameters, not their IDs, so an emit that delivers to another output
//! than its branch's, or an opaque host standing for another component, is a structural difference.
//!
//! Every other port of a node mapped to a component leaves through the component's default output. Each plan edge
//! between nodes of different components, followed through merges and tees, is a legacy edge from a component output to
//! a component; edges between nodes of one component are inside it.
//!
//! The comparison then checks, both ways: every legacy edge leaving a component output the plan maps is a plan edge,
//! except the host taps on `dsd_in.metrics` (`dsd_stats_out`, and `dsd_debug_log_out` with DogStatsD logging enabled);
//! every legacy edge entering a component the plan sends metrics to comes from the plan; every mapped component exists
//! with its type; and every output's encoder feeds the output's forwarder.
//!
//! # What the comparison does not cover
//!
//! A blueprint snapshot shows components and edges, not what runs inside a chained transform. So the nodes inside the
//! `dsd_enrich` and `metrics_enrich` chains, the mapper and host tags, are outside the comparison: whether host tags
//! are lowered is pinned by the presence table of `branches_exist_only_when_native_wiring_builds_them` (from the same
//! gate `run.rs` uses), and what every node's parameters are by the lowering tests' fixtures.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use saluki_core::{accounting::ComponentRegistry, components::ComponentType, topology::BlueprintSnapshot};
use saluki_metric_plan::{
    ir::{Node, Plan, PortRef},
    operators::names,
};
use serde_json::{json, Value};

use super::{
    matrix::{matrix_entry, MATRIX},
    Lowering,
};
use crate::{
    cli::run::create_topology,
    config::remote_agent_client_configuration,
    programmable_metrics::{
        outputs::{NativeOutputs, OutputKind},
        test_support::{configuration, configuration_system, environment_provider, facts},
    },
};

/// What a lowered node is in the legacy topology.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Legacy {
    /// The node runs inside the component of that ID and type.
    Component(String, ComponentType),

    /// The node is the fan-in at the input of the component its output feeds.
    FanIn,

    /// The node is the fan-out of the output that feeds it.
    FanOut,
}

/// Returns the legacy counterpart of a lowered node, by the mapping table, independently of the node's provenance.
///
/// An `emit` maps to the component of the output it names, among `outputs`, and an opaque host to the component its
/// parameters name.
fn legacy(node: &Node, outputs: &NativeOutputs) -> Legacy {
    use ComponentType::{Destination, Encoder, Source, Transform};

    let id = node.id.to_string();
    let op = node.op.as_str();
    let component = |id: &str, ty| Legacy::Component(id.to_string(), ty);
    let param = |name: &str| {
        node.params
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("`{}` ({}) has a `{}` parameter", id, op, name))
    };
    match op {
        names::MERGE => return Legacy::FanIn,
        names::TEE => return Legacy::FanOut,
        names::SOURCE if id == "trace_sampler" => return component(&id, Transform),
        names::SOURCE => return component(&id, Source),
        names::EMIT => {
            let output = outputs
                .get(param("output"))
                .unwrap_or_else(|| panic!("`{}` emits to an output the host declares", id));
            let ty = match output.kind {
                OutputKind::ClientTelemetry => Destination,
                _ => Encoder,
            };
            return component(&output.component, ty);
        }
        names::OPAQUE_HOST => return component(param("component"), Transform),
        _ => {}
    }
    match id.as_str() {
        "dsd_enrich" | "dsd_no_agg_split" | "dsd_tag_filterlist" | "dsd_agg" => component(&id, Transform),
        "dsd_prefix_filter" | "dsd_prefix_filter_blocklist" | "dsd_prefix_filter_blocklist_true" => {
            component("dsd_prefix_filter", Transform)
        }
        "dsd_post_agg_filter" | "dsd_post_agg_filter_true" => component("dsd_post_agg_filter", Transform),
        "metrics_enrich" | "metrics_enrich_host_tags" => component("metrics_enrich", Transform),
        _ => panic!("node `{}` ({}) is not in the mapping table", id, op),
    }
}

/// The physical structure a plan describes, by the mapping table.
struct Physical {
    /// Every legacy edge, as `output -> component`.
    edges: BTreeSet<(String, String)>,

    /// Every component output a plan port maps to.
    outputs: BTreeSet<String>,

    /// Every component the plan sends metrics into.
    inputs: BTreeSet<String>,

    /// Every component a node maps to, with its type.
    components: BTreeMap<String, ComponentType>,
}

impl Physical {
    fn of(plan: &Plan, outputs: &NativeOutputs, snapshot: &BlueprintSnapshot) -> Self {
        let node = |id: &saluki_metric_plan::ir::NodeId| plan.node(id).expect("edges reach nodes");
        let targets = |from: &PortRef| -> Vec<&PortRef> {
            plan.edges
                .iter()
                .filter(|edge| &edge.from == from)
                .map(|edge| &edge.to)
                .collect()
        };
        // The components a port's records reach, through merges and tees.
        fn reach<'p>(
            plan: &'p Plan, outputs: &NativeOutputs, to: &PortRef, targets: &dyn Fn(&PortRef) -> Vec<&'p PortRef>,
            found: &mut Vec<String>,
        ) {
            let node = plan.node(&to.node).expect("edges reach nodes");
            match legacy(node, outputs) {
                Legacy::Component(component, _) => found.push(component),
                Legacy::FanIn => {
                    let out = PortRef::new(node.id.clone(), "out".parse().unwrap());
                    targets(&out)
                        .into_iter()
                        .for_each(|to| reach(plan, outputs, to, targets, found));
                }
                Legacy::FanOut => {
                    for edge in plan.edges.iter().filter(|edge| edge.from.node == node.id) {
                        reach(plan, outputs, &edge.to, targets, found);
                    }
                }
            }
        }

        let mut physical = Physical {
            edges: BTreeSet::new(),
            outputs: BTreeSet::new(),
            inputs: BTreeSet::new(),
            components: BTreeMap::new(),
        };
        for edge in &plan.edges {
            let from = node(&edge.from.node);
            let Legacy::Component(component, ty) = legacy(from, outputs) else {
                continue;
            };
            physical.components.insert(component.clone(), ty);
            // A port leaves through the component's output of that name, or through its default output.
            let named = format!("{}.{}", component, edge.from.port);
            let output = match snapshot.component(&component) {
                Some(snapshot) if snapshot.outputs().iter().any(|output| output.id().to_string() == named) => named,
                _ => component.clone(),
            };
            let mut found = Vec::new();
            reach(plan, outputs, &edge.to, &targets, &mut found);
            for target in found.into_iter().filter(|target| *target != component) {
                physical.edges.insert((output.clone(), target.clone()));
                physical.outputs.insert(output.clone());
                physical.inputs.insert(target);
            }
        }
        for node in &plan.nodes {
            if let Legacy::Component(component, ty) = legacy(node, outputs) {
                physical.components.insert(component, ty);
            }
        }
        physical
    }
}

/// Builds the legacy topology for the configuration `yaml` and lowers the same configuration, returning the blueprint
/// snapshot, the plan, and the outputs.
async fn legacy_and_lowered(yaml: &str) -> (BlueprintSnapshot, Plan, NativeOutputs, bool) {
    let system = configuration_system(yaml).await;
    let config = system.config();
    let env = environment_provider(&config).await;
    let client = (!config.control.standalone_mode).then(|| remote_agent_client_configuration(&config).unwrap());
    let registry = ComponentRegistry::default();
    let (blueprint, _) = create_topology(&system, client.as_ref(), &env, &registry)
        .await
        .expect("the legacy topology builds");
    let snapshot = blueprint.snapshot().expect("the blueprint is not initialized");
    let lowering = Lowering::new(Arc::clone(&config), facts(&config)).unwrap().unwrap();
    let lowered = lowering.lower(&config).unwrap();
    let debug_log = config.domains.dogstatsd.debug_log.logging_enabled;
    (snapshot, lowered.plan, lowering.outputs().clone(), debug_log)
}

/// Returns the legacy edges as `output -> component`.
fn edges(snapshot: &BlueprintSnapshot) -> BTreeSet<(String, String)> {
    snapshot
        .edges()
        .iter()
        .map(|edge| (edge.from().to_string(), edge.to().to_string()))
        .collect()
}

#[tokio::test]
async fn lowered_plans_match_the_legacy_blueprint() {
    for (name, yaml) in MATRIX {
        let (snapshot, plan, outputs, debug_log) = legacy_and_lowered(yaml).await;
        let physical = Physical::of(&plan, &outputs, &snapshot);
        let legacy = edges(&snapshot);
        assert!(!physical.edges.is_empty(), "{}", name);

        // Every mapped component exists, with its type.
        for (component, ty) in &physical.components {
            let found = snapshot
                .component(component)
                .unwrap_or_else(|| panic!("{}: no legacy component `{}`", name, component));
            assert_eq!(found.component_type(), *ty, "{}: `{}`", name, component);
        }

        // The legacy edges leaving the mapped outputs are the plan's, and the host taps on the DogStatsD source.
        let mut expected = physical.edges.clone();
        if physical.outputs.contains("dsd_in.metrics") {
            expected.insert(("dsd_in.metrics".to_string(), "dsd_stats_out".to_string()));
            if debug_log {
                expected.insert(("dsd_in.metrics".to_string(), "dsd_debug_log_out".to_string()));
            }
        }
        let leaving = legacy
            .iter()
            .filter(|(from, _)| physical.outputs.contains(from))
            .cloned()
            .collect::<BTreeSet<_>>();
        assert_eq!(leaving, expected, "{}: edges leaving the plan's outputs", name);

        // Nothing outside the plan feeds a component the plan sends metrics into.
        let entering = legacy
            .iter()
            .filter(|(_, to)| physical.inputs.contains(to))
            .cloned()
            .collect::<BTreeSet<_>>();
        assert_eq!(
            entering, physical.edges,
            "{}: edges entering the plan's components",
            name
        );

        // Every output's encoder feeds its forwarder; the client telemetry tap is a destination with no forwarder.
        for output in outputs.iter() {
            match output.forwarder {
                Some(forwarder) => assert!(
                    legacy.contains(&(output.component.clone(), forwarder.to_string())),
                    "{}: `{}` feeds `{}`",
                    name,
                    output.component,
                    forwarder
                ),
                None => assert_eq!(
                    snapshot.component(&output.component).map(|c| c.component_type()),
                    Some(ComponentType::Destination),
                    "{}: `{}`",
                    name,
                    output.component
                ),
            }
        }
    }
}

/// The physical structure of two configurations, written by hand from `run.rs`, so that the comparison above cannot
/// pass by mapping both sides to nothing.
#[tokio::test]
async fn the_comparison_finds_the_native_edges() {
    let pairs = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(from, to)| (from.to_string(), to.to_string()))
            .collect::<BTreeSet<_>>()
    };

    let (snapshot, plan, outputs, _) = legacy_and_lowered("").await;
    assert_eq!(
        Physical::of(&plan, &outputs, &snapshot).edges,
        pairs(&[
            ("dsd_agg", "dsd_post_agg_filter"),
            ("dsd_enrich", "dsd_prefix_filter"),
            ("dsd_in.metrics", "dsd_enrich"),
            ("dsd_no_agg_split", "dsd_tag_filterlist"),
            ("dsd_no_agg_split.passthrough", "dsd_post_agg_filter"),
            ("dsd_post_agg_filter", "dsd_client_telemetry_out"),
            ("dsd_post_agg_filter", "metrics_enrich"),
            ("dsd_prefix_filter", "dsd_no_agg_split"),
            ("dsd_tag_filterlist", "dsd_agg"),
            ("metrics_enrich", "dd_metrics_encode"),
        ])
    );

    let (snapshot, plan, outputs, _) = legacy_and_lowered(matrix_entry("every branch")).await;
    let physical = Physical::of(&plan, &outputs, &snapshot);
    for edge in [
        ("checks_ipc_in.metrics", "metrics_enrich"),
        ("liveness_in.metrics", "metrics_enrich"),
        ("otlp_in.metrics", "metrics_enrich"),
        ("trace_sampler.metrics", "metrics_enrich"),
        ("metrics_enrich", "metrics_routing_filter_0"),
        ("metrics_enrich", "metrics_routing_filter_1"),
        ("metrics_enrich", "mrf_metrics_gateway"),
        ("metrics_enrich", "af_metrics_gateway"),
        ("metrics_routing_filter_0", "metrics_routing_encode_0"),
        ("metrics_routing_filter_1", "metrics_routing_encode_1"),
        ("mrf_metrics_gateway", "mrf_metrics_encode"),
        ("af_metrics_gateway", "af_metrics_encode"),
    ] {
        assert!(
            physical.edges.contains(&(edge.0.to_string(), edge.1.to_string())),
            "{:?}",
            edge
        );
    }
    assert_eq!(physical.edges.len(), 22);
}

#[tokio::test]
async fn every_lowered_node_has_a_legacy_counterpart_by_the_mapping_table() {
    // The table, not provenance, decides; this pins the rows that differ from a node's own ID. Emits and opaque hosts
    // map by their parameters, whatever their IDs.
    let config = configuration(matrix_entry("every branch")).await;
    let lowering = Lowering::new(
        Arc::new(config),
        facts(&configuration(matrix_entry("every branch")).await),
    )
    .unwrap()
    .unwrap();
    let outputs = lowering.outputs();
    let node = |id: &str, op: &str, params: Value| Node {
        id: id.parse().unwrap(),
        op: op.parse().unwrap(),
        params: serde_json::from_value(params).unwrap(),
        provenance: None,
    };
    let component = |id: &str, ty| Legacy::Component(id.to_string(), ty);
    let cases = [
        (
            node("dsd_prefix_filter_blocklist", names::SPLIT, json!({})),
            component("dsd_prefix_filter", ComponentType::Transform),
        ),
        (node("dsd_post_agg_filter_in", names::MERGE, json!({})), Legacy::FanIn),
        (
            node("dsd_post_agg_filter_fanout", names::TEE, json!({})),
            Legacy::FanOut,
        ),
        (
            node(
                "metrics_routing_filter_0_out",
                names::EMIT,
                json!({"output": "metrics_routing_encode_0"}),
            ),
            component("metrics_routing_encode_0", ComponentType::Encoder),
        ),
        (
            node(
                "metrics_routing_filter_0_out",
                names::EMIT,
                json!({"output": "metrics_routing_encode_1"}),
            ),
            component("metrics_routing_encode_1", ComponentType::Encoder),
        ),
        (
            node(
                "dsd_post_agg_filter_fanout_0",
                names::EMIT,
                json!({"output": "dsd_client_telemetry"}),
            ),
            component("dsd_client_telemetry_out", ComponentType::Destination),
        ),
        (
            node("metrics_policy_0", names::EMIT, json!({"output": "mrf_metrics_encode"})),
            component("mrf_metrics_encode", ComponentType::Encoder),
        ),
        (
            node(
                "af_metrics_gateway",
                names::OPAQUE_HOST,
                json!({"component": "mrf_metrics_gateway"}),
            ),
            component("mrf_metrics_gateway", ComponentType::Transform),
        ),
        (
            node("trace_sampler", names::SOURCE, json!({})),
            component("trace_sampler", ComponentType::Transform),
        ),
    ];
    for (node, expected) in cases {
        assert_eq!(legacy(&node, outputs), expected, "{}", node.id);
    }
}
