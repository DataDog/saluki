use std::num::NonZeroU64;

use saluki_core::{
    data_model::event::metric::{context::Context, Metric, ScalarPoints},
    execution::AdmissionTime,
};
use serde_json::{json, Value};

use super::*;
use crate::{
    equivalence::{compare, OutputPoints},
    interpret::{InstallOutcome, Interpreter, Trace},
    testing::{default_program_catalog, DEFAULT_PROGRAM_JSON},
};

fn id(value: &str) -> NodeId {
    value.parse().unwrap()
}

/// Leaves out what does nothing: the host's tags and failover's switches are a fact and signals, which stay bound.
fn without_facts(plan: &Plan) -> Eliminated {
    eliminate(plan, &OperatorRegistry::native())
}

fn omitted(eliminated: &Eliminated) -> Vec<(String, Omission)> {
    eliminated
        .omitted
        .iter()
        .map(|(node, why)| (node.to_string(), *why))
        .collect()
}

/// Sets a parameter of the default-program fixture: at `path` (member names and indexes, from the node's parameters),
/// the value `value`, which replaces a literal or a binding's resolved value.
fn resolve(plan: &mut Value, node: &str, path: &[&str], value: Value) {
    let node = plan["nodes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|candidate| candidate["id"] == node)
        .unwrap_or_else(|| panic!("no node `{}`", node));
    let mut at = &mut node["params"];
    for step in path {
        at = match step.parse::<usize>() {
            Ok(index) => &mut at[index],
            Err(_) => &mut at[*step],
        };
    }
    match at.get_mut("resolved") {
        Some(resolved) => *resolved = value,
        None => *at = value,
    }
}

/// The default-program fixture with nothing configured for DogStatsD processing: no mapper profiles, no namespace, an
/// empty metric filterlist, no tag rules, no histogram statistic filterlisted, and the first endpoint routing group
/// with neither names nor prefixes, each predicate in normal form. Aggregation, the timestamp split, and the policy are
/// as the fixture has them.
fn unconfigured() -> Value {
    let mut plan = serde_json::from_str::<Value>(DEFAULT_PROGRAM_JSON).unwrap();
    resolve(&mut plan, "dsd_enrich", &["profiles"], json!([]));
    resolve(
        &mut plan,
        "dsd_prefix_filter_blocklist_namespace_prefix",
        &["namespace"],
        json!(""),
    );
    // A filterlist with nothing in it keeps everything.
    resolve(
        &mut plan,
        "dsd_prefix_filter_blocklist",
        &["predicate"],
        json!({"const": true}),
    );
    resolve(&mut plan, "dsd_tag_filterlist", &["rules"], json!([]));
    resolve(&mut plan, "dsd_tag_filterlist", &["value_rules"], json!([]));
    resolve(&mut plan, "dsd_post_agg_filter", &["predicate"], json!({"const": true}));
    // A routing group with neither names nor prefixes selects nothing.
    resolve(
        &mut plan,
        "metrics_routing_filter_0",
        &["predicate"],
        json!({"const": false}),
    );
    plan
}

#[test]
fn a_configured_default_program_loses_only_host_enrichment_which_changes_no_metric() {
    let plan = Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap();
    let eliminated = without_facts(&plan);
    assert_eq!(
        omitted(&eliminated),
        [("metrics_host_enrichment".to_string(), Omission::Identity)]
    );
    // The merge in front of it now feeds the host tags directly.
    assert!(eliminated
        .plan
        .edges
        .iter()
        .any(|edge| edge.from.to_string() == "metrics_in.out" && edge.to.to_string() == "metrics.in"));
}

#[test]
fn an_unconfigured_default_program_loses_every_native_feature_that_does_nothing_and_nothing_else() {
    let plan = Plan::from_value(unconfigured()).unwrap();
    let eliminated = without_facts(&plan);
    assert_eq!(
        omitted(&eliminated),
        [
            // Constant splits first, with what only their dead ports reach, in plan order.
            ("dsd_prefix_filter_blocklist", Omission::ConstantSplit),
            ("dsd_prefix_filter_blocklist_false", Omission::Unreachable),
            ("dsd_post_agg_filter", Omission::ConstantSplit),
            ("dsd_post_agg_filter_false", Omission::Unreachable),
            // An endpoint routing group with neither names nor prefixes forwards nothing: its `emit` goes, and its
            // output stays declared.
            ("metrics_routing_filter_0", Omission::ConstantSplit),
            ("metrics_routing_filter_0_true", Omission::Unreachable),
            // Then the nodes that pass every record through unchanged.
            ("dsd_enrich", Omission::Identity),
            ("dsd_prefix_filter_blocklist_namespace_prefix", Omission::Identity),
            ("dsd_tag_filterlist", Omission::Identity),
            ("metrics_host_enrichment", Omission::Identity),
        ]
        .map(|(node, why)| (node.to_string(), why))
    );
    assert_eq!(eliminated.plan.outputs, plan.outputs);
    let edges = eliminated
        .plan
        .edges
        .iter()
        .map(|edge| format!("{} -> {}", edge.from, edge.to))
        .collect::<Vec<_>>();
    for edge in [
        "dsd_in.metrics -> dsd_no_agg_split.in",
        "dsd_no_agg_split.default -> dsd_agg.in",
        "dsd_post_agg_filter_in.out -> dsd_post_agg_filter_fanout.in",
        "metrics_in.out -> metrics.in",
        "metrics_fanout.1 -> metrics_routing_filter_0_false.in",
    ] {
        assert!(edges.contains(&edge.to_string()), "no edge {}: {:?}", edge, edges);
    }

    // The host's tags are a fact, and failover's switches a fact and a signal, which stay bound: they decide nothing,
    // whatever they resolve to.
    let mut plan = unconfigured();
    resolve(&mut plan, "metrics", &["tags"], json!([]));
    resolve(
        &mut plan,
        "mrf_metrics_gateway",
        &["predicate", "all", "0", "flag"],
        json!(false),
    );
    let plan = Plan::from_value(plan).unwrap();
    let kept = without_facts(&plan);
    for node in ["metrics", "mrf_metrics_gateway", "mrf_metrics_gateway_true"] {
        assert!(kept.plan.node(&id(node)).is_some(), "{} was left out", node);
    }
    // Aggregation, the timestamp split, the client telemetry tap, and the sources stay in every case.
    for eliminated in [&eliminated, &kept] {
        for node in [
            "dsd_in",
            "dsd_no_agg_split",
            "dsd_agg",
            "dsd_post_agg_filter_in",
            "dsd_post_agg_filter_fanout",
            "dsd_post_agg_filter_fanout_0",
            "metrics_fanout",
        ] {
            assert!(eliminated.plan.node(&id(node)).is_some(), "{} was left out", node);
        }
    }
}

#[test]
fn a_constant_split_whose_dead_port_reaches_a_state_owner_stays() {
    // The filterlist written to drop everything, a split that keeps nothing on its `true` port: what it no longer
    // forwards includes aggregation, which R3's obligations never remove, so the split stays, and so does everything
    // after it.
    let mut plan = unconfigured();
    let blocklist = plan["nodes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|node| node["id"] == "dsd_prefix_filter_blocklist")
        .unwrap();
    blocklist["params"]["predicate"] = json!({"const": false});
    let plan = Plan::from_value(plan).unwrap();
    let eliminated = without_facts(&plan);
    for node in ["dsd_prefix_filter_blocklist", "dsd_no_agg_split", "dsd_agg"] {
        assert!(eliminated.plan.node(&id(node)).is_some(), "{} was left out", node);
    }
}

/// Builds a small plan from nodes and edges written as `from.port -> to.port`, delivering to the output `o0`.
fn small_plan(nodes: Value, edges: &[&str]) -> Plan {
    let edges = edges
        .iter()
        .map(|edge| {
            let (from, to) = edge.split_once(" -> ").unwrap();
            json!({"from": from, "to": to})
        })
        .collect::<Vec<_>>();
    Plan::from_value(json!({
        "ir_version": 1,
        "nodes": nodes,
        "edges": edges,
        "outputs": [{"id": "o0", "accepts": {"kinds": "all", "timestamps": "mixed"},
                     "recipients": ["out/o0"]}],
    }))
    .unwrap()
}

#[test]
fn a_merge_a_fold_leaves_with_one_input_goes_too() {
    let nodes = json!([
        {"id": "in", "op": "source", "params": {"binding": "otlp"}},
        {"id": "pick", "op": "split", "params": {"predicate": {"name": {"in": []}}}},
        {"id": "rejoin", "op": "merge", "params": {"inputs": 2}},
        {"id": "out", "op": "emit", "params": {"output": "o0"}},
    ]);
    let edges = [
        "in.metrics -> pick.in",
        "pick.true -> rejoin.0",
        "pick.false -> rejoin.1",
        "rejoin.out -> out.in",
    ];
    let plan = small_plan(nodes.clone(), &edges);
    let eliminated = eliminate(&plan, &OperatorRegistry::native());
    assert_eq!(
        omitted(&eliminated),
        [
            ("pick".to_string(), Omission::ConstantSplit),
            ("rejoin".to_string(), Omission::Identity)
        ]
    );
    assert_eq!(
        eliminated
            .plan
            .edges
            .iter()
            .map(|edge| format!("{} -> {}", edge.from, edge.to))
            .collect::<Vec<_>>(),
        ["in.metrics -> out.in"]
    );
}

fn t(relative: u64) -> AdmissionTime {
    AdmissionTime::from_unix_secs(1_700_000_000 + relative)
}

fn ts(relative: u64) -> Option<NonZeroU64> {
    NonZeroU64::new(1_700_000_000 + relative)
}

/// Every node left out of the unconfigured default program does nothing: the reference interpreter delivers exactly
/// the same points to every output with and without them, over records of every kind the plan's sources produce, with
/// and without timestamps, tags, origin tags, and a host, and with names that look filtered, prefixed, or mapped, or
/// that have no intake name.
#[test]
fn what_is_left_out_changes_nothing_the_reference_interpreter_delivers() {
    let mut value = unconfigured();
    // With no host tags, written as a literal, host tag enrichment does nothing too.
    let tags = value["nodes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|node| node["id"] == "metrics")
        .unwrap();
    tags["params"]["tags"] = json!([]);
    let plan = Plan::from_value(value).unwrap();
    let eliminated = without_facts(&plan);
    assert!(eliminated.omitted.contains(&(id("metrics"), Omission::Identity)));
    assert_eq!(eliminated.omitted.len(), 11);

    let dogstatsd = || {
        vec![
            Metric::counter(Context::from_static_parts("raw.requests.login", &["env:prod"]), 1.0),
            Metric::counter(Context::from_static_name("tenant.raw.blocked"), 1.0),
            Metric::distribution(
                Context::from_static_parts("checkout.latency", &["user_id:42", "env:dev"]),
                10.0,
            ),
            Metric::gauge(Context::from_static_name("internal.queue"), 7.0),
            Metric::histogram(
                Context::from_static_parts("checkout.latency", &["env:prod"]),
                [1.0, 2.0, 3.0, 4.0, 5.0],
            ),
            Metric::counter(
                Context::from_static_name("checkout.requests"),
                ScalarPoints::from_iter([(ts(1), 2.0)]),
            ),
            Metric::gauge(
                Context::from_static_name("system.load"),
                ScalarPoints::from_iter([(None, 1.0), (ts(2), 2.0)]),
            ),
            Metric::set(Context::from_static_parts("users", &["team:", "canary"]), "u1"),
            Metric::counter(Context::from_static_name("123"), 1.0),
        ]
    };
    let otlp = || {
        vec![
            Metric::gauge(Context::from_static_name("catalog.items"), (1_700_000_004, 3.0)),
            Metric::counter(Context::from_static_name("requests.count"), (1_700_000_004, 4.0)),
            Metric::counter(Context::from_static_name("checkout.requests"), (1_700_000_004, 1.0)),
        ]
    };
    let run = |plan: Plan| {
        let trace = Trace::new()
            .install(plan)
            .admit(id("dsd_in"), t(3), dogstatsd())
            .admit(id("otlp_in"), t(4), otlp())
            .advance(t(15))
            .admit(id("dsd_in"), t(302), dogstatsd())
            .advance(t(325));
        let catalog = default_program_catalog();
        let registry = OperatorRegistry::native();
        let run = Interpreter::new(&registry, &catalog).run(&trace).unwrap();
        assert!(
            matches!(run.installs()[0].outcome, InstallOutcome::Active { .. }),
            "{:?}",
            run.installs()[0].outcome
        );
        OutputPoints::from_run(&run)
    };
    let (before, after) = (run(plan), run(eliminated.plan));
    assert!(before.count(&"dd_metrics_encode".parse().unwrap()) > 0);
    compare(&before, &after).unwrap();
}
