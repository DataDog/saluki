//! Exports of lowered default programs: every configuration of the matrix exports its program, the export prints
//! and parses back as the same document but for the order of its nodes and edges, and composing the detached export,
//! or its printed form, back gives a plan that does what the default program does, and the printed form the same core
//! plan as the export itself. The detached export leaves out exactly the nodes the comparison of what plans do leaves
//! out. The bound export, which writes every node as the default program has it, composes back to the default
//! program's core plan itself.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use saluki_metric_plan::{
    compose::{compose, receive_artifact, Approval},
    elaborate::elaborate_default,
    export::{eliminate, export, export_as, program_hash, ExportForm, Omission},
    install::diff_operations,
    ir::Plan,
    operators::{names, OperatorRegistry},
    validate::Rule,
};
use saluki_metric_plan_syntax::{parse, print, PrintOptions};
use serde_json::Value;

use super::{matrix::MATRIX, Lowering};
use crate::programmable_metrics::test_support::{configuration, facts};

#[tokio::test]
async fn every_lowered_default_program_exports_and_composes_back_unchanged() {
    let registry = OperatorRegistry::native();
    for (name, yaml) in MATRIX {
        let config = configuration(yaml).await;
        let lowered = Lowering::new(Arc::new(config.clone()), facts(&config))
            .expect("the configuration lowers")
            .expect("the configuration has a metrics pipeline")
            .lower(&config)
            .expect("the configuration lowers");
        let plan = &lowered.plan;
        let bundle = export(plan, &lowered.catalog.to_document(), &BTreeMap::new())
            .unwrap_or_else(|e| panic!("{}: {}", name, e));
        assert!(!bundle.artifact.nodes.is_empty(), "{}", name);
        // The export writes no source: a program's edges leave the sources by their bindings' names.
        assert!(
            bundle
                .artifact
                .nodes
                .iter()
                .all(|node| node.op.as_str() != names::SOURCE),
            "{}",
            name
        );

        // The program hash is what the export hashes to when it is received unmodified.
        assert_eq!(
            program_hash(&bundle.artifact).unwrap(),
            receive_artifact(serde_json::to_value(&bundle.artifact).unwrap())
                .unwrap()
                .hash,
            "{}",
            name
        );

        // The export prints and parses back as itself, but for the order of its nodes and edges, and for the
        // parameters it writes with their defaults, which the printed form leaves out and normal form writes again:
        // the printed form writes the edges from the sources first, in source-name order, and each node's edges
        // in its block, and the drop of a filter in normal form, which keeps its `true` port, after what it keeps,
        // while the export lists them in the plan's order. Elaboration puts them in canonical order.
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
        let by_id = |nodes: &[saluki_metric_plan::ir::ArtifactNode]| {
            let mut nodes = nodes.to_vec();
            nodes.sort_by(|a, b| a.id.cmp(&b.id));
            nodes
        };
        assert_eq!(
            by_id(&saluki_metric_plan::normalize::normalize_artifact(&parsed.artifact).nodes),
            by_id(&bundle.artifact.nodes),
            "{}:\n{}",
            name,
            printed
        );
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

        // The bound export writes every policy setting as the default program does, as a literal: it composes back to
        // the default program's core plan itself, node for node and edge for edge, with its hash.
        let default = elaborate_default(plan, &lowered.catalog, &registry);
        let core = &default.plan;
        let bound = compose_document(&bundle.artifact);
        assert_eq!(bound.plan.nodes, core.nodes, "{}", name);
        assert_eq!(bound.plan.edges, core.edges, "{}", name);
        assert_eq!(bound.plan.outputs, core.outputs, "{}", name);
        assert_eq!(bound.plan.resources, core.resources, "{}", name);
        assert_eq!(bound.hash().unwrap(), default.hash().unwrap(), "{}", name);
        assert_eq!(bound.ids.len(), bundle.artifact.nodes.len(), "{}", name);

        // Composing the detached export back, with ADP's catalog, gives a plan that does what the default program
        // does, and its diff against the default program by what they do is empty.
        let detached = export_as(
            plan,
            &lowered.catalog.to_document(),
            &BTreeMap::new(),
            ExportForm::Detached,
            &OperatorRegistry::native(),
        )
        .unwrap_or_else(|e| panic!("{}: {}", name, e));
        assert_leaves_out_what_the_comparison_does(&detached, plan, &registry, name);
        let composed = compose_document(&detached.artifact);
        assert!(composed.report.findings().is_empty(), "{}:\n{}", name, composed.report);
        assert!(!composed.stale_review, "{}", name);
        assert_does_the_same(&composed.plan, core, &registry, name);
        // What the comparison leaves out of the composed plan is what it leaves out of the default program's core plan
        // and the export did not: what a fact's or signal's value decides, which the export keeps bound and the core
        // plan writes as a value.
        let omitted = |plan: &Plan| {
            eliminate(plan, &registry)
                .omitted
                .into_iter()
                .map(|(node, _)| node.local().to_string())
                .collect::<BTreeSet<_>>()
        };
        let exported = detached
            .omitted
            .keys()
            .map(ToString::to_string)
            .collect::<BTreeSet<_>>();
        let decided = omitted(core).difference(&exported).cloned().collect::<BTreeSet<_>>();
        assert_eq!(omitted(&composed.plan), decided, "{}", name);
        let diff = diff_operations(core, &composed.plan, &registry);
        assert!(diff.is_empty(), "{}: {:?}", name, diff);
        assert_eq!(composed.ids.len(), detached.artifact.nodes.len(), "{}", name);

        // Composing the printed detached export, whose nodes and edges are in another order, gives the same core plan,
        // with the same hash: elaboration puts nodes and edges in canonical order.
        let printed = print(&detached.artifact, &PrintOptions::default());
        let parsed = parse(&printed).unwrap_or_else(|diagnostics| panic!("{}:\n{}\n{}", name, diagnostics, printed));
        let unprinted = composed;
        let composed = compose_document(&parsed.artifact);
        assert_eq!(composed.plan.nodes, unprinted.plan.nodes, "{}", name);
        assert_eq!(composed.plan.edges, unprinted.plan.edges, "{}", name);
        assert_eq!(composed.hash().unwrap(), unprinted.hash().unwrap(), "{}", name);
        assert!(composed.report.findings().is_empty(), "{}:\n{}", name, composed.report);
        let diff = diff_operations(core, &composed.plan, &registry);
        assert!(diff.is_empty(), "{}: {:?}", name, diff);
    }
}

