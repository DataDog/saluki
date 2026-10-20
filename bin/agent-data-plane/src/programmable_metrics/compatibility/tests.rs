use std::{collections::BTreeSet, sync::Arc};

use saluki_core::observability::metrics::RemapperRule;
use saluki_metric_plan::{
    ir::Plan,
    operators::{names, OperatorRegistry, PlanValue, Recorder},
    testing::DEFAULT_PROGRAM_JSON,
    validate::{contract::findings, Location, Rule, Validator},
};

use super::{
    contract::{claim, Scope, Unclaimed},
    *,
};
use crate::{
    components::{
        dogstatsd_post_aggregate_filter::telemetry::FILTERED_METRICS_METRIC,
        dogstatsd_prefix_filter::{
            LISTENER_FILTERED_POINTS_METRIC, METRIC_FILTERLIST_SIZE_METRIC, METRIC_FILTERLIST_UPDATES_METRIC,
        },
        metric_filter::{ALLOWLIST_METRICS_FILTERED_METRIC, ALLOWLIST_SIZE_METRIC},
    },
    programmable_metrics::{
        lowering::{
            matrix::{EVERY_BRANCH, MATRIX},
            Lowered, Lowering,
        },
        telemetry::reported,
        test_support::{configuration, facts},
    },
};

/// The components whose series the remapper reads that are the host's, not a node's: no program can remove them.
const HOST_COMPONENTS: &[&str] = &["dsd_in", "dd_events_encode", "dd_service_checks_encode"];

fn rules() -> Vec<RemapperRule> {
    remapper_rules()
}

async fn lowered(yaml: &str) -> Lowered {
    let config = configuration(yaml).await;
    Lowering::new(Arc::new(config.clone()), facts(&config))
        .unwrap()
        .unwrap()
        .lower(&config)
        .unwrap()
}

async fn lower(yaml: &str) -> Plan {
    lowered(yaml).await.plan
}

/// The rules no node of `plan` reports for, other than the host's.
fn unclaimed_by_nodes(plan: &Plan) -> Vec<Unclaimed> {
    claim(plan, &OperatorRegistry::native(), &rules())
        .1
        .into_iter()
        .filter(
            |unclaimed| !matches!(unclaimed.scope, Scope::Component(component) if HOST_COMPONENTS.contains(&component)),
        )
        .collect()
}

#[tokio::test]
async fn the_default_program_meets_the_whole_contract_and_every_rule_scoped_to_a_node() {
    let registry = OperatorRegistry::native();
    for plan in [
        lower(EVERY_BRANCH).await,
        Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap(),
    ] {
        let contract = contract(&plan, &registry, &rules());
        let entries = contract
            .entries
            .iter()
            .map(|entry| (entry.series.as_str(), entry.node.to_string(), entry.operator.as_str()))
            .collect::<BTreeSet<_>>();
        let expected = [
            ("aggregator.dogstatsd_contexts_by_mtype", "dsd_agg", names::AGGREGATE),
            (
                "aggregator.dogstatsd_contexts_bytes_by_mtype",
                "dsd_agg",
                names::AGGREGATE,
            ),
            ("aggregator.processed", "dsd_agg", names::AGGREGATE),
            ("aggregator.flush", "dsd_agg", names::AGGREGATE),
            ("aggregator.number_of_flush", "dsd_agg", names::AGGREGATE),
            ("no_aggregation.processed", "dsd_no_agg_split", names::TIMESTAMP_SPLIT),
            ("dogstatsd.listener_filtered_points", "dsd_prefix_filter", names::SPLIT),
            ("filterlist.size", "dsd_prefix_filter", names::SPLIT),
            ("filterlist.updates", "dsd_prefix_filter", names::SPLIT),
            ("no_aggregation.flush", "dsd_no_agg_split", names::TIMESTAMP_SPLIT),
            (
                "aggregator.dogstatsd_filtered_metrics",
                "dsd_post_agg_filter",
                names::SPLIT,
            ),
            ("tag_filterlist.size", "dsd_tag_filterlist", names::TAG_PROJECTION),
            ("tag_filterlist.updates", "dsd_tag_filterlist", names::TAG_PROJECTION),
            ("aggregator.filtered_tags", "dsd_tag_filterlist", names::TAG_PROJECTION),
            (
                "aggregator.filtered_tags_cache_hit",
                "dsd_tag_filterlist",
                names::TAG_PROJECTION,
            ),
            (
                "aggregator.filtered_tags_cache_miss",
                "dsd_tag_filterlist",
                names::TAG_PROJECTION,
            ),
            (
                "aggregator.filtered_tags_cache_evict",
                "dsd_tag_filterlist",
                names::TAG_PROJECTION,
            ),
        ]
        .into_iter()
        .map(|(series, node, operator)| (series, node.to_string(), operator))
        .collect::<BTreeSet<_>>();
        assert_eq!(entries, expected);
        assert!(report(&plan, &contract).is_met());
        assert!(findings(&plan, &contract).is_empty());

        // Every rule scoped to a node is met.
        let unclaimed = unclaimed_by_nodes(&plan);
        assert!(
            unclaimed.is_empty(),
            "read from no node of the default program: {:?}",
            unclaimed
        );
    }
}

