//! Exports of lowered default programs: every configuration of the matrix exports its one scope, the export prints
//! and parses back as the same document but for the order of its edges, and composing either back gives the default
//! program again, with its hash.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use saluki_metric_plan::{
    canonical::{PlanHash, ResolvedValues},
    compose::{compose, receive_artifact, Approval},
    export::export,
    install::diff_with_correspondence,
    ir::{OutputId, ScopeName},
    operators::{names, OperatorRegistry},
    validate::Catalog as _,
};
use saluki_metric_plan_syntax::{parse, print, PrintOptions};
use serde_json::Value;

use super::{matrix::MATRIX, Lowering};
use crate::programmable_metrics::{
    catalog::METRICS_SCOPE,
    test_support::{configuration, facts},
};

#[tokio::test]
async fn every_lowered_default_program_exports_and_composes_back_unchanged() {
    let registry = OperatorRegistry::native();
    let scope = ScopeName::new(METRICS_SCOPE).unwrap();
    for (name, yaml) in MATRIX {
        let config = configuration(yaml).await;
        let lowered = Lowering::new(Arc::new(config.clone()), facts(&config))
            .expect("the configuration lowers")
            .expect("the configuration has a metrics pipeline")
            .lower(&config)
            .expect("the configuration lowers");
        let plan = &lowered.plan;
        let bundle = export(plan, &scope, &lowered.catalog.to_document(), &BTreeMap::new())
            .unwrap_or_else(|e| panic!("{}: {}", name, e));
        assert!(!bundle.artifact.nodes.is_empty(), "{}", name);

        // The base program hash, which ADP reports as `default_program_hash`, is the digest of the exported document
        // without it, and the base records the catalog's fingerprint of every output the export emits to.
        let mut document = serde_json::to_value(&bundle.artifact).unwrap();
        document["base"].as_object_mut().unwrap().remove("program_hash");
        assert_eq!(
            PlanHash::of_value(&document).unwrap(),
            bundle.artifact.base.program_hash,
            "{}",
            name
        );
        let emitted = bundle
            .artifact
            .nodes
            .iter()
            .filter(|node| node.op.as_str() == names::EMIT)
            .map(|node| {
                node.params
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap()
                    .parse()
                    .unwrap()
            })
            .collect::<BTreeSet<OutputId>>();
        assert_eq!(
            bundle.artifact.base.outputs.keys().cloned().collect::<BTreeSet<_>>(),
            emitted,
            "{}",
            name
        );
        for (output, fingerprint) in &bundle.artifact.base.outputs {
            assert_eq!(
                lowered.catalog.output(output).map(|declared| &declared.fingerprint),
                Some(fingerprint),
                "{}: {}",
                name,
                output
            );
        }

        // Lowering writes nodes in the syntax's canonical order, so the export prints and parses back as itself, but
        // for the order of its edges: the printed form writes the edges from the scope's inputs first, in input-name
        // order, and each node's edges in its block, and the export lists them in the plan's order.
        let printed = print(&bundle.artifact, &PrintOptions::default());
        let parsed = parse(&printed).unwrap_or_else(|diagnostics| panic!("{}:\n{}\n{}", name, diagnostics, printed));
        let sorted = |edges: &[saluki_metric_plan::ir::ArtifactEdge]| {
            let mut edges = edges
                .iter()
                .map(|edge| format!("{} -> {}", edge.from, edge.to))
                .collect::<Vec<_>>();
            edges.sort();
            edges
        };
        assert_eq!(parsed.artifact.nodes, bundle.artifact.nodes, "{}:\n{}", name, printed);
        assert_eq!(
            sorted(&parsed.artifact.edges),
            sorted(&bundle.artifact.edges),
            "{}:\n{}",
            name,
            printed
        );
        assert_eq!(print(&parsed.artifact, &PrintOptions::default()), printed, "{}", name);

        let approval = Approval {
            revision: 1,
            approved_against: None,
            active_plan_hash: None,
            in_effect: false,
        };
        let compose_document = |artifact: &saluki_metric_plan::ir::PolicyArtifact| {
            let received =
                receive_artifact(serde_json::to_value(artifact).unwrap()).unwrap_or_else(|e| panic!("{}: {}", name, e));
            compose(plan, &received, &approval, &lowered.catalog, &registry)
                .unwrap_or_else(|e| panic!("{}: {}", name, e))
        };

        // Composing the export back, with ADP's catalog, gives the default program, with the same hash, and its diff
        // against the default program is empty.
        let composed = compose_document(&bundle.artifact);
        assert!(composed.report.findings().is_empty(), "{}:\n{}", name, composed.report);
        assert!(!composed.stale_base && !composed.stale_review, "{}", name);
        assert_eq!(composed.plan.nodes, plan.nodes, "{}", name);
        assert_eq!(composed.plan.edges, plan.edges, "{}", name);
        assert_eq!(
            composed.plan.hash(ResolvedValues::Include).unwrap(),
            plan.hash(ResolvedValues::Include).unwrap(),
            "{}",
            name
        );
        let diff = diff_with_correspondence(plan, &composed.plan, &composed.correspondence, &registry);
        assert!(diff.diff.is_empty(), "{}: {:?}", name, diff.diff);
        assert_eq!(diff.artifact_nodes.len(), bundle.artifact.nodes.len(), "{}", name);

        // Composing the printed export, whose edges are in another order, gives the default program too, with the same
        // hash: composition puts an artifact's edges in the default program's order.
        let composed = compose_document(&parsed.artifact);
        assert!(composed.report.findings().is_empty(), "{}:\n{}", name, composed.report);
        assert_eq!(composed.plan.nodes, plan.nodes, "{}", name);
        assert_eq!(composed.plan.edges, plan.edges, "{}", name);
        assert_eq!(
            composed.plan.hash(ResolvedValues::Include).unwrap(),
            plan.hash(ResolvedValues::Include).unwrap(),
            "{}",
            name
        );
        let diff = diff_with_correspondence(plan, &composed.plan, &composed.correspondence, &registry);
        assert!(diff.diff.is_empty(), "{}: {:?}", name, diff.diff);
    }
}