/// Asserts that the detached export of `plan` leaves out exactly the nodes of its program that the comparison of what
/// plans do leaves out of `plan`, with ADP's facts and signals kept bound, for the same reasons: one pass decides both.
fn assert_leaves_out_what_the_comparison_does(
    detached: &saluki_metric_plan::export::ExportBundle, plan: &Plan, registry: &OperatorRegistry, name: &str,
) {
    let compared = eliminate(plan, registry)
        .omitted
        .into_iter()
        // The sources are not the program's, and are never left out.
        .map(|(node, why)| (node.local().to_string(), why))
        .collect::<BTreeMap<_, _>>();
    let exported = detached
        .omitted
        .iter()
        .map(|(node, why)| (node.to_string(), *why))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(exported, compared, "{}", name);
    // Host enrichment changes no metric in any configuration, and the nodes the export keeps are all it has.
    let enrichment = plan
        .nodes
        .iter()
        .find(|node| node.op.as_str() == names::HOST_ENRICHMENT)
        .map(|node| node.id.local().to_string())
        .expect("the default program has host enrichment");
    assert_eq!(exported.get(&enrichment), Some(&Omission::Identity), "{}", name);
    for node in &detached.artifact.nodes {
        assert!(!exported.contains_key(node.id.as_str()), "{}: {}", name, node.id);
    }
}

/// Asserts that `composed` does what the default program's core plan `core` does: without what does nothing as
/// configured, the same nodes, each the same operation, and the same edges.
fn assert_does_the_same(composed: &Plan, core: &Plan, registry: &OperatorRegistry, name: &str) {
    let (composed, core) = (&eliminate(composed, registry).plan, &eliminate(core, registry).plan);
    let edges = |plan: &Plan| {
        let mut edges = plan
            .edges
            .iter()
            .map(|edge| format!("{} -> {}", edge.from, edge.to))
            .collect::<Vec<_>>();
        edges.sort();
        edges
    };
    assert_eq!(edges(composed), edges(core), "{}", name);
    let by_id = |plan: &Plan| {
        let mut nodes = plan.nodes.clone();
        nodes.sort_by(|a, b| a.id.cmp(&b.id));
        nodes
    };
    assert_eq!(by_id(composed), by_id(core), "{}", name);
}

/// Lowers the configuration `yaml`, and composes its unmodified detached export, and then the export with `edit`.
/// Returns the default program's core plan, the export, and both compositions.
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
    let detached = export_as(
        &lowered.plan,
        &lowered.catalog.to_document(),
        &BTreeMap::new(),
        ExportForm::Detached,
        &OperatorRegistry::native(),
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
        elaborate_default(&lowered.plan, &lowered.catalog, &registry).plan,
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
        mapper["params"]["string_interner_bytes"] = serde_json::json!(134217728 + 67108864);
    })
    .await;
    // The unmodified export keeps it: valid, and the same as the default program, so it would adopt its generation.
    assert!(unmodified.report.is_valid(), "{}", unmodified.report);
    assert_does_the_same(&unmodified.plan, &plan, &OperatorRegistry::native(), "unmodified");
    // Growth beyond what native configuration sized the host for is refused: the limit holds. (The program leaves out
    // the tag filterlist, which has no rules, and the memory its context cache declares with it, which the mapper may
    // take instead; 64 MiB more it may not.)
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

