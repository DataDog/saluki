//! Exports of lowered default programs: every configuration of the matrix exports its one scope, the export prints
//! and parses back as the same document but for the order of its edges, and composing the detached export, or its
//! printed form, back gives a plan that does what the default program does, with its operation hash. The bound
//! export, which binds native policy settings, is refused: ADP's programs bind facts and signals only.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use saluki_metric_plan::{
    canonical::PlanHash,
    compose::{compose, receive_artifact, Approval},
    export::{export, export_as, operation_hash, operation_params, ExportForm},
    install::diff_with_correspondence,
    ir::{OutputId, ScopeName},
    operators::{names, OperatorRegistry},
    validate::{Catalog as _, Rule},
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
        let try_compose = |artifact: &saluki_metric_plan::ir::PolicyArtifact| {
            let received =
                receive_artifact(serde_json::to_value(artifact).unwrap()).unwrap_or_else(|e| panic!("{}: {}", name, e));
            compose(plan, &received, &approval, &lowered.catalog, &registry)
        };
        let compose_document = |artifact: &saluki_metric_plan::ir::PolicyArtifact| {
            try_compose(artifact).unwrap_or_else(|e| panic!("{}: {}", name, e))
        };

        // The bound export binds policy settings, which a published program owns: refused, unless it binds none.
        match try_compose(&bundle.artifact) {
            Ok(_) | Err(saluki_metric_plan::compose::ComposeError::PolicyBinding { .. }) => {}
            Err(other) => panic!("{}: {}", name, other),
        }

        // Composing the detached export back, with ADP's catalog, gives a plan that does what the default program
        // does, writing its policy as literals, and its diff against the default program is empty.
        let detached = export_as(
            plan,
            &scope,
            &lowered.catalog.to_document(),
            &BTreeMap::new(),
            ExportForm::Detached,
        )
        .unwrap_or_else(|e| panic!("{}: {}", name, e));
        assert_eq!(detached.artifact.base, bundle.artifact.base, "{}", name);
        let composed = compose_document(&detached.artifact);
        assert!(composed.report.findings().is_empty(), "{}:\n{}", name, composed.report);
        assert!(!composed.stale_base && !composed.stale_review, "{}", name);
        assert_does_the_same(&composed.plan, plan, &registry, name);
        let diff = diff_with_correspondence(plan, &composed.plan, &composed.correspondence, &registry);
        assert!(diff.diff.is_empty(), "{}: {:?}", name, diff.diff);
        assert_eq!(diff.artifact_nodes.len(), detached.artifact.nodes.len(), "{}", name);

        // Composing the printed detached export, whose edges are in another order, gives the same: composition puts an
        // artifact's edges in the default program's order.
        let printed = print(&detached.artifact, &PrintOptions::default());
        let parsed = parse(&printed).unwrap_or_else(|diagnostics| panic!("{}:\n{}\n{}", name, diagnostics, printed));
        let composed = compose_document(&parsed.artifact);
        assert!(composed.report.findings().is_empty(), "{}:\n{}", name, composed.report);
        assert_does_the_same(&composed.plan, plan, &registry, name);
        let diff = diff_with_correspondence(plan, &composed.plan, &composed.correspondence, &registry);
        assert!(diff.diff.is_empty(), "{}: {:?}", name, diff.diff);
    }
}

/// Asserts that `composed` does what the default program `plan` does: the same edges, each node the same operation,
/// and so the same operation hash, by which the installer adopts or keeps the running generation.
fn assert_does_the_same(
    composed: &saluki_metric_plan::ir::Plan, plan: &saluki_metric_plan::ir::Plan, registry: &OperatorRegistry,
    name: &str,
) {
    assert_eq!(composed.edges, plan.edges, "{}", name);
    assert_eq!(composed.nodes.len(), plan.nodes.len(), "{}", name);
    for (composed, default) in composed.nodes.iter().zip(&plan.nodes) {
        assert_eq!((&composed.id, &composed.op), (&default.id, &default.op), "{}", name);
        assert_eq!(
            operation_params(&composed.op, &composed.params),
            operation_params(&default.op, &default.params),
            "{}: {}",
            name,
            composed.id
        );
    }
    assert_eq!(
        operation_hash(composed, registry).unwrap(),
        operation_hash(plan, registry).unwrap(),
        "{}",
        name
    );
}

