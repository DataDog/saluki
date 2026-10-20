//! The default program, exported as an artifact, printed, and parsed back: the whole metrics path after its sources.
//!
//! The tests that compose a parsed export back into the default program need the plan crate's `compose`.

use std::collections::BTreeMap;

use saluki_metric_plan::{
    canonical::PlanHash,
    ir::{Plan, PolicyArtifact},
    testing::{
        catalog_for, default_program_catalog, with_opaque_gateways, DEFAULT_PROGRAM_JSON,
        NO_AGGREGATION_PIPELINE_OFF_JSON,
    },
};

use super::*;

fn default_program() -> Plan {
    Plan::from_json(DEFAULT_PROGRAM_JSON).expect("the default program fixture reads")
}

/// Exports a plan, the whole program, as the host does, as an artifact.
fn program_export(plan: &Plan) -> PolicyArtifact {
    saluki_metric_plan::export::export(plan, &default_program_catalog(), &BTreeMap::new())
        .expect("the default program exports")
        .artifact
}

#[test]
fn the_default_programs_export_prints_with_a_let_for_each_named_stream_and_no_as() {
    // The normalized export: every node whose ID is not the one its position derives ends a `let`
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
        .collect::<Vec<_>>();
    assert_eq!(
        statements,
        [
            "  let dsd_enrich = dogstatsd",
            "  let dsd_prefix_filter = dsd_enrich",
            "  let dsd_no_agg_split = dsd_prefix_filter | timestamp_split(10s)",
            "  let dsd_tag_filterlist = dsd_no_agg_split.default",
            "  let dsd_agg = dsd_tag_filterlist | aggregate(window: 10s)",
            "  let dsd_post_agg_filter = (dsd_agg, dsd_no_agg_split.passthrough)",
            "  dsd_post_agg_filter | emit output dsd_client_telemetry",
            "  let metrics = (dsd_post_agg_filter, otlp, checks, liveness, trace_sampler)",
            "  metrics | emit output dd_metrics_encode",
            "  let metrics_routing_filter_0 = metrics",
            "  metrics_routing_filter_0 | emit output metrics_routing_encode_0",
            "  let metrics_routing_filter_1 = metrics | where intake_name starts_with \"catalog.\"",
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
        "\n    | namespace_prefix(\"tenant\", exclusions: [\"internal.\", \"system.\"])\n",
        "\n    | where intake_name != \"tenant.raw.blocked\"\n",
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
    // The export is in normal form, with every default written, and lists its nodes and edges in the plan's order. Its
    // text leaves out the defaults and parses in canonical order, so it parses back to the same program once normalized
    // and in canonical order, which elaborates to the default program's core plan, with its hash, which adoption
    // compares.
    for (what, plan) in plans {
        let export = program_export(&plan);
        let printed = print_default(&export);
        let parsed = parse_ok(&printed);
        assert_same_program(&parsed.artifact, &export, &format!("{}:\n{}", what, printed));
        if !what.ends_with("opaque gateways") {
            // (A program cannot hold host components, such as opaque gateways.)
            assert_eq!(composed_hash(&plan, &parsed.artifact), core_hash(&plan), "{}", what);
        }
        assert_eq!(print_default(&parsed.artifact), printed, "{}", what);
    }
}

/// Returns the hash of `plan`'s core plan, which installs, and which adoption compares.
fn core_hash(plan: &Plan) -> PlanHash {
    saluki_metric_plan::elaborate::elaborate_default(
        plan,
        &catalog_for(plan),
        &saluki_metric_plan::operators::OperatorRegistry::native(),
    )
    .hash()
    .unwrap()
}

/// Returns the hash of the core plan `artifact` composes to in `plan`, with the host's catalog for it: an unmodified
/// export's nodes resolve their bindings from it, and it carries the default program's values.
fn composed_hash(plan: &Plan, artifact: &PolicyArtifact) -> PlanHash {
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
        &catalog_for(plan),
        &OperatorRegistry::native(),
    )
    .unwrap();
    assert!(composed.report.findings().is_empty(), "{}", composed.report);
    composed.hash().unwrap()
}

#[test]
fn the_default_program_prints_from_its_sources_through_aggregation_and_enrichment_to_its_routes() {
    // (plan, how its DogStatsD chain starts and reaches the post-aggregation filter)
    let cases = [
        (
            default_program(),
            &[
                "\n  let dsd_enrich = dogstatsd\n",
                "\n  let dsd_post_agg_filter = (dsd_agg, dsd_no_agg_split.passthrough)\n    | where not (kind in",
            ][..],
        ),
        (
            Plan::from_json(NO_AGGREGATION_PIPELINE_OFF_JSON).unwrap(),
            &[
                "\n  let dsd_enrich = dogstatsd\n",
                "\n  let dsd_tag_filterlist = dsd_prefix_filter\n",
                "\n  let dsd_post_agg_filter = dsd_agg\n    | where not (kind in",
            ][..],
        ),
    ];
    for (plan, chain) in cases {
        let export = program_export(&plan);
        // Every node but the five sources, and every edge.
        assert_eq!(export.nodes.len(), plan.nodes.len() - 5);
        assert_eq!(export.edges.len(), plan.edges.len());
        let printed = print_default(&export);
        assert!(printed.starts_with("policy {\n"), "{}", printed);
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

        // It parses back to the same program, and elaborates to the default program's core plan.
        let parsed = parse_ok(&printed).artifact;
        assert_same_program(&parsed, &export, &printed);
        assert_eq!(print_default(&parsed), printed);
        assert_eq!(composed_hash(&plan, &parsed), core_hash(&plan));
    }
}
