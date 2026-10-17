//! The default program, exported as artifacts, printed, and parsed back: its one scope, the whole metrics path after
//! its sources; the two scopes it declared through stage 9, the policy region and DogStatsD's pre-aggregation chain,
//! which end before the plan does; and its DogStatsD chain.

use std::collections::{BTreeMap, BTreeSet};

use saluki_metric_plan::{
    canonical::{strip_resolved_values, ResolvedValues},
    ir::{
        ArtifactBase, ArtifactEdge, ArtifactNode, ArtifactNodeId, ArtifactPortRef, InputName, NodeId, OutputId, Plan,
        PolicyArtifact, PortRef, ReceivedArtifact,
    },
    testing::{
        default_program_catalog, two_scope_catalog, with_opaque_gateways, with_two_scopes, DEFAULT_PROGRAM_JSON,
        NO_AGGREGATION_PIPELINE_OFF_JSON,
    },
};

use super::*;

fn default_program() -> Plan {
    Plan::from_json(DEFAULT_PROGRAM_JSON).expect("the default program fixture reads")
}

/// Returns `plan` with [the two scopes the default program declared through stage 9](with_two_scopes).
fn two_scopes(plan: &Plan) -> Plan {
    Plan::from_value(with_two_scopes(serde_json::to_value(plan).unwrap())).unwrap()
}

/// Exports the `post_enrich` scope, one of [the two scopes of stage 9](with_two_scopes), of a plan as the host
/// exported it, as an artifact.
fn host_export(plan: &Plan) -> PolicyArtifact {
    saluki_metric_plan::export::export(
        &two_scopes(plan),
        &"post_enrich".parse().unwrap(),
        &two_scope_catalog(),
        &BTreeMap::new(),
    )
    .expect("the default program's policy scope exports")
    .artifact
}

/// Exports the one scope of a plan, `metrics`, as the host does, as an artifact.
fn metrics_export(plan: &Plan) -> PolicyArtifact {
    saluki_metric_plan::export::export(
        plan,
        &"metrics".parse().unwrap(),
        &default_program_catalog(),
        &BTreeMap::new(),
    )
    .expect("the default program's scope exports")
    .artifact
}

/// Exports the `post_enrich` scope of the default program as an artifact.
fn export_post_enrich() -> PolicyArtifact {
    host_export(&default_program())
}

/// Exports the region of the default program's DogStatsD pipeline between its source and the shared enrichment, the
/// scope the design's later stages add, as an artifact, with one exit, `enrichment`, for the edge that leaves the region.
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
    let mut export = export_region(&plan, "dogstatsd_pre_aggregation", &source, &region);
    // The edge that leaves the region leaves the scope, by an exit.
    let leaving = plan
        .edges
        .iter()
        .find(|edge| region.contains(&edge.from.node) && !region.contains(&edge.to.node))
        .expect("an edge leaves the DogStatsD chain");
    export.edges.push(ArtifactEdge {
        from: ArtifactPortRef::Node {
            node: ArtifactNodeId::new(leaving.from.node.local().as_str()).unwrap(),
            port: leaving.from.port.clone(),
        },
        to: ArtifactPortRef::Exit("enrichment".parse().unwrap()),
    });
    export
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

/// The export, printed, written by hand. `policy` is the ID of the `tee` that copies the scope's input to its routes,
/// which is not the ID derived for a copy of the input, `input_metrics_fanout`, so it is written in the core form.
fn expected_text(program_hash: &str, policy: &str) -> String {
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

  let {policy} = metrics | tee(ports: 5)

  {policy}.0 | emit output dd_metrics_encode

  let metrics_routing_filter_0 = {policy}.1
    | where intake_name in config.metrics_endpoint_routing.group.0123456789abcdef.names
        or intake_name starts_with config.metrics_endpoint_routing.group.0123456789abcdef.prefixes

  metrics_routing_filter_0 | emit output metrics_routing_encode_0

  let metrics_routing_filter_1 = {policy}.2
    | where intake_name in config.metrics_endpoint_routing.group.fedcba9876543210.names
        or intake_name starts_with config.metrics_endpoint_routing.group.fedcba9876543210.prefixes

  metrics_routing_filter_1 | emit output metrics_routing_encode_1

  let mrf_metrics_gateway = {policy}.3
    | where config.multi_region_failover.enabled
        and config.multi_region_failover.metric_mirroring.enabled
        and (
          is_empty(config.multi_region_failover.metric_mirroring.allowlist)
          or name in config.multi_region_failover.metric_mirroring.allowlist
        )

  mrf_metrics_gateway | emit output mrf_metrics_encode

  let af_metrics_gateway = {policy}.4
    | where kind in [counter, rate, gauge, set]
        and config.autoscaling_failover.enabled
        and name in config.autoscaling_failover.metrics

  af_metrics_gateway | emit output af_metrics_encode
}}
"#
    )
}