/// Returns the export of the one scope of the configuration `yaml` as ADP serves it, with its annotations.
async fn served_export(yaml: &str) -> saluki_metric_plan::export::ExportBundle {
    let config = configuration(yaml).await;
    let lowered = Lowering::new(Arc::new(config.clone()), facts(&config))
        .unwrap()
        .unwrap()
        .lower(&config)
        .unwrap();
    let scope = ScopeName::new(METRICS_SCOPE).unwrap();
    let mut bundle = export(&lowered.plan, &scope, &lowered.catalog.to_document(), &BTreeMap::new()).unwrap();
    bundle.annotations = crate::programmable_metrics::installer::export_annotations(&bundle, &lowered.annotations);
    bundle
}

#[tokio::test]
async fn the_export_notes_what_each_node_and_binding_of_the_default_program_is() {
    let bundle = served_export(super::matrix::EVERY_BRANCH).await;
    let nodes = bundle
        .annotations
        .nodes
        .iter()
        .map(|(node, note)| (node.as_str(), note.as_str()))
        .collect::<Vec<_>>();
    // Every reserved node lowering notes, in ID order, and each routing filter with its endpoints; terminals and
    // sources have no note.
    assert_eq!(
        nodes,
        [
            (
                "af_metrics_gateway",
                "Autoscaling failover: listed scalar series to the Cluster Agent"
            ),
            (
                "dsd_agg",
                "Aggregation; edits keep its state unless they change its window or lineage"
            ),
            (
                "dsd_enrich",
                "DogStatsD mapper: renames and tags by `dogstatsd_mapper_profiles`"
            ),
            (
                "dsd_no_agg_split",
                "Timestamped points skip aggregation (no-aggregation pipeline)"
            ),
            (
                "dsd_post_agg_filter",
                "Drops filterlisted histogram statistics, after aggregation"
            ),
            (
                "dsd_post_agg_filter_fanout",
                "Copies every metric to the client telemetry tap and to enrichment"
            ),
            ("dsd_post_agg_filter_in", "Aggregated and timestamped points rejoin"),
            (
                "dsd_prefix_filter",
                "Namespace prefix, from `statsd_metric_namespace`, except blocklisted names"
            ),
            (
                "dsd_prefix_filter_blocklist",
                "Metric filterlist, from `metric_filterlist`: matches drop"
            ),
            (
                "dsd_tag_filterlist",
                "Tag filterlist and value rules: `metric_tag_filterlist`, `metric_tag_value_allowlist`"
            ),
            ("metrics_enrich", "Host enrichment: leaves metrics unchanged"),
            (
                "metrics_enrich_host_tags",
                "Adds the host's tags (a fact) until `expected_tags_duration` passes"
            ),
            ("metrics_enrich_in", "Every source's metrics, to enrichment"),
            ("metrics_policy", "Policy: one branch per output; route here"),
            ("metrics_routing_filter_0", "Endpoint routing to https://c.example.com"),
            ("metrics_routing_filter_1", "Endpoint routing to https://b.example.com"),
            (
                "mrf_metrics_gateway",
                "Multi-Region Failover: mirrors listed, or all, metrics while failing over"
            ),
        ]
    );
    // Every binding of the catalog has its class and meaning.
    let catalog = bundle
        .catalog
        .bindings()
        .map(|(name, _)| name.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        bundle.annotations.bindings.keys().cloned().collect::<BTreeSet<_>>(),
        catalog
    );
    let binding = |name: &str| bundle.annotations.bindings[&name.parse().unwrap()].as_str();
    assert_eq!(
        binding("autoscaling_failover.enabled"),
        "signal: Whether autoscaling failover sends metrics to the Cluster Agent, from `autoscaling.failover.enabled`."
    );
    assert_eq!(
        binding("host.tags_deadline"),
        "fact: When host tags stop being added: the process start plus `expected_tags_duration`."
    );
}

#[tokio::test]
async fn a_routing_note_spells_its_endpoints_as_status_does() {
    // An endpoint with user information is written as its digest, as status spells it.
    let bundle = served_export(
        "additional_endpoints: {\"https://user:secret@b.example.com\": [k]}\nexperimental: {metrics_endpoint_routing: \
         {metric_allowlist: {\"https://user:secret@b.example.com\": [m]}}}",
    )
    .await;
    let note = &bundle.annotations.nodes[&"metrics_routing_filter_0"
        .parse::<saluki_metric_plan::ir::ArtifactNodeId>()
        .unwrap()];
    assert!(note.starts_with("Endpoint routing to sha256:"), "{}", note);
    assert!(!serde_json::to_string(&bundle).unwrap().contains("secret"));
}
