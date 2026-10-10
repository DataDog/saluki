//! The default program, exported as artifacts, printed, and parsed back: its policy scope, and its DogStatsD chain.

use std::collections::{BTreeMap, BTreeSet};

use saluki_metric_plan::{
    canonical::{strip_resolved_values, ResolvedValues},
    ir::{
        ArtifactBase, ArtifactEdge, ArtifactNode, ArtifactNodeId, ArtifactPortRef, InputName, NodeId, OutputId, Plan,
        PolicyArtifact, PortRef, ReceivedArtifact,
    },
    testing::{default_program_catalog, with_opaque_gateways, DEFAULT_PROGRAM_JSON, NO_AGGREGATION_PIPELINE_OFF_JSON},
};

use super::*;

fn default_program() -> Plan {
    Plan::from_json(DEFAULT_PROGRAM_JSON).expect("the default program fixture reads")
}

/// Exports the `post_enrich` scope of a plan as the host does, as an artifact.
fn host_export(plan: &Plan) -> PolicyArtifact {
    saluki_metric_plan::export::export(
        plan,
        &"post_enrich".parse().unwrap(),
        &default_program_catalog(),
        &BTreeMap::new(),
    )
    .expect("the default program's policy scope exports")
    .artifact
}

/// Exports the `post_enrich` scope of the default program as an artifact.
fn export_post_enrich() -> PolicyArtifact {
    host_export(&default_program())
}

/// Exports the region of the default program's DogStatsD pipeline between its source and the shared enrichment, the
/// scope the design's later stages add, as an artifact. The edge that leaves the region is left out, since artifacts
/// cannot name exits yet.
fn export_dogstatsd_chain() -> PolicyArtifact {
    let plan = default_program();
    let source = plan
        .edges
        .iter()
        .find(|edge| edge.from.node.local().as_str() == "dsd_in")
        .expect("the DogStatsD source has an edge")
        .from
        .clone();
    let region = plan
        .nodes
        .iter()
        .map(|node| node.id.clone())
        .filter(|id| id.local().as_str().starts_with("dsd_") && id.local().as_str() != "dsd_in")
        .collect::<BTreeSet<_>>();
    export_region(&plan, "dogstatsd_pre_aggregation", &source, &region)
}

/// Exports a region of the default program that is not one of its declared scopes, which the host does not export, as
/// an artifact, the way the host exports scopes: the region's nodes, and the edges between them, in the plan's order,
/// without provenance or resolved values; the edge from `entry` as leaving the scope input `metrics`; and a base of the
/// program's hash and the fingerprints of the outputs the region emits to.
fn export_region(plan: &Plan, scope: &str, entry: &PortRef, region: &BTreeSet<NodeId>) -> PolicyArtifact {
    let local = |id: &NodeId| ArtifactNodeId::new(id.local().as_str()).unwrap();
    let nodes = plan
        .nodes
        .iter()
        .filter(|node| region.contains(&node.id))
        .map(|node| {
            let mut params = node.params.clone();
            let names = params.iter().map(|(name, _)| name.clone()).collect::<Vec<_>>();
            for name in names {
                let mut value = params.get(name.as_str()).cloned().unwrap();
                strip_resolved_values(&mut value);
                params.insert(name, value);
            }
            ArtifactNode {
                id: local(&node.id),
                op: node.op.clone(),
                params,
            }
        })
        .collect::<Vec<_>>();
    let edges = plan
        .edges
        .iter()
        .filter(|edge| edge.from == *entry || region.contains(&edge.from.node))
        .filter(|edge| region.contains(&edge.to.node))
        .map(|edge| ArtifactEdge {
            from: if edge.from == *entry {
                ArtifactPortRef::Input(InputName::new("metrics").unwrap())
            } else {
                ArtifactPortRef::Node {
                    node: local(&edge.from.node),
                    port: edge.from.port.clone(),
                }
            },
            to: ArtifactPortRef::Node {
                node: local(&edge.to.node),
                port: edge.to.port.clone(),
            },
        })
        .collect::<Vec<_>>();
    let outputs = nodes
        .iter()
        .filter(|node| node.op.as_str() == "emit")
        .map(|node| {
            let output = OutputId::new(node.params.get("output").unwrap().as_str().unwrap()).unwrap();
            let fingerprint = plan.output(&output).unwrap().fingerprint;
            (output, fingerprint)
        })
        .collect::<BTreeMap<_, _>>();

    PolicyArtifact {
        ir_version: plan.ir_version,
        scope: scope.parse().unwrap(),
        base: ArtifactBase {
            program_hash: plan.hash(ResolvedValues::Exclude).unwrap(),
            outputs,
        },
        nodes,
        edges,
        blobs: Default::default(),
    }
}