/// Lowers the configuration `yaml`, and composes its unmodified detached export, and then the export with `edit`.
/// Returns the default program, the export, and both compositions.
async fn compose_detached(
    yaml: &str, edit: impl FnOnce(&mut Value),
) -> (
    saluki_metric_plan::ir::Plan,
    Value,
    saluki_metric_plan::compose::Composed,
    saluki_metric_plan::compose::Composed,
) {
    let registry = OperatorRegistry::native();
    let config = configuration(yaml).await;
    let lowered = Lowering::new(Arc::new(config.clone()), facts(&config))
        .unwrap()
        .unwrap()
        .lower(&config)
        .unwrap();
    let scope = ScopeName::new(METRICS_SCOPE).unwrap();
    let detached = export_as(
        &lowered.plan,
        &scope,
        &lowered.catalog.to_document(),
        &BTreeMap::new(),
        ExportForm::Detached,
    )
    .unwrap();
    let approval = Approval {
        revision: 1,
        approved_against: None,
        active_plan_hash: None,
        in_effect: false,
    };
    let document = serde_json::to_value(&detached.artifact).unwrap();
    let mut edited = document.clone();
    edit(&mut edited);
    let compose_document = |document: Value| {
        compose(
            &lowered.plan,
            &receive_artifact(document).unwrap(),
            &approval,
            &lowered.catalog,
            &registry,
        )
        .unwrap()
    };
    (
        lowered.plan.clone(),
        document.clone(),
        compose_document(document),
        compose_document(edited),
    )
}

/// Returns the errors of `composed`'s report under `rule`, as their messages.
fn errors_of(composed: &saluki_metric_plan::compose::Composed, rule: Rule) -> Vec<String> {
    composed
        .report
        .errors()
        .filter(|finding| finding.rule == rule)
        .map(|finding| finding.message.clone())
        .collect()
}

#[tokio::test]
async fn a_program_may_keep_a_mapper_interner_beyond_the_memory_limit_that_native_configuration_set_but_not_grow_it() {
    // Native configuration sizes the DogStatsD mapper's interner at 128 MiB, twice the memory limit on programs.
    let yaml = "dogstatsd_mapper_string_interner_size: 134217728\ndogstatsd_mapper_profiles: [{name: p, prefix: \
                \"p.\", mappings: [{match: \"p.*\", name: q}]}]";
    let (plan, _, unmodified, grown) = compose_detached(yaml, |document| {
        let mapper = document["nodes"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|node| node["op"] == "dogstatsd_mapper")
            .expect("the program maps DogStatsD names");
        mapper["params"]["string_interner_bytes"] = serde_json::json!(134217728 + 1);
    })
    .await;
    // The unmodified export keeps it: valid, and the same as the default program, so it would adopt its generation.
    assert!(unmodified.report.is_valid(), "{}", unmodified.report);
    assert_does_the_same(&unmodified.plan, &plan, &OperatorRegistry::native(), "unmodified");
    // One byte more is growth beyond what native configuration sized the host for: the limit holds.
    let errors = errors_of(&grown, Rule::ArtifactMemory);
    assert_eq!(errors.len(), 1, "{}", grown.report);
    assert!(
        errors[0].contains("which the default program has here, above the limit of 67108864"),
        "{}",
        errors[0]
    );
}

#[tokio::test]
async fn a_program_may_keep_a_routing_tee_wider_than_the_fan_out_limit_but_not_widen_it() {
    // Forty routing groups, each an additional endpoint with its own allowlist: the routing tee has a port for each,
    // more than the fan-out limit on programs.
    let endpoints = (0..40)
        .map(|index| format!("https://e{}.example.com", index))
        .collect::<Vec<_>>();
    let additional = endpoints
        .iter()
        .map(|endpoint| format!("\"{}\": [k]", endpoint))
        .collect::<Vec<_>>()
        .join(", ");
    let routing = endpoints
        .iter()
        .enumerate()
        .map(|(index, endpoint)| format!("\"{}\": [m{}.]", endpoint, index))
        .collect::<Vec<_>>()
        .join(", ");
    let yaml = format!(
        "additional_endpoints: {{{}}}\nexperimental: {{metrics_endpoint_routing: {{metric_prefix_allowlist: {{{}}}}}}}",
        additional, routing
    );
    let wide = |document: &Value| {
        document["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|node| node["op"] == "tee")
            .map(|node| node["params"]["ports"].as_u64().unwrap_or(0))
            .max()
            .unwrap_or(0)
    };
    let (plan, exported, unmodified, widened) = compose_detached(&yaml, |document| {
        // Another copy of the widest tee's input: one more port, to a drop.
        let tee = document["nodes"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .filter(|node| node["op"] == "tee")
            .max_by_key(|node| node["params"]["ports"].as_u64().unwrap_or(0))
            .unwrap();
        let ports = tee["params"]["ports"].as_u64().unwrap();
        tee["params"]["ports"] = serde_json::json!(ports + 1);
        let id = tee["id"].as_str().unwrap().to_string();
        document["nodes"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"id": "extra_copy", "op": "drop"}));
        document["edges"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"from": format!("{}.{}", id, ports), "to": "extra_copy.in"}));
    })
    .await;
    assert!(wide(&exported) > 32, "{}", wide(&exported));
    assert!(unmodified.report.is_valid(), "{}", unmodified.report);
    assert_does_the_same(&unmodified.plan, &plan, &OperatorRegistry::native(), "unmodified");
    let errors = errors_of(&widened, Rule::TeeFanOut);
    assert_eq!(errors.len(), 1, "{}", widened.report);
    assert!(
        errors[0].contains("which the default program has here"),
        "{}",
        errors[0]
    );
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