/// Lowering hands the catalog the contract of the default program it lowers, and the catalog's document carries it:
/// every program is validated against it, here and in the editor.
#[tokio::test]
async fn the_catalog_carries_the_contract_of_the_default_program_and_validation_warns_against_it() {
    let lowered = lowered(EVERY_BRANCH).await;
    let registry = OperatorRegistry::native();
    let expected = contract(&lowered.plan, &registry, &rules());
    assert_eq!(of_default(&lowered.plan), expected, "every branch does something");
    let document = lowered.catalog.to_document();
    assert_eq!(
        saluki_metric_plan::validate::Catalog::telemetry_contract(&lowered.catalog),
        Some(&expected)
    );
    assert_eq!(
        saluki_metric_plan::validate::Catalog::telemetry_contract(&document),
        Some(&expected)
    );

    // The default program meets it, and names its contract nodes for the rewrites.
    let report = Validator::new(&registry, &lowered.catalog).validate(&lowered.plan);
    assert!(report.findings().is_empty(), "{}", report);
    let nodes = report
        .contract_nodes()
        .iter()
        .map(ToString::to_string)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        nodes,
        [
            "dsd_agg",
            "dsd_no_agg_split",
            "dsd_post_agg_filter",
            "dsd_prefix_filter",
            "dsd_tag_filterlist"
        ]
        .into_iter()
        .map(String::from)
        .collect()
    );

    // A program without the prefix filter's `split` gets one warning, through either catalog.
    let mut renamed = lowered.plan.clone();
    for node in &mut renamed.nodes {
        if node.id.to_string() == "dsd_prefix_filter" {
            node.op = names::TEE.parse().unwrap();
        }
    }
    for catalog in [
        &lowered.catalog as &dyn saluki_metric_plan::validate::Catalog,
        &document,
    ] {
        let report = Validator::new(&registry, catalog).validate(&renamed);
        let warnings = report
            .warnings()
            .filter(|finding| finding.rule == Rule::TelemetryContract)
            .collect::<Vec<_>>();
        assert_eq!(warnings.len(), 1, "{}", report);
        assert!(
            warnings[0].message.contains("filterlist.size"),
            "{}",
            warnings[0].message
        );
    }
}

/// A node of the default program that does nothing as configured, which the detached export leaves out, is not in the
/// contract: a program written from that export is not warned about it.
#[tokio::test]
async fn the_contract_leaves_out_the_nodes_that_do_nothing_as_configured() {
    let plan = lower("").await;
    let nodes = of_default(&plan)
        .entries
        .into_iter()
        .map(|entry| entry.node.to_string())
        .collect::<BTreeSet<_>>();
    // With defaults, the filterlists have nothing in them.
    assert_eq!(
        nodes,
        ["dsd_agg", "dsd_no_agg_split"].into_iter().map(String::from).collect()
    );
    let full = contract(&plan, &OperatorRegistry::native(), &rules());
    assert!(full
        .entries
        .iter()
        .any(|entry| entry.node.to_string() == "dsd_prefix_filter"));
}

/// Under every configuration, each node of the contract the default program has runs the contract's operator and
/// reports all its series, and a node it lacks is one the configuration turns off, as it turns off the native
/// component: with DogStatsD off, the contract is empty.
#[tokio::test]
async fn the_default_program_under_every_configuration_meets_the_contract_of_every_node_it_has() {
    let registry = OperatorRegistry::native();
    let every_branch = contract(&lower(EVERY_BRANCH).await, &registry, &rules());
    for (name, yaml) in MATRIX {
        let plan = lower(yaml).await;
        let own = contract(&plan, &registry, &rules());
        let present = every_branch
            .entries
            .iter()
            .filter(|entry| plan.nodes.iter().any(|node| node.id == entry.node))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(own.entries, present, "{}", name);
        let dogstatsd = plan.nodes.iter().any(|node| node.id.to_string() == "dsd_agg");
        assert_eq!(own.entries.is_empty(), !dogstatsd, "{}: {:?}", name, own);
    }
}