/// The export, printed, written by hand.
fn expected_text(program_hash: &str) -> String {
    format!(
        r#"policy post_enrich {{
  base "{program_hash}"
  outputs {{
    af_metrics_encode = "sha256:6666666666666666666666666666666666666666666666666666666666666666",
    dd_metrics_encode = "sha256:2222222222222222222222222222222222222222222222222222222222222222",
    metrics_routing_encode_0 = "sha256:3333333333333333333333333333333333333333333333333333333333333333",
    metrics_routing_encode_1 = "sha256:4444444444444444444444444444444444444444444444444444444444444444",
    mrf_metrics_encode = "sha256:5555555555555555555555555555555555555555555555555555555555555555",
  }}

  input.metrics -> metrics_policy

  node metrics_policy = tee {{
    -> emit dd_metrics_encode
    -> metrics_routing_filter_0
    -> metrics_routing_filter_1
    -> mrf_metrics_gateway
    -> af_metrics_gateway
  }}

  node metrics_routing_filter_0 = split(
    intake_name matches(values: config("metrics_endpoint_routing.group.0123456789abcdef.names"), prefix: false)
    or intake_name matches(values: config("metrics_endpoint_routing.group.0123456789abcdef.prefixes"), prefix: true)
  ) {{
    true  -> emit metrics_routing_encode_0
    false -> drop
  }}

  node metrics_routing_filter_1 = split(
    intake_name matches(values: config("metrics_endpoint_routing.group.fedcba9876543210.names"), prefix: false)
    or intake_name matches(values: config("metrics_endpoint_routing.group.fedcba9876543210.prefixes"), prefix: true)
  ) {{
    true  -> emit metrics_routing_encode_1
    false -> drop
  }}

  node mrf_metrics_gateway = split(
    flag(config("multi_region_failover.enabled"))
    and flag(config("multi_region_failover.metric_mirroring.enabled"))
    and (
      is_empty(config("multi_region_failover.metric_mirroring.allowlist"))
      or name matches(values: config("multi_region_failover.metric_mirroring.allowlist"), prefix: false)
    )
  ) {{
    true  -> emit mrf_metrics_encode
    false -> drop
  }}

  node af_metrics_gateway = split(
    kind in [counter, rate, gauge, set] and name matches(values: config("autoscaling_failover.metrics"), prefix: false)
  ) {{
    true  -> emit af_metrics_encode
    false -> drop
  }}
}}
"#
    )
}

#[test]
fn the_default_programs_policy_scope_prints_and_parses_back() {
    let export = export_post_enrich();
    // The export is a valid artifact document, with the scope's 14 nodes and 14 edges in lowering's order.
    let document = serde_json::to_value(&export).unwrap();
    assert_eq!(PolicyArtifact::from_value(document).unwrap(), export);
    assert_eq!((export.nodes.len(), export.edges.len()), (14, 14));

    let expected = expected_text(&export.base.program_hash.to_string());
    let printed = print_default(&export);
    assert_eq!(printed, expected);
    assert!(printed.lines().all(|line| line.chars().count() <= 120));

    // Lowering writes nodes and edges in the order parsing does, so the export comes back exactly.
    let parsed = parse_ok(&expected);
    assert_eq!(parsed.artifact, export);

    // Every node and edge has a span.
    for node in &export.nodes {
        assert!(parsed.spans.node(node.id.as_str()).is_some(), "{}", node.id);
    }
    assert!((0..export.edges.len()).all(|i| parsed.spans.edge(i).is_some()));
    assert_eq!(
        text(&expected, parsed.spans.node("mrf_metrics_gateway_true").unwrap()),
        "emit mrf_metrics_encode"
    );
    let flag = parsed.spans.param("mrf_metrics_gateway", "predicate").unwrap();
    assert!(text(&expected, flag).starts_with("flag(config(\"multi_region_failover.enabled\"))\n    and"));
}

