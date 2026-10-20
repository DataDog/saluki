use std::{collections::BTreeSet, sync::Arc};

use saluki_core::observability::metrics::RemapperRule;
use saluki_metric_plan::{
    ir::Plan,
    operators::{names, OperatorRegistry, PlanValue, Recorder, SeriesKind},
    testing::DEFAULT_PROGRAM_JSON,
    validate::{contract::findings, Location, Rule},
};

use super::{
    contract::{claim, Scope, Unclaimed},
    *,
};
use crate::{
    programmable_metrics::{
        lowering::{
            matrix::{EVERY_BRANCH, MATRIX},
            Lowering,
        },
        telemetry_profiles::{native_telemetry, MetricSource, NativeMetricKind},
        test_support::{configuration, facts},
    },
    state::metrics::{get_compat_remappings, get_datadog_agent_remappings},
};

/// The components whose series the remapper reads that are the host's, not a node's: no program can remove them.
const HOST_COMPONENTS: &[&str] = &["dsd_in", "dd_events_encode", "dd_service_checks_encode"];

/// The nodes the remapper selects on that the default program does not have today, by name: lowering still calls the
/// prefix filter's `split` `dsd_prefix_filter_blocklist`, which reports as `dsd_prefix_filter` through its telemetry
/// profile. V10's lowering rename names it `dsd_prefix_filter`, and this list empties.
const NOT_YET_NAMED: &[&str] = &["dsd_prefix_filter"];

fn rules() -> Vec<RemapperRule> {
    let mut rules = get_datadog_agent_remappings();
    rules.extend(get_compat_remappings());
    rules
}

async fn lower(yaml: &str) -> Plan {
    let config = configuration(yaml).await;
    Lowering::new(Arc::new(config.clone()), facts(&config))
        .unwrap()
        .unwrap()
        .lower(&config)
        .unwrap()
        .plan
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

        // Every rule scoped to a node is met, but those whose node lowering does not name yet.
        let unclaimed = unclaimed_by_nodes(&plan);
        assert!(!unclaimed.is_empty());
        for unclaimed in unclaimed {
            assert!(
                matches!(unclaimed.scope, Scope::Component(component) if NOT_YET_NAMED.contains(&component)),
                "`{}` ({:?}) is read from no node of the default program",
                unclaimed.series,
                unclaimed.scope
            );
        }
    }
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

/// The node each telemetry profile is for today, its operator, and the name its role has.
const PROFILED: &[(&str, &str, &str)] = &[
    ("dsd_prefix_filter_blocklist", names::SPLIT, "dsd_prefix_filter"),
    ("dsd_post_agg_filter", names::SPLIT, "dsd_post_agg_filter"),
    ("metrics_routing_filter_1", names::SPLIT, "metrics_routing_filter_1"),
    ("dsd_tag_filterlist", names::TAG_PROJECTION, "dsd_tag_filterlist"),
    ("dsd_no_agg_split", names::TIMESTAMP_SPLIT, "dsd_no_agg_split"),
    ("dsd_agg", names::AGGREGATE, "dsd_agg"),
];

/// What operators and roles describe is what the telemetry profiles they replace keep: the same series, with the same
/// recorders, but those outside a profile (a cache's, and what enters a region).
#[test]
fn operators_and_roles_describe_what_the_telemetry_profiles_keep() {
    let registry = OperatorRegistry::native();
    for (today, operator, name) in PROFILED {
        let profile = native_telemetry(&today.parse().unwrap(), operator)
            .unwrap_or_else(|| panic!("no profile for `{}`", today))
            .profile;
        let kept = profile
            .metrics()
            .iter()
            .map(|metric| {
                let kind = match metric.kind {
                    NativeMetricKind::Counter => SeriesKind::Counter,
                    NativeMetricKind::Gauge => SeriesKind::Gauge,
                };
                let recorder = match metric.source {
                    MetricSource::Kernel => Recorder::Kernel,
                    MetricSource::PortEvents(port) => Recorder::Port(port),
                    MetricSource::ListLength => Recorder::Plan(PlanValue::ListLength),
                    MetricSource::MatcherEntries { prefixes } => Recorder::Plan(PlanValue::MatcherEntries { prefixes }),
                    MetricSource::ParamChanges => Recorder::Plan(PlanValue::ParamChanges),
                    MetricSource::TagProjectionRules => Recorder::Plan(PlanValue::TagProjectionRules),
                };
                (metric.name, kind, metric.tags, recorder)
            })
            .collect::<BTreeSet<_>>();
        let role = roles::of(&name.parse().unwrap(), operator);
        let described = registry
            .get(operator)
            .unwrap()
            .reports()
            .iter()
            .chain(role.map(|role| role.series).unwrap_or(&[]))
            .filter(|series| !matches!(series.recorder, Recorder::Cache | Recorder::RegionInput))
            .map(|series| (series.name, series.kind, series.tags, series.recorder))
            .collect::<BTreeSet<_>>();
        assert_eq!(described, kept, "`{}`", today);
    }
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