#[test]
fn the_default_programs_export_prints_with_a_let_for_each_named_stream_and_no_as() {
    // The normalized export of the one scope: every node whose ID is not the one its position derives ends a `let`
    // named for it, so no `as` is needed.
    let export = default_export();
    let printed = print_default(&export);
    assert!(
        printed.split_whitespace().all(|word| word != "as"),
        "no `as`:\n{}",
        printed
    );
    assert!(printed.lines().all(|line| line.chars().count() <= 120), "{}", printed);

    // The first line of each statement, in order.
    let statements = printed
        .lines()
        .filter(|line| line.starts_with("  ") && !line.starts_with("   "))
        .filter(|line| {
            !["  base ", "  outputs {", "  }"]
                .iter()
                .any(|header| line.starts_with(header))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        statements,
        [
            "  let dsd_enrich = dogstatsd",
            "  let dsd_prefix_filter_blocklist = dsd_enrich",
            "  let dsd_no_agg_split = dsd_prefix_filter_blocklist",
            "  let dsd_tag_filterlist = dsd_no_agg_split.default",
            "  let dsd_agg = dsd_tag_filterlist",
            "  let dsd_post_agg_filter = (dsd_agg, dsd_no_agg_split.passthrough)",
            "  dsd_post_agg_filter | emit output dsd_client_telemetry",
            "  let metrics = (dsd_post_agg_filter, otlp, checks, liveness, trace_sampler)",
            "  metrics | emit output dd_metrics_encode",
            "  let metrics_routing_filter_0 = metrics",
            "  metrics_routing_filter_0 | emit output metrics_routing_encode_0",
            "  let metrics_routing_filter_1 = metrics",
            "  metrics_routing_filter_1 | emit output metrics_routing_encode_1",
            "  let mrf_metrics_gateway = metrics",
            "  mrf_metrics_gateway | emit output mrf_metrics_encode",
            "  let af_metrics_gateway = metrics",
            "  af_metrics_gateway | emit output af_metrics_encode",
        ],
        "{}",
        printed
    );
    for fragment in [
        "\n    | namespace_prefix(\n",
        "\n    | where not intake_name matches(\n",
        "\n    | timestamp_split(config.dogstatsd.aggregation.window_duration_seconds)\n",
        "\n    | host_enrichment\n    | host_tags(config.host.tags, deadline: config.host.tags_deadline)\n",
    ] {
        assert!(printed.contains(fragment), "{:?} in\n{}", fragment, printed);
    }

    // It parses back to the same program, and prints the same again.
    let parsed = parse_ok(&printed).artifact;
    assert_same_program(&parsed, &export, &printed);
    assert_eq!(print_default(&parsed), printed);
}

#[test]
fn the_default_programs_policy_scope_prints_and_parses_back() {
    let export = export_post_enrich();
    // The export is a valid artifact document, with the scope's 14 nodes and 14 edges in lowering's order.
    let document = serde_json::to_value(&export).unwrap();
    assert_eq!(PolicyArtifact::from_value(document).unwrap(), export);
    assert_eq!((export.nodes.len(), export.edges.len()), (14, 14));

    let policy = export
        .nodes
        .iter()
        .find(|node| node.op.as_str() == "tee")
        .expect("the routes are copies of the input")
        .id
        .to_string();
    let expected = expected_text(&export.base.program_hash.to_string(), &policy);
    let printed = print_default(&export);
    assert_eq!(printed, expected);
    // Every line fits the width.
    assert!(printed.lines().all(|line| line.chars().count() <= 120), "{}", printed);

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
        "emit output mrf_metrics_encode"
    );
    let flag = parsed.spans.param("mrf_metrics_gateway", "predicate").unwrap();
    assert!(text(&expected, flag).starts_with("config.multi_region_failover.enabled\n        and"));
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
    // The one scope, and the policy scope of stage 9. The one scope's export is in normal form, with every default
    // written, and lists its nodes and edges in the plan's order. Its text leaves out the defaults and parses in
    // canonical order, so it parses back to the same program once normalized and in canonical order, which composes to
    // the default program, with its hash, since composition normalizes an artifact and puts its nodes and edges in the
    // default program's order. The policy scope of stage 9 has no defaults to leave out and is already in canonical
    // order, so its export comes back exactly, with its hash.
    for (what, plan) in plans {
        for (export, exact) in [(metrics_export(&plan), false), (host_export(&plan), true)] {
            let printed = print_default(&export);
            let parsed = parse_ok(&printed);
            assert_same_program(&parsed.artifact, &export, &format!("{}:\n{}", what, printed));
            if exact {
                assert_eq!(parsed.artifact, export, "{}:\n{}", what, printed);
                let hash = |artifact: &PolicyArtifact| {
                    ReceivedArtifact::from_value(serde_json::to_value(artifact).unwrap())
                        .unwrap()
                        .hash
                };
                assert_eq!(hash(&parsed.artifact), hash(&export), "{}", what);
            } else if !what.ends_with("opaque gateways") {
                // (A scope that holds host components, such as opaque gateways, cannot be replaced at all: a program
                // cannot hold them.)
                assert_eq!(
                    composed_hash(&plan, &parsed.artifact),
                    plan.hash(ResolvedValues::Include).unwrap(),
                    "{}",
                    what
                );
            }
            assert_eq!(print_default(&parsed.artifact), printed, "{}", what);
        }
    }
}