#[test]
fn every_default_programs_export_prints_and_parses_back_to_the_same_document() {
    let plans = [
        ("default program", default_program()),
        (
            "no-aggregation pipeline off",
            Plan::from_json(NO_AGGREGATION_PIPELINE_OFF_JSON).unwrap(),
        ),
        (
            "default program, opaque gateways",
            Plan::from_value(with_opaque_gateways(DEFAULT_PROGRAM_JSON)).unwrap(),
        ),
        (
            "no-aggregation pipeline off, opaque gateways",
            Plan::from_value(with_opaque_gateways(NO_AGGREGATION_PIPELINE_OFF_JSON)).unwrap(),
        ),
    ];
    for (what, plan) in plans {
        let export = host_export(&plan);
        let printed = print_default(&export);
        let parsed = parse_ok(&printed);
        assert_eq!(parsed.artifact, export, "{}:\n{}", what, printed);

        // So the parsed document is the exported one, and has its hash.
        let hash = |artifact: &PolicyArtifact| {
            ReceivedArtifact::from_value(serde_json::to_value(artifact).unwrap())
                .unwrap()
                .hash
        };
        assert_eq!(hash(&parsed.artifact), hash(&export), "{}", what);
        assert_eq!(print_default(&parsed.artifact), printed, "{}", what);
    }
}

#[test]
fn the_default_programs_policy_scope_prints_without_fingerprints() {
    let export = export_post_enrich();
    let printed = print(
        &export,
        &PrintOptions {
            elide_fingerprints: true,
            ..PrintOptions::default()
        },
    );
    assert!(!printed.contains("outputs"));
    let mut without = export.clone();
    without.base.outputs.clear();
    assert_eq!(parse_ok(&printed).artifact, without);
}

/// Exports a declared scope of a plan as an artifact, the way the host exports scopes, through its exits: the nodes
/// reachable from the scope's inputs without entering one of its exits, in the plan's order, without provenance or
/// resolved values; the edges from its inputs as leaving `input.<name>`, the edges into its exits as entering
/// `exit.<name>`, and the edges between its nodes, in the plan's order; and a base of the program's hash and the
/// fingerprints of the outputs the region emits to.
fn export_scope(plan: &Plan, scope: &str) -> PolicyArtifact {
    let decl = &plan.scopes[scope];
    let exits = decl
        .exits
        .iter()
        .map(|(name, port)| (port, name))
        .collect::<BTreeMap<_, _>>();
    let mut region = BTreeSet::new();
    let mut pending = decl.inputs.values().cloned().collect::<Vec<_>>();
    while let Some(from) = pending.pop() {
        for edge in plan.edges.iter().filter(|edge| edge.from == from) {
            if exits.contains_key(&edge.to) || !region.insert(edge.to.node.clone()) {
                continue;
            }
            pending.extend(
                plan.edges
                    .iter()
                    .filter(|other| other.from.node == edge.to.node)
                    .map(|other| other.from.clone()),
            );
        }
    }
    let mut export = export_region(plan, scope, &decl.inputs[&InputName::new("metrics").unwrap()], &region);
    export.edges = plan
        .edges
        .iter()
        .filter_map(|edge| {
            let from = match decl.inputs.iter().find(|(_, port)| **port == edge.from) {
                Some((name, _)) => ArtifactPortRef::Input(name.clone()),
                None if region.contains(&edge.from.node) => ArtifactPortRef::Node {
                    node: ArtifactNodeId::new(edge.from.node.local().as_str()).unwrap(),
                    port: edge.from.port.clone(),
                },
                None => return None,
            };
            let to = match exits.get(&edge.to) {
                Some(name) => ArtifactPortRef::Exit((*name).clone()),
                None if region.contains(&edge.to.node) => ArtifactPortRef::Node {
                    node: ArtifactNodeId::new(edge.to.node.local().as_str()).unwrap(),
                    port: edge.to.port.clone(),
                },
                None => return None,
            };
            Some(ArtifactEdge { from, to })
        })
        .collect();
    export
}