/// Returns the export of the configuration `yaml`'s default program, bound, with its annotations.
async fn served_export(yaml: &str) -> saluki_metric_plan::export::ExportBundle {
    let config = configuration(yaml).await;
    let lowered = Lowering::new(Arc::new(config.clone()), facts(&config))
        .unwrap()
        .unwrap()
        .lower(&config)
        .unwrap();
    let mut bundle = export(&lowered.plan, &lowered.catalog.to_document(), &BTreeMap::new()).unwrap();
    bundle.annotations = crate::programmable_metrics::installer::export_annotations(&bundle, &lowered.annotations);
    bundle
}

/// Returns the detached export of the configuration `yaml`'s default program as ADP serves it, with its annotations.
async fn served_detached_export(yaml: &str) -> saluki_metric_plan::export::ExportBundle {
    let config = configuration(yaml).await;
    let lowered = Lowering::new(Arc::new(config.clone()), facts(&config))
        .unwrap()
        .unwrap()
        .lower(&config)
        .unwrap();
    let mut bundle = export_as(
        &lowered.plan,
        &lowered.catalog.to_document(),
        &BTreeMap::new(),
        ExportForm::Detached,
        &OperatorRegistry::native(),
    )
    .unwrap();
    bundle.annotations = crate::programmable_metrics::installer::export_annotations(&bundle, &lowered.annotations);
    bundle
}

#[tokio::test]
async fn the_detached_export_names_what_it_leaves_out_in_one_note_in_program_order() {
    // DogStatsD with its defaults: nothing configured for its processing, and DogStatsD the only source.
    let bundle = served_detached_export("").await;
    let omitted = bundle
        .omitted
        .iter()
        .map(|(node, why)| (node.as_str(), *why))
        .collect::<Vec<_>>();
    use Omission::*;
    assert_eq!(
        omitted,
        [
            ("dsd_enrich", Identity),
            ("dsd_post_agg_filter", ConstantSplit),
            ("dsd_post_agg_filter_false", Unreachable),
            ("dsd_prefix_filter_blocklist", ConstantSplit),
            ("dsd_prefix_filter_blocklist_false", Unreachable),
            ("dsd_prefix_filter_blocklist_namespace_prefix", Identity),
            ("dsd_tag_filterlist", Identity),
            ("metrics", Identity),
            ("metrics_in", Identity),
        ]
    );
    // Human names, in program order; the `drop`s go with their filters, unnamed.
    assert_eq!(
        bundle.annotations.omitted.as_deref(),
        Some(
            "Not shown, because they do nothing as configured: mapper (no profiles), namespace prefix (none), metric \
             filterlist (empty), tag filterlist (empty), post-aggregation filter (empty), enrichment merge \
             (one input), host enrichment (changes no metric)"
        )
    );
    for node in bundle.omitted.keys() {
        assert!(!bundle.annotations.nodes.contains_key(node), "{}", node);
    }

    // Configured, they show: with a mapper profile, a namespace, a filterlist entry, a tag rule, and a second source,
    // only host enrichment and the histogram statistics filter, since the filterlist names no statistic, are left out.
    let bundle = served_detached_export(
        "metric_filterlist: [a.b]\nstatsd_metric_namespace: tenant\ndogstatsd_mapper_profiles: [{name: p, prefix: \
         \"p.\", mappings: [{match: \"p.*\", name: q}]}]\nmetric_tag_filterlist: [{metric_name: x, action: exclude, \
         tags: [y]}]\ndata_plane: {otlp: {enabled: true}}",
    )
    .await;
    let omitted = bundle.omitted.keys().map(|node| node.as_str()).collect::<Vec<_>>();
    assert_eq!(
        omitted,
        ["dsd_post_agg_filter", "dsd_post_agg_filter_false", "metrics"],
        "{:?}",
        bundle.annotations.omitted
    );
    // The bound export leaves nothing out, and says nothing.
    let bound = served_export("").await;
    assert!(bound.omitted.is_empty() && bound.annotations.omitted.is_none());
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
                "dsd_prefix_filter_blocklist",
                "Metric filterlist, from `metric_filterlist`: matches drop"
            ),
            (
                "dsd_prefix_filter_blocklist_namespace_prefix",
                "Namespace prefix, from `statsd_metric_namespace`, except blocklisted names"
            ),
            (
                "dsd_tag_filterlist",
                "Tag filterlist and value rules: `metric_tag_filterlist`, `metric_tag_value_allowlist`"
            ),
            (
                "metrics",
                "Adds the host's tags (a fact) until `expected_tags_duration` passes"
            ),
            ("metrics_fanout", "Policy: one branch per output; route here"),
            ("metrics_host_enrichment", "Host enrichment: leaves metrics unchanged"),
            ("metrics_in", "Every source's metrics, to enrichment"),
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