/// Returns the hash of the plan `artifact` composes to in `plan`, with the fixtures' catalog: an unmodified export's
/// nodes keep the default program's resolved values, so the catalog needs none.
fn composed_hash(plan: &Plan, artifact: &PolicyArtifact) -> saluki_metric_plan::canonical::PlanHash {
    use saluki_metric_plan::{
        compose::{compose, receive_artifact, Approval},
        operators::OperatorRegistry,
    };

    let received = receive_artifact(serde_json::to_value(artifact).unwrap()).unwrap();
    let approval = Approval {
        revision: 1,
        approved_against: None,
        active_plan_hash: None,
        in_effect: false,
    };
    let composed = compose(
        plan,
        &received,
        &approval,
        &default_program_catalog(),
        &OperatorRegistry::native(),
    )
    .unwrap();
    assert!(composed.report.findings().is_empty(), "{}", composed.report);
    composed.plan.hash(ResolvedValues::Include).unwrap()
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
            two_scopes(&default_program()),
            (7, 9),
            &[
                // Each node whose ID is not derived ends a `let` named for it. In normal form, the prefix filter's
                // blocklist is a `where`.
                "\n  let dsd_enrich = metrics\n    | dogstatsd_mapper(",
                "\n  dsd_agg | exit aggregated\n",
                "\n  dsd_no_agg_split.passthrough | exit passthrough\n",
            ][..],
        ),
        (
            "no-aggregation pipeline off",
            two_scopes(&Plan::from_json(NO_AGGREGATION_PIPELINE_OFF_JSON).unwrap()),
            (6, 7),
            &["\n  dsd_agg | exit aggregated\n"][..],
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
        assert!(
            printed.contains("\n    | where not intake_name matches("),
            "{}",
            printed
        );
        for fragment in fragments {
            assert!(printed.contains(fragment), "{}: {:?} in\n{}", what, fragment, printed);
        }
        assert!(printed.lines().all(|line| line.chars().count() <= 120), "{}", printed);

        // It comes back as the same program, its exits included.
        let parsed = parse_ok(&printed);
        assert_same_program(&parsed.artifact, &export, &format!("{}:\n{}", what, printed));
        assert_eq!(print_default(&parsed.artifact), printed, "{}", what);
    }
}