#[test]
fn the_default_programs_pre_aggregation_scope_prints_and_parses_back_through_its_exits() {
    // The DogStatsD pre-aggregation scope ends where aggregation's output and the passthrough of timestamped points
    // join the post-aggregation filter: with the no-aggregation pipeline, by two exits; without it, by one.
    let cases = [
        (
            "default program",
            default_program(),
            (7, 9),
            &["    -> exit.aggregated\n  }\n", "    passthrough -> exit.passthrough\n"][..],
        ),
        (
            "no-aggregation pipeline off",
            Plan::from_json(NO_AGGREGATION_PIPELINE_OFF_JSON).unwrap(),
            (6, 7),
            &["    -> exit.aggregated\n  }\n"][..],
        ),
    ];
    for (what, plan, (nodes, edges), fragments) in cases {
        let export = export_scope(&plan, "dogstatsd_pre_aggregation");
        assert_eq!((export.nodes.len(), export.edges.len()), (nodes, edges), "{}", what);
        // The export is a valid artifact document.
        let document = serde_json::to_value(&export).unwrap();
        assert_eq!(PolicyArtifact::from_value(document).unwrap(), export, "{}", what);

        let printed = print_default(&export);
        assert!(
            printed.starts_with("policy dogstatsd_pre_aggregation {\n"),
            "{}",
            printed
        );
        assert!(printed.contains("\n  input.metrics -> dsd_enrich\n"), "{}", printed);
        for fragment in fragments {
            assert!(printed.contains(fragment), "{}: {:?} in\n{}", what, fragment, printed);
        }
        assert!(printed.lines().all(|line| line.chars().count() <= 120), "{}", printed);

        // Lowering writes the scope's nodes and edges in the order parsing does, its exits included, so it comes
        // back exactly, with its hash.
        let parsed = parse_ok(&printed);
        assert_eq!(parsed.artifact, export, "{}:\n{}", what, printed);
        assert_eq!(print_default(&parsed.artifact), printed, "{}", what);
    }
}

#[test]
fn the_default_programs_dogstatsd_chain_prints_within_the_line_width() {
    let export = export_dogstatsd_chain();
    assert_eq!((export.nodes.len(), export.edges.len()), (12, 13));
    let printed = print_default(&export);
    let long = printed
        .lines()
        .filter(|line| line.chars().count() > 120)
        .collect::<Vec<_>>();
    assert!(long.is_empty(), "lines over 120 columns: {:#?}", long);

    // Set matches whose bindings do not fit on the line put each argument on its own line.
    let blocklist = r#"
  node dsd_prefix_filter_blocklist = split(
    intake_name matches(
      values: config("dogstatsd.metric_filter.values"),
      prefix: config("dogstatsd.metric_filter.match_prefix"),
    )
  ) {
    true  -> drop
    false -> dsd_no_agg_split
  }
"#;
    let post_aggregation = r#"
  node dsd_post_agg_filter = split(
    kind in [counter, rate, gauge, set]
    and intake_name matches(
      values: config("dogstatsd.post_aggregate_filter.values"),
      prefix: config("dogstatsd.metric_filter.match_prefix"),
    )
  ) {
    true  -> drop
    false -> dsd_post_agg_filter_fanout
  }
"#;
    assert!(printed.contains(blocklist), "{}", printed);
    assert!(printed.contains(post_aggregation), "{}", printed);

    // Lowering writes this region in the order parsing does too, so it comes back exactly.
    assert_eq!(parse_ok(&printed).artifact, export);
}