#[tokio::test]
async fn a_program_without_a_node_of_the_contract_gets_one_warning_listing_what_it_loses() {
    let registry = OperatorRegistry::native();
    let default = lower(EVERY_BRANCH).await;
    let contract = contract(&default, &registry, &rules());

    // Renamed: the aggregation's series are absent.
    let mut renamed = default.clone();
    for node in &mut renamed.nodes {
        if node.id.to_string() == "dsd_agg" {
            node.id = "other_agg".parse().unwrap();
        }
    }
    let warnings = findings(&renamed, &contract);
    assert_eq!(warnings.len(), 1, "{:?}", warnings);
    assert_eq!(warnings[0].rule, Rule::TelemetryContract);
    assert_eq!(warnings[0].location, Location::Plan);
    for series in [
        "aggregator.flush",
        "aggregator.processed",
        "aggregator.dogstatsd_contexts_by_mtype",
    ] {
        assert!(warnings[0].message.contains(series), "{}", warnings[0].message);
    }

    // Another operator: the warning is at the node.
    let mut replaced = default.clone();
    for node in &mut replaced.nodes {
        if node.id.to_string() == "dsd_agg" {
            node.op = names::SPLIT.parse().unwrap();
        }
    }
    let warnings = findings(&replaced, &contract);
    assert_eq!(warnings.len(), 1, "{:?}", warnings);
    assert_eq!(
        warnings[0].location.node().map(ToString::to_string).as_deref(),
        Some("dsd_agg")
    );
}

/// The roles report the series the native filters they stand for register, by the native components' own names.
#[test]
fn roles_name_the_series_of_the_native_filters_they_stand_for() {
    let names = |name: &str| {
        roles::ROLES
            .iter()
            .find(|role| role.name == name)
            .unwrap()
            .series
            .iter()
            .map(|series| series.name)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names("dsd_prefix_filter"),
        [
            METRIC_FILTERLIST_SIZE_METRIC,
            METRIC_FILTERLIST_UPDATES_METRIC,
            LISTENER_FILTERED_POINTS_METRIC
        ]
    );
    assert_eq!(names("dsd_post_agg_filter"), [FILTERED_METRICS_METRIC]);
    assert_eq!(
        names("metrics_routing_filter_{i}"),
        [
            ALLOWLIST_METRICS_FILTERED_METRIC,
            ALLOWLIST_SIZE_METRIC,
            ALLOWLIST_SIZE_METRIC
        ]
    );
    // A role's series are recorded by a host: its operator describes what its kernel records.
    assert!(roles::ROLES
        .iter()
        .flat_map(|role| role.series)
        .all(|series| matches!(series.recorder, Recorder::Port(_) | Recorder::Plan(_))));
}

/// Every series a host records rather than a kernel names what it is computed from, on the nodes of the default
/// program that report it: a port of the node, the names its predicate matches, or its parameters.
#[tokio::test]
async fn series_recorded_outside_kernels_name_ports_and_parameters_of_their_nodes() {
    let plans = [
        lower(EVERY_BRANCH).await,
        Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap(),
    ];
    let mut checked = BTreeSet::new();
    for plan in &plans {
        for node in &plan.nodes {
            for series in reported(&node.id, node.op.as_str()) {
                let what = format!("`{}` of `{}`", series.name, node.id);
                match series.recorder {
                    Recorder::Kernel | Recorder::Cache | Recorder::RegionInput => continue,
                    Recorder::Port(port) => assert!(
                        plan.edges
                            .iter()
                            .any(|edge| edge.from.node == node.id && edge.from.port.as_str() == port),
                        "{}: no port `{}`",
                        what,
                        port
                    ),
                    Recorder::Plan(PlanValue::ListLength | PlanValue::MatcherEntries { .. }) => {
                        assert_eq!(node.op.as_str(), names::SPLIT, "{}: not a `split`", what);
                        assert!(node.params.get("predicate").is_some(), "{}: no predicate", what);
                    }
                    Recorder::Plan(PlanValue::TagProjectionRules) => {
                        assert_eq!(node.op.as_str(), names::TAG_PROJECTION, "{}", what);
                        assert!(
                            node.params.get("rules").is_some() && node.params.get("value_rules").is_some(),
                            "{}: no rules",
                            what
                        );
                    }
                    Recorder::Plan(PlanValue::ParamChanges) => {
                        assert!(node.params.iter().next().is_some(), "{}: no parameters", what);
                    }
                }
                checked.insert(roles::of(&node.id, node.op.as_str()).unwrap().name);
            }
        }
    }
    let every = roles::ROLES.iter().map(|role| role.name).collect::<BTreeSet<_>>();
    assert_eq!(checked, every, "every role was checked");
}

#[test]
fn roles_are_keyed_by_name_and_operator() {
    let role = |name: &str, operator| roles::of(&name.parse().unwrap(), operator).map(|role| role.name);
    assert_eq!(role("dsd_prefix_filter", names::SPLIT), Some("dsd_prefix_filter"));
    assert_eq!(
        role("metrics_routing_filter_12", names::SPLIT),
        Some("metrics_routing_filter_{i}")
    );
    assert_eq!(role("metrics_routing_filter_01", names::SPLIT), None);
    assert_eq!(role("dsd_prefix_filter", names::TAG_PROJECTION), None);
    assert_eq!(role("policy/dsd_post_agg_filter", names::SPLIT), None);
    assert_eq!(
        roles::of(&"dsd_tag_filterlist".parse().unwrap(), names::TAG_PROJECTION).and_then(|role| role.cache_id),
        Some("tag_filterlist/context_cache")
    );
}