#[test]
fn the_default_programs_one_scope_prints_from_its_sources_through_aggregation_and_enrichment_to_its_policy() {
    // (plan, how its DogStatsD chain starts and reaches the post-aggregation filter)
    let cases = [
        (
            default_program(),
            &[
                "\n  let dsd_enrich = dogstatsd\n",
                "\n  let dsd_post_agg_filter = (dsd_agg, dsd_no_agg_split.passthrough)\n    | where not (\n",
            ][..],
        ),
        (
            Plan::from_json(NO_AGGREGATION_PIPELINE_OFF_JSON).unwrap(),
            &[
                "\n  let dsd_enrich = dogstatsd\n",
                "\n  let dsd_tag_filterlist = dsd_prefix_filter_blocklist\n",
                "\n  let dsd_post_agg_filter = dsd_agg\n    | where not (\n",
            ][..],
        ),
    ];
    for (plan, chain) in cases {
        let export = metrics_export(&plan);
        // Every node but the five sources, and every edge.
        assert_eq!(export.nodes.len(), plan.nodes.len() - 5);
        assert_eq!(export.edges.len(), plan.edges.len());
        let printed = print_default(&export);
        assert!(printed.starts_with("policy metrics {\n"), "{}", printed);
        assert!(printed.lines().all(|line| line.chars().count() <= 120), "{}", printed);

        // The statements follow the path: DogStatsD's chain from its input through aggregation and the
        // post-aggregation filter, whose stream is reused by the client telemetry and by enrichment, which the other
        // sources complete, then the routes. (The IDs of enrichment and of the `tee` before the routes are left out,
        // as native lowering is changing them.)
        let at = |line: &str| {
            printed
                .find(line)
                .unwrap_or_else(|| panic!("{:?} is not in\n{}", line, printed))
        };
        let order = chain.iter().copied().chain([
            "\n  dsd_post_agg_filter | emit output dsd_client_telemetry\n",
            " = (dsd_post_agg_filter, otlp, checks, liveness, trace_sampler)",
            " | emit output dd_metrics_encode\n",
            "\n  af_metrics_gateway | emit output af_metrics_encode\n",
        ]);
        let positions = order.map(at).collect::<Vec<_>>();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{}", printed);

        // It parses back to the same program, and composes to the default program, with its hash.
        let parsed = parse_ok(&printed).artifact;
        assert_same_program(&parsed, &export, &printed);
        assert_eq!(print_default(&parsed), printed);
        assert_eq!(
            composed_hash(&plan, &parsed),
            plan.hash(ResolvedValues::Include).unwrap()
        );
    }
}

#[test]
fn the_default_programs_dogstatsd_chain_prints_within_the_line_width() {
    let export = export_dogstatsd_chain();
    assert_eq!((export.nodes.len(), export.edges.len()), (12, 14));
    let printed = print_default(&export);
    let long = printed
        .lines()
        .filter(|line| line.chars().count() > 120)
        .collect::<Vec<_>>();
    assert!(long.is_empty(), "lines over 120 columns: {:#?}", long);

    // Set matches whose bindings do not fit on the line put each argument on its own line. In normal form, both
    // filters keep what does not match, as `where not`.
    let blocklist = r#"
    | where not intake_name matches(
        values: config.dogstatsd.metric_filter.values,
        prefix: config.dogstatsd.metric_filter.match_prefix,
      )

  let dsd_no_agg_split = dsd_prefix_filter_blocklist
    | timestamp_split(config.dogstatsd.aggregation.window_duration_seconds)
"#;
    let post_aggregation = r#"
    | where not (
        kind in [counter, rate, gauge, set]
        and intake_name matches(
          values: config.dogstatsd.post_aggregate_filter.values,
          prefix: config.dogstatsd.metric_filter.match_prefix,
        )
      )

  dsd_post_agg_filter | emit output dsd_client_telemetry

  dsd_post_agg_filter | exit enrichment
"#;
    assert!(printed.contains(blocklist), "{}", printed);
    assert!(printed.contains(post_aggregation), "{}", printed);

    assert_same_program(&parse_ok(&printed).artifact, &export, &printed);
}
