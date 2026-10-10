use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use saluki_core::accounting::ComponentRegistry;
use saluki_metric_plan::{
    canonical::{PlanHash, ResolvedValues},
    compile::{compile, Frame, RewriteId},
    ir::Plan,
    operators::OperatorRegistry,
    validate::{Catalog, Liveness, ValidationReport, Validator},
};
use serde_json::{json, Value};

use super::{
    matrix::{matrix_entry, EVERY_BRANCH, MATRIX},
    *,
};
use crate::{
    cli::run::create_legacy_topology,
    config::remote_agent_client_configuration,
    programmable_metrics::{
        bindings,
        telemetry_profiles::native_telemetry,
        test_support::{
            configuration, environment_provider, facts, layered_configuration, layered_configuration_system,
        },
    },
};

/// Prepares lowering for a process started with `startup` and the host facts of such a process.
fn lowering(startup: &SalukiConfiguration) -> Lowering {
    Lowering::new(Arc::new(startup.clone()), facts(startup))
        .expect("the configuration lowers")
        .expect("the configuration has a metrics pipeline")
}

/// Lowers `startup` with the current configuration `current`, and the host facts of a process started with
/// `startup`.
fn lower_with(startup: &SalukiConfiguration, current: &SalukiConfiguration) -> Lowered {
    lowering(startup).lower(current).expect("the configuration lowers")
}

/// Returns the error preparing lowering for `config`, or else lowering it, reports.
fn lowering_error(config: &SalukiConfiguration, facts: HostFacts) -> GenericError {
    match Lowering::new(Arc::new(config.clone()), facts) {
        Err(error) => error,
        Ok(lowering) => lowering
            .expect("the configuration has a metrics pipeline")
            .lower(config)
            .expect_err("the configuration does not lower"),
    }
}

/// Lowers the configuration `yaml`, with no configuration change since startup.
async fn lowered(yaml: &str) -> Lowered {
    let config = configuration(yaml).await;
    lower_with(&config, &config)
}

fn validate(plan: &Plan, catalog: &dyn Catalog) -> ValidationReport {
    Validator::new(&OperatorRegistry::native(), catalog).validate(plan)
}

/// Returns every configuration binding in a node's parameters: the parameter, the binding, and its resolved value.
fn bindings_of(node: &Node) -> Vec<(String, String, Value)> {
    fn visit(param: &str, value: &Value, found: &mut Vec<(String, String, Value)>) {
        match value {
            Value::Object(members) => match members.get("config") {
                Some(Value::String(binding)) => found.push((
                    param.to_string(),
                    binding.clone(),
                    members.get("resolved").cloned().unwrap_or(Value::Null),
                )),
                _ => members.values().for_each(|value| visit(param, value, found)),
            },
            Value::Array(values) => values.iter().for_each(|value| visit(param, value, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    for (param, value) in node.params.iter() {
        visit(param.as_str(), value, &mut found);
    }
    found
}

/// Returns the parameters, as `node.param`, that differ between two plans of the same structure.
#[track_caller]
fn changed_parameters(old: &Plan, new: &Plan) -> BTreeSet<String> {
    assert_eq!(
        old.hash(ResolvedValues::Exclude).unwrap(),
        new.hash(ResolvedValues::Exclude).unwrap(),
        "the structure is unchanged"
    );
    let mut changed = BTreeSet::new();
    for (old, new) in old.nodes.iter().zip(&new.nodes) {
        for (param, value) in old.params.iter() {
            if new.params.get(param.as_str()) != Some(value) {
                changed.insert(format!("{}.{}", old.id, param));
            }
        }
    }
    changed
}

#[tokio::test]
async fn every_branch_lowers_to_the_designed_default_program() {
    let lowered = lowered(EVERY_BRANCH).await;

    // Fingerprints are digests of the documented binding documents, written here by hand.
    let documents = [
        (
            "dsd_client_telemetry",
            json!({"encoder": {"destination": "dsd_client_telemetry_out"}, "forwarder": null, "recipients": []}),
        ),
        (
            "dd_metrics_encode",
            json!({
                "encoder": {"type": "datadog_metrics",
                            "endpoint_routing": {"all_except": ["https://b.example.com", "https://c.example.com"]}},
                "forwarder": "dd_out",
                "recipients": ["dd_out/primary/0/https://primary.example.com"],
            }),
        ),
        (
            "metrics_routing_encode_0",
            json!({
                "encoder": {"type": "datadog_metrics", "endpoint_routing": {"only": ["https://c.example.com"]}},
                "forwarder": "dd_out",
                "recipients": ["dd_out/additional/0/https://c.example.com"],
            }),
        ),
        (
            "metrics_routing_encode_1",
            json!({
                "encoder": {"type": "datadog_metrics", "endpoint_routing": {"only": ["https://b.example.com"]}},
                "forwarder": "dd_out",
                "recipients": [
                    "dd_out/additional/0/https://b.example.com",
                    "dd_out/additional/1/https://b.example.com",
                ],
            }),
        ),
        (
            "mrf_metrics_encode",
            json!({
                "encoder": {"type": "datadog_metrics", "metrics_endpoint_override": "https://mrf.example.com"},
                "forwarder": "mrf_dd_out",
                "recipients": ["mrf_dd_out/primary/0/https://mrf.example.com"],
            }),
        ),
        (
            "af_metrics_encode",
            json!({
                "encoder": {"type": "datadog_metrics", "v2_series_only": true},
                "forwarder": "cluster_agent_out",
                "recipients": ["cluster_agent_out/primary/0/https://cluster-agent.example.com:5005"],
            }),
        ),
    ];
    let mut plan = lowered.plan.clone();
    assert_eq!(plan.outputs.len(), documents.len());
    for (output, (id, document)) in plan.outputs.iter_mut().zip(documents) {
        assert_eq!(output.id.as_str(), id);
        assert_eq!(output.fingerprint, PlanHash::of_value(&document).unwrap(), "{}", id);
        output.fingerprint = format!("sha256:{}", "0".repeat(64)).parse().unwrap();
    }

    let expected = Plan::from_json(include_str!("fixtures/every-branch.json")).unwrap();
    let ids = |plan: &Plan| plan.nodes.iter().map(|node| node.id.to_string()).collect::<Vec<_>>();
    assert_eq!(ids(&plan), ids(&expected));
    for (actual, expected) in plan.nodes.iter().zip(&expected.nodes) {
        assert_eq!(actual, expected, "node `{}`", expected.id);
    }
    assert_eq!(plan.edges, expected.edges);
    assert_eq!(plan.outputs, expected.outputs);
    assert_eq!(plan.scopes, expected.scopes);
    assert_eq!(plan, expected);
    assert!(lowered.notes.is_empty());
}

#[tokio::test]
async fn every_configuration_of_the_matrix_lowers_to_a_valid_plan() {
    for (name, yaml) in MATRIX {
        let lowered = lowered(yaml).await;
        let report = validate(&lowered.plan, &lowered.catalog);
        assert!(report.findings().is_empty(), "{}:\n{}", name, report);

        // The catalog's document form, as an export carries it, validates the plan alike.
        let document = lowered.catalog.to_document();
        assert!(validate(&lowered.plan, &document).findings().is_empty(), "{}", name);

        // The plan reads back as written, and lowering the same configuration again writes the same plan.
        let json = serde_json::to_string(&lowered.plan).unwrap();
        assert_eq!(Plan::from_json(&json).unwrap(), lowered.plan, "{}", name);
        let again = self::lowered(yaml).await;
        assert_eq!(
            again.plan.hash(ResolvedValues::Include).unwrap(),
            lowered.plan.hash(ResolvedValues::Include).unwrap(),
            "{}",
            name
        );

        // Every node is reserved, stands for a native component, and has that component's telemetry identity.
        for node in &lowered.plan.nodes {
            let Some(Provenance::Native(component)) = &node.provenance else {
                panic!("{}: node `{}` has no native provenance", name, node.id);
            };
            let telemetry = native_telemetry(&node.id, node.op.as_str())
                .unwrap_or_else(|| panic!("{}: node `{}` has no telemetry profile", name, node.id));
            assert_eq!(telemetry.component_id, component.to_string(), "{}: `{}`", name, node.id);
        }
    }
}

#[tokio::test]
async fn no_configuration_of_the_matrix_gives_r3_or_r7_anything_to_rewrite() {
    // Lowering writes no constant `split`, and every junction in R7's static order (operands cheapest first, ties by
    // canonical form, so by binding name), so compiling a lowered default program applies neither logical rewrite:
    // explain and the rewrite telemetry show neither. A lowering whose order drifts would start reordering in
    // production. (Decision compilation still folds bound constants, such as a Multi-Region Failover gateway whose
    // mirroring is off, inside the compiled programs: R3 records of the decisions, not of the plan.)
    let registry = OperatorRegistry::native();
    for (name, yaml) in MATRIX {
        let lowered = lowered(yaml).await;
        let report = validate(&lowered.plan, &lowered.catalog);
        assert!(report.is_valid(), "{}:\n{}", name, report);
        let frame = Frame::compute(
            &lowered.plan,
            &registry,
            &crate::programmable_metrics::topology::slot_name,
        )
        .unwrap_or_else(|error| panic!("{}: {}", name, error));
        let compiled =
            compile(&lowered.plan, &report, &registry, &frame).unwrap_or_else(|error| panic!("{}: {}", name, error));
        assert!(compiled.converged, "{}", name);
        assert_eq!(*compiled.plan, lowered.plan, "{}", name);
        let records = compiled
            .logical_rewrites()
            .iter()
            .filter(|record| matches!(record.id, RewriteId::R3 | RewriteId::R7))
            .collect::<Vec<_>>();
        assert!(records.is_empty(), "{}: {:#?}", name, records);
    }
}

/// The nodes whose presence depends on configuration, for each configuration of the matrix, as `run.rs` wires their
/// components.
#[tokio::test]
async fn branches_exist_only_when_native_wiring_builds_them() {
    const OPTIONAL: &[&str] = &[
        "dsd_in",
        "dsd_no_agg_split",
        "dsd_post_agg_filter_in",
        "otlp_in",
        "checks_ipc_in",
        "liveness_in",
        "trace_sampler",
        "metrics_enrich_host_tags",
        "metrics_routing_filter_0",
        "metrics_routing_filter_1",
        "metrics_routing_filter_2",
        "mrf_metrics_gateway",
        "af_metrics_gateway",
    ];
    let dsd = ["dsd_in", "dsd_no_agg_split", "dsd_post_agg_filter_in"];
    let expected: BTreeMap<&str, Vec<&str>> = [
        ("dogstatsd with defaults", dsd.to_vec()),
        ("no-aggregation pipeline off", vec!["dsd_in"]),
        // Standalone OTLP requires the traces pipeline, whose sampler reports metrics.
        ("otlp without dogstatsd", vec!["otlp_in", "trace_sampler"]),
        // In proxy mode, OTLP metrics and traces bypass the plan, and only liveness feeds it.
        ("otlp proxy mode, connected", vec!["liveness_in"]),
        // In proxy mode with traces not proxied, OTLP metrics bypass the plan, but the traces pipeline runs, and its
        // sampler reports metrics.
        ("otlp proxy mode with traces not proxied", vec!["trace_sampler"]),
        ("checks", [&dsd[..], &["checks_ipc_in"]].concat()),
        ("checks without dogstatsd", vec!["checks_ipc_in"]),
        ("connected without host tags", [&dsd[..], &["liveness_in"]].concat()),
        (
            "connected with host tags",
            [&dsd[..], &["liveness_in", "metrics_enrich_host_tags"]].concat(),
        ),
        ("one routing group", [&dsd[..], &["metrics_routing_filter_0"]].concat()),
        // b and c share a group; d has empty lists and gets none.
        (
            "two routing groups, a shared group, and an endpoint with empty lists",
            [&dsd[..], &["metrics_routing_filter_0", "metrics_routing_filter_1"]].concat(),
        ),
        (
            "a routing policy on the primary endpoint",
            [&dsd[..], &["metrics_routing_filter_0"]].concat(),
        ),
        ("multi-region failover", [&dsd[..], &["mrf_metrics_gateway"]].concat()),
        (
            "multi-region failover mirroring an allowlist",
            [&dsd[..], &["mrf_metrics_gateway"]].concat(),
        ),
        ("multi-region failover without an api key", dsd.to_vec()),
        ("autoscaling failover", [&dsd[..], &["af_metrics_gateway"]].concat()),
        ("autoscaling failover without a cluster agent", dsd.to_vec()),
        ("a cluster agent without autoscaling failover", dsd.to_vec()),
        ("mapper profiles, a namespace prefix, and filterlists", dsd.to_vec()),
        ("the legacy metric blocklist", dsd.to_vec()),
        ("a metrics override intake", dsd.to_vec()),
        (
            "the primary endpoint repeated as an additional endpoint",
            [&dsd[..], &["metrics_routing_filter_0"]].concat(),
        ),
        ("repeated histogram statistics", dsd.to_vec()),
        ("aggregation settings", dsd.to_vec()),
        (
            "every branch",
            [
                &dsd[..],
                &[
                    "otlp_in",
                    "checks_ipc_in",
                    "liveness_in",
                    "trace_sampler",
                    "metrics_enrich_host_tags",
                    "metrics_routing_filter_0",
                    "metrics_routing_filter_1",
                    "mrf_metrics_gateway",
                    "af_metrics_gateway",
                ],
            ]
            .concat(),
        ),
    ]
    .into_iter()
    .collect();
    assert_eq!(expected.len(), MATRIX.len(), "every configuration has an expectation");

    for (name, yaml) in MATRIX {
        let lowered = lowered(yaml).await;
        let present = OPTIONAL
            .iter()
            .copied()
            .filter(|id| lowered.plan.node(&id.parse().unwrap()).is_some())
            .collect::<Vec<_>>();
        assert_eq!(present, expected[name], "{}", name);

        // The DogStatsD nodes that are always there with DogStatsD, and the enrichment and policy nodes that always
        // are.
        let has = |id: &str| lowered.plan.node(&id.parse().unwrap()).is_some();
        let dogstatsd = present.contains(&"dsd_in");
        for id in [
            "dsd_enrich",
            "dsd_prefix_filter",
            "dsd_prefix_filter_blocklist",
            "dsd_tag_filterlist",
            "dsd_agg",
            "dsd_post_agg_filter",
            "dsd_post_agg_filter_fanout",
        ] {
            assert_eq!(has(id), dogstatsd, "{}: `{}`", name, id);
        }
        for id in [
            "metrics_enrich_in",
            "metrics_enrich",
            "metrics_policy",
            "metrics_policy_0",
        ] {
            assert!(has(id), "{}: `{}`", name, id);
        }
    }
}

#[tokio::test]
async fn configurations_without_a_metrics_pipeline_lower_to_nothing() {
    for yaml in [
        // Standalone OTLP proxy mode, with traces proxied too.
        "data_plane: {dogstatsd: {enabled: false}, otlp: {enabled: true, proxy: {enabled: true}}}",
        // No data pipeline at all.
        "data_plane: {dogstatsd: {enabled: false}}",
    ] {
        let config = configuration(yaml).await;
        assert!(
            Lowering::new(Arc::new(config.clone()), facts(&config))
                .unwrap()
                .is_none(),
            "{}",
            yaml
        );
    }
}

#[tokio::test]
async fn live_inputs_change_only_the_parameters_bound_to_them() {
    let startup = configuration(EVERY_BRANCH).await;
    let before = lower_with(&startup, &startup);

    let cases: [(&str, &str, &[&str]); 7] = [
        // A filterlist entry that names no histogram statistic changes the listener filterlist only.
        (
            "an entry for the listener filterlist",
            "metric_filterlist: [tenant.raw.blocked, tenant.checkout.latency.max, tenant.other]",
            &["dsd_prefix_filter_blocklist.predicate"],
        ),
        // One that names a histogram statistic's series reaches the post-aggregation filter too.
        (
            "an entry naming a histogram statistic",
            "metric_filterlist: [tenant.raw.blocked, tenant.checkout.latency.max, tenant.checkout.latency.count]",
            &["dsd_prefix_filter_blocklist.predicate", "dsd_post_agg_filter.predicate"],
        ),
        (
            "the filterlist match mode",
            "metric_filterlist_match_prefix: true",
            &["dsd_prefix_filter_blocklist.predicate", "dsd_post_agg_filter.predicate"],
        ),
        (
            "the tag filterlist",
            "metric_tag_filterlist: [{metric_name: tenant.checkout.latency, action: include, tags: [env]}]",
            &["dsd_tag_filterlist.rules"],
        ),
        // Metric mirroring and its allowlist reach the multi-region failover gateway only.
        (
            "metric mirroring",
            "multi_region_failover: {failover_metrics: true}",
            &["mrf_metrics_gateway.predicate"],
        ),
        (
            "the mirroring allowlist",
            "multi_region_failover: {metric_allowlist: [requests.count]}",
            &["mrf_metrics_gateway.predicate"],
        ),
        (
            "metric mirroring and its allowlist",
            "multi_region_failover: {failover_metrics: true, metric_allowlist: [requests.count]}",
            &["mrf_metrics_gateway.predicate"],
        ),
    ];
    for (name, change, expected) in cases {
        let current = layered_configuration(&[EVERY_BRANCH, change]).await;
        let after = lower_with(&startup, &current);
        assert_eq!(
            changed_parameters(&before.plan, &after.plan),
            expected.iter().map(|param| param.to_string()).collect(),
            "{}",
            name
        );
        assert!(validate(&after.plan, &after.catalog).findings().is_empty(), "{}", name);
    }

    // The gateway's predicate carries the new values, and so does the catalog.
    let current = layered_configuration(&[
        EVERY_BRANCH,
        "multi_region_failover: {failover_metrics: true, metric_allowlist: [requests.count]}",
    ])
    .await;
    let after = lower_with(&startup, &current);
    let gateway = after.plan.node(&ids::MRF_METRICS_GATEWAY.parse().unwrap()).unwrap();
    assert_eq!(
        bindings_of(gateway),
        [
            ("predicate", "multi_region_failover.enabled", json!(true)),
            (
                "predicate",
                "multi_region_failover.metric_mirroring.enabled",
                json!(true)
            ),
            (
                "predicate",
                "multi_region_failover.metric_mirroring.allowlist",
                json!(["requests.count"])
            ),
            (
                "predicate",
                "multi_region_failover.metric_mirroring.allowlist",
                json!(["requests.count"])
            ),
        ]
        .map(|(param, binding, value)| (param.to_string(), binding.to_string(), value))
    );
    let value = |lowered: &Lowered, name: &str| {
        lowered
            .catalog
            .binding(&name.parse().unwrap())
            .and_then(|binding| binding.value.clone())
    };
    assert_eq!(
        value(&before, "multi_region_failover.metric_mirroring.enabled"),
        Some(json!(false))
    );
    assert_eq!(
        value(&after, "multi_region_failover.metric_mirroring.enabled"),
        Some(json!(true))
    );
    assert_eq!(
        value(&after, "multi_region_failover.metric_mirroring.allowlist"),
        Some(json!(["requests.count"]))
    );
}

#[tokio::test]
async fn startup_inputs_do_not_change_a_re_lowered_plan() {
    let startup = configuration(EVERY_BRANCH).await;
    let before = lower_with(&startup, &startup);
    for change in [
        "statsd_metric_namespace: other",
        "statsd_metric_namespace_blacklist: []",
        "dogstatsd_mapper_profiles: []",
        "dogstatsd_mapper_cache_size: 5",
        "metric_tag_value_allowlist: []",
        "data_plane: {dogstatsd: {aggregator_tag_filter_cache_capacity: 7}}",
        "dogstatsd_no_aggregation_pipeline: false",
        "aggregate_window_duration_seconds: 60",
        "aggregate_context_limit: 5",
        "dogstatsd_expiry_seconds: 0",
        "histogram_aggregates: [min]",
        "histogram_percentiles: ['0.5']",
        "experimental: {metrics_endpoint_routing: {metric_prefix_allowlist: {https://c.example.com: [other.]}}}",
        "multi_region_failover: {enabled: false}",
        "autoscaling: {failover: {metrics: [other.metric]}}",
        "expected_tags_duration: 1h",
        "data_plane: {checks: {enabled: false}}",
    ] {
        let current = layered_configuration(&[EVERY_BRANCH, change]).await;
        assert_ne!(current, startup, "{} changes the configuration", change);
        let after = lower_with(&startup, &current);
        assert_eq!(after.plan, before.plan, "{}", change);
        for spec in bindings::BINDINGS
            .iter()
            .filter(|spec| spec.liveness == Liveness::Startup)
        {
            for key in spec
                .key
                .map(|key| key.keys(&startup))
                .unwrap_or_else(|| vec![String::new()])
            {
                let name = spec.instance_name(&key).parse().unwrap();
                assert_eq!(
                    after.catalog.binding(&name),
                    before.catalog.binding(&name),
                    "{}: `{}`",
                    change,
                    name
                );
            }
        }
    }
}

#[tokio::test]
async fn live_bindings_are_only_on_components_that_read_live_views_natively() {
    // The components the legacy metrics path gives live views of the metrics path's configuration, other than the
    // forwarders.
    let live_components = [
        "dsd_prefix_filter",
        "dsd_post_agg_filter",
        "dsd_tag_filterlist",
        "mrf_metrics_gateway",
    ];
    for (name, yaml) in MATRIX {
        let lowered = lowered(yaml).await;
        for node in &lowered.plan.nodes {
            for (param, binding, _) in bindings_of(node) {
                let catalog = lowered.catalog.binding(&binding.parse().unwrap()).unwrap();
                if catalog.liveness == Liveness::Live {
                    let Some(Provenance::Native(component)) = &node.provenance else {
                        unreachable!("every lowered node has native provenance");
                    };
                    assert!(
                        live_components.contains(&component.to_string().as_str()),
                        "{}: `{}.{}` is bound to the live `{}`",
                        name,
                        node.id,
                        param,
                        binding
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn parameters_are_native_configuration_converted_into_plan_values() {
    // (configuration, node, parameter, binding, resolved value), each derived by hand from the configuration.
    let mapper = matrix_entry("mapper profiles, a namespace prefix, and filterlists");
    let aggregation = matrix_entry("aggregation settings");
    let legacy_blocklist = matrix_entry("the legacy metric blocklist");
    let routing_groups = matrix_entry("two routing groups, a shared group, and an endpoint with empty lists");
    let multi_region_failover = matrix_entry("multi-region failover");
    let mirroring = matrix_entry("multi-region failover mirroring an allowlist");
    let autoscaling_failover = matrix_entry("autoscaling failover");
    // The keys of that configuration's routing groups: the first 16 hexadecimal digits of the SHA-256 of
    // `["https://e.example.com"]` and of `["https://b.example.com","https://c.example.com"]`, by `shasum -a 256`.
    let (group_e, group_b_c) = ("ce7249fa1ea667a9", "e3488d4119bf8279");
    let group = |key: &str, list: &str| format!("metrics_endpoint_routing.group.{}.{}", key, list);
    let (e_names, e_prefixes) = (group(group_e, "names"), group(group_e, "prefixes"));
    let (b_c_names, b_c_prefixes) = (group(group_b_c, "names"), group(group_b_c, "prefixes"));
    let cases: Vec<(&str, &str, &str, &str, Value)> = vec![
        (
            mapper,
            "dsd_enrich",
            "profiles",
            "dogstatsd.mapper.profiles",
            // An empty match type is wildcard, and tags are written in canonical order.
            json!([
                {"name": "wildcards", "prefix": "raw.", "mappings": [
                    {"match": "raw.requests.*", "match_type": "wildcard", "name": "mapped.requests",
                     "tags": [{"name": "kind", "value": "request"}, {"name": "route", "value": "$1"}]},
                    {"match": "raw.errors.*", "match_type": "wildcard", "name": "mapped.errors"}
                ]},
                {"name": "regexes", "prefix": "re.", "mappings": [
                    {"match": "re\\.(\\w+)\\.latency", "match_type": "regex", "name": "latency",
                     "tags": [{"name": "service", "value": "$1"}]}
                ]}
            ]),
        ),
        (
            mapper,
            "dsd_enrich",
            "cache_size",
            "dogstatsd.mapper.cache_size",
            json!(0),
        ),
        (
            mapper,
            "dsd_prefix_filter",
            "namespace",
            "dogstatsd.prefix_filter.metric_namespace",
            json!("tenant."),
        ),
        (
            mapper,
            "dsd_prefix_filter",
            "exclusions",
            "dogstatsd.prefix_filter.metric_namespace_blocklist",
            json!(["internal."]),
        ),
        (
            mapper,
            "dsd_tag_filterlist",
            "rules",
            "dogstatsd.tag_filterlist",
            // An entry without an action excludes.
            json!([
                {"metric_name": "tenant.requests", "action": "include", "tags": ["env"]},
                {"metric_name": "tenant.latency", "action": "exclude", "tags": ["user_id"]}
            ]),
        ),
        (
            mapper,
            "dsd_tag_filterlist",
            "value_rules",
            "dogstatsd.tag_value_allowlist",
            // Mismatches are removed by default, and a removing rule has no replacement.
            json!([{"metric_prefix": "tenant.", "tag_name": "env", "values": ["prod"], "on_miss": "remove"}]),
        ),
        (
            aggregation,
            "dsd_agg",
            "window",
            "dogstatsd.aggregation.window_duration_seconds",
            json!("1m"),
        ),
        (
            aggregation,
            "dsd_no_agg_split",
            "window",
            "dogstatsd.aggregation.window_duration_seconds",
            json!("1m"),
        ),
        (
            aggregation,
            "dsd_agg",
            "flush_interval",
            "dogstatsd.aggregation.flush_interval",
            json!("1500ms"),
        ),
        (
            aggregation,
            "dsd_agg",
            "context_limit",
            "dogstatsd.aggregation.context_limit",
            json!(5000),
        ),
        // A zero expiry never expires counters, written `0s`.
        (
            aggregation,
            "dsd_agg",
            "counter_expiry",
            "dogstatsd.aggregation.counter_expiry_seconds",
            json!("0s"),
        ),
        (
            aggregation,
            "dsd_agg",
            "flush_open_windows",
            "dogstatsd.aggregation.flush_open_windows",
            json!(true),
        ),
        (
            aggregation,
            "dsd_agg",
            "copy_to_distribution",
            "metrics_encoding.histogram.copy_to_distribution",
            json!(true),
        ),
        (
            aggregation,
            "dsd_agg",
            "copy_to_distribution_prefix",
            "metrics_encoding.histogram.copy_to_distribution_prefix",
            json!("dist."),
        ),
        // The legacy blocklist applies while `metric_filterlist` is empty, as translation resolves it.
        (
            legacy_blocklist,
            "dsd_prefix_filter_blocklist",
            "predicate",
            "dogstatsd.metric_filter.values",
            json!(["blocked.metric"]),
        ),
        (
            legacy_blocklist,
            "dsd_prefix_filter_blocklist",
            "predicate",
            "dogstatsd.metric_filter.match_prefix",
            json!(true),
        ),
        // Only the filterlist entries that name a histogram statistic's series reach the post-aggregation filter.
        (
            mapper,
            "dsd_post_agg_filter",
            "predicate",
            "dogstatsd.post_aggregate_filter.values",
            json!(["tenant.requests.max"]),
        ),
        // Routing groups are ordered by their lists, an endpoint with empty lists has none, and endpoints with equal
        // lists share one.
        (
            routing_groups,
            "metrics_routing_filter_0",
            "predicate",
            &e_names,
            json!([]),
        ),
        (
            routing_groups,
            "metrics_routing_filter_0",
            "predicate",
            &e_prefixes,
            json!(["catalog."]),
        ),
        (
            routing_groups,
            "metrics_routing_filter_1",
            "predicate",
            &b_c_names,
            json!(["orders.total"]),
        ),
        (
            routing_groups,
            "metrics_routing_filter_1",
            "predicate",
            &b_c_prefixes,
            json!([]),
        ),
        // The branch exists only while failover is enabled; mirroring is off and mirrors everything by default.
        (
            multi_region_failover,
            "mrf_metrics_gateway",
            "predicate",
            "multi_region_failover.enabled",
            json!(true),
        ),
        (
            multi_region_failover,
            "mrf_metrics_gateway",
            "predicate",
            "multi_region_failover.metric_mirroring.enabled",
            json!(false),
        ),
        (
            multi_region_failover,
            "mrf_metrics_gateway",
            "predicate",
            "multi_region_failover.metric_mirroring.allowlist",
            json!([]),
        ),
        (
            mirroring,
            "mrf_metrics_gateway",
            "predicate",
            "multi_region_failover.metric_mirroring.enabled",
            json!(true),
        ),
        (
            mirroring,
            "mrf_metrics_gateway",
            "predicate",
            "multi_region_failover.metric_mirroring.allowlist",
            json!(["requests.count", "orders.total"]),
        ),
        (
            autoscaling_failover,
            "af_metrics_gateway",
            "predicate",
            "autoscaling_failover.metrics",
            json!(["requests.count"]),
        ),
    ];
    for (yaml, node, param, binding, expected) in cases {
        let lowered = lowered(yaml).await;
        let node = lowered.plan.node(&node.parse().unwrap()).unwrap();
        let found = bindings_of(node)
            .into_iter()
            .find(|(p, b, _)| p == param && b == binding)
            .unwrap_or_else(|| panic!("`{}.{}` is bound to `{}`", node.id, param, binding));
        assert_eq!(found.2, expected, "`{}.{}` bound to `{}`", node.id, param, binding);
    }
}

#[tokio::test]
async fn repeated_histogram_statistics_are_computed_once_and_noted() {
    let lowered = lowered("histogram_aggregates: [max, max, count]\nhistogram_percentiles: ['0.95', '0.951']").await;
    let aggregate = lowered.plan.node(&"dsd_agg".parse().unwrap()).unwrap();
    let resolved = |param: &str| {
        bindings_of(aggregate)
            .into_iter()
            .find(|(p, _, _)| p == param)
            .map(|(_, _, value)| value)
            .unwrap()
    };
    assert_eq!(resolved("histogram_aggregates"), json!(["max", "count"]));
    // Both quantiles round to the 95th percentile.
    assert_eq!(resolved("histogram_percentiles"), json!([95]));
    assert_eq!(
        lowered.notes,
        vec![LoweringNote {
            node: "dsd_agg".parse().unwrap(),
            message: "The histogram statistics `max`, `95percentile` are listed more than once; each is computed \
                      once. Native aggregation emits a repeated statistic's series once per listing."
                .to_string(),
        }]
    );
    assert!(validate(&lowered.plan, &lowered.catalog).findings().is_empty());
}

#[tokio::test]
async fn lowering_fails_where_the_native_topology_fails_to_build() {
    let cases = [
        (
            "dd_url: https://primary.example.com\nexperimental: {metrics_endpoint_routing: {metric_allowlist: \
             {https://unknown.example.com: [a]}}}",
            "Experimental metrics endpoint-routing policy endpoint 'https://unknown.example.com' does not match the \
             configured primary endpoint and is not present in `additional_endpoints`; correct the endpoint, add it \
             and its API key to `additional_endpoints`, or remove it from \
             `experimental.metrics_endpoint_routing.metric_allowlist` and \
             `experimental.metrics_endpoint_routing.metric_prefix_allowlist`.",
        ),
        (
            "histogram_aggregates: [p99]",
            "Failed to configure histogram aggregation.",
        ),
        (
            "histogram_percentiles: ['1.5']",
            "Failed to configure histogram aggregation.",
        ),
        (
            "dogstatsd_mapper_profiles: [{name: p, prefix: a., mappings: [{match: a.*, match_type: glob, name: b}]}]",
            "profile: p, mapping num 0: invalid match type `glob`, expected `wildcard` or `regex`",
        ),
        (
            "dogstatsd_mapper_profiles: [{name: p, prefix: a., mappings: [{match: a.**, name: b}]}]",
            "invalid wildcard match pattern `a.**`, it should not contain consecutive `*`",
        ),
        (
            "autoscaling: {failover: {enabled: true}}\ncluster_agent: {enabled: true, url: \
             https://cluster-agent.example.com, auth_token: \"bad\\ntoken\"}",
            "Failed to configure Cluster Agent forwarder.",
        ),
        (
            "dd_url: 'https://bad host'",
            "Failed to resolve the endpoints of a metrics forwarder.",
        ),
    ];
    for (yaml, expected) in cases {
        let config = configuration(yaml).await;
        let error = lowering_error(&config, facts(&config));
        assert_eq!(error.to_string(), expected, "{}", yaml);
    }
}

#[tokio::test]
async fn aggregation_durations_a_plan_cannot_represent_fail_with_the_setting_and_its_limit() {
    // (configuration, error); 2^53 + 1 seconds is no whole number of minutes, so seconds are its largest unit.
    let cases = [
        (
            "aggregate_window_duration_seconds: 9007199254740993",
            "`aggregate_window_duration_seconds` is 9007199254740993 seconds, which the metrics plan cannot \
             represent: a plan duration is a whole number of at most 9007199254740991 of the largest unit that \
             divides it exactly. Set `aggregate_window_duration_seconds` to at most 9007199254740991 seconds.",
        ),
        (
            "dogstatsd_expiry_seconds: 9007199254740993",
            "`dogstatsd_expiry_seconds` is 9007199254740993 seconds, which the metrics plan cannot represent: a plan \
             duration is a whole number of at most 9007199254740991 of the largest unit that divides it exactly. Set \
             `dogstatsd_expiry_seconds` to at most 9007199254740991 seconds.",
        ),
    ];
    for (yaml, expected) in cases {
        let config = configuration(yaml).await;
        let error = lowering_error(&config, facts(&config));
        assert_eq!(error.to_string(), expected, "{}", yaml);
    }
    // A longer duration that is a whole number of a larger unit is represented in that unit: 2^53 − 1 minutes.
    let config = configuration("aggregate_window_duration_seconds: 540431955284459460").await;
    lowering(&config)
        .lower(&config)
        .expect("9007199254740991 minutes lower");
}

/// With several problems, the one native construction meets first is reported: blueprint assembly checks endpoint
/// routing, then the Cluster Agent forwarder, then the DogStatsD pipeline's histogram statistics; building the
/// components then checks mapper profiles (transforms build first) and finally forwarder endpoints. (Invalid tag value
/// rules never get this far: loading the configuration rejects them.)
#[tokio::test]
async fn with_several_problems_lowering_reports_the_one_native_construction_meets_first() {
    const BAD_ROUTING: &str = "dd_url: https://primary.example.com\nexperimental: {metrics_endpoint_routing: \
                               {metric_allowlist: {https://unknown.example.com: [a]}}}";
    const BAD_TOKEN: &str = "autoscaling: {failover: {enabled: true}}\ncluster_agent: {enabled: true, url: \
                             https://cluster-agent.example.com, auth_token: \"bad\\ntoken\"}";
    const BAD_HISTOGRAM: &str = "histogram_aggregates: [p99]";
    const BAD_MAPPER: &str = "dogstatsd_mapper_profiles: [{name: p, prefix: a., mappings: [{match: a.**, name: b}]}]";
    const BAD_ENDPOINT: &str = "dd_url: 'https://bad host'";

    const ROUTING: &str = "Experimental metrics endpoint-routing policy endpoint";
    const TOKEN: &str = "Failed to configure Cluster Agent forwarder.";
    const HISTOGRAM: &str = "Failed to configure histogram aggregation.";
    const MAPPER: &str = "invalid wildcard match pattern `a.**`, it should not contain consecutive `*`";
    const ENDPOINT: &str = "Failed to resolve the endpoints of a metrics forwarder.";

    // (configuration layers, the error reported, and whether assembling the native blueprint already fails with it)
    let cases: [(&[&str], &str, bool); 7] = [
        (&[BAD_ENDPOINT], ENDPOINT, false),
        (&[BAD_ENDPOINT, BAD_HISTOGRAM], HISTOGRAM, true),
        (&[BAD_ENDPOINT, BAD_TOKEN], TOKEN, true),
        (&[BAD_HISTOGRAM, BAD_ROUTING], ROUTING, true),
        (&[BAD_HISTOGRAM, BAD_TOKEN], TOKEN, true),
        (&[BAD_MAPPER, BAD_HISTOGRAM], HISTOGRAM, true),
        // Both pass assembly; component construction builds the mapper before the forwarders.
        (&[BAD_ENDPOINT, BAD_MAPPER], MAPPER, false),
    ];
    for (layers, expected, at_assembly) in cases {
        let config = layered_configuration(layers).await;
        let error = lowering_error(&config, facts(&config)).to_string();
        assert!(error.starts_with(expected), "{:?}: {}", layers, error);

        // Native blueprint assembly agrees, and fails with the same error, or passes.
        let system = layered_configuration_system(layers).await;
        let env = environment_provider(&system.config()).await;
        let client = (!system.config().control.standalone_mode)
            .then(|| remote_agent_client_configuration(&system.config()).unwrap());
        let native = create_legacy_topology(&system, client.as_ref(), &env, &ComponentRegistry::default()).await;
        match native {
            Err(native) => {
                assert!(at_assembly, "{:?}: native assembly fails: {}", layers, native);
                assert_eq!(native.to_string(), error, "{:?}", layers);
            }
            Ok(_) => assert!(!at_assembly, "{:?}: native assembly passes", layers),
        }
    }
}

#[tokio::test]
async fn host_tags_need_the_host_facts_to_carry_them() {
    let config = configuration("data_plane: {standalone_mode: false}\nexpected_tags_duration: 5m").await;
    let mut facts = facts(&config);
    facts.host_tags = None;
    let error = lowering_error(&config, facts);
    assert_eq!(
        error.to_string(),
        "Host tags enrichment is enabled, but the host facts carry no host tags."
    );
}

#[tokio::test]
async fn host_tags_deadlines_are_whole_seconds_whatever_the_process_start() {
    // A wall clock reads the process start to the nanosecond, a count since the epoch that no plan integer holds.
    let config = configuration("data_plane: {standalone_mode: false}\nexpected_tags_duration: 5m").await;
    let mut facts = facts(&config);
    facts.process_start += Duration::from_nanos(123_456_789);
    let lowered = Lowering::new(Arc::new(config.clone()), facts)
        .unwrap()
        .unwrap()
        .lower(&config)
        .unwrap();
    let host_tags = lowered.plan.node(&"metrics_enrich_host_tags".parse().unwrap()).unwrap();
    assert_eq!(host_tags.params.get("deadline"), Some(&json!("1700000301s")));
    assert!(validate(&lowered.plan, &lowered.catalog).findings().is_empty());
}

#[tokio::test]
async fn the_dogstatsd_source_parses_timestamps_only_with_the_no_aggregation_pipeline() {
    for (yaml, timestamps) in [
        ("", "mixed"),
        ("dogstatsd_no_aggregation_pipeline: true", "mixed"),
        // Natively, client timestamps are discarded at parse time without the pipeline.
        ("dogstatsd_no_aggregation_pipeline: false", "untimestamped"),
    ] {
        let lowered = lowered(yaml).await;
        let source = lowered.plan.node(&"dsd_in".parse().unwrap()).unwrap();
        assert_eq!(source.params.get("timestamps"), Some(&json!(timestamps)), "{:?}", yaml);
    }
}

/// Native configurations that run natively, but whose default program does not validate: lowering describes them as
/// they are, and validation reports what a plan cannot express. The implementation design records each (amendments).
#[tokio::test]
async fn native_configurations_a_plan_cannot_express_lower_to_plans_that_do_not_validate() {
    let cases = [
        // Integers beyond the plan's integer range.
        ("aggregate_context_limit: 18014398509481984", "dsd_agg", "context_limit"),
        (
            "dogstatsd_mapper_cache_size: 18014398509481984",
            "dsd_enrich",
            "cache_size",
        ),
        // A zero flush interval, which natively stops the process when aggregation starts: Tokio's interval timer
        // panics on a zero period.
        ("aggregate_flush_interval: 0s", "dsd_agg", "flush_interval"),
    ];
    for (yaml, node, param) in cases {
        let lowered = lowered(yaml).await;
        let report = validate(&lowered.plan, &lowered.catalog);
        let errors = report
            .errors()
            .map(|finding| (finding.rule, finding.location.node().map(ToString::to_string)))
            .collect::<Vec<_>>();
        assert_eq!(
            errors,
            [(
                saluki_metric_plan::validate::Rule::ValidParameters,
                Some(node.to_string())
            )],
            "{}:\n{}",
            yaml,
            report
        );
        assert!(report.to_string().contains(param), "{}:\n{}", yaml, report);
    }

    // Native quirks a plan expresses: an additional endpoint spelled like the primary one, natively a second
    // recipient that every payload reaches, is a separate recipient, and the program validates.
    let lowered =
        lowered("dd_url: https://primary.example.com\nadditional_endpoints: {primary.example.com: [key-again]}").await;
    assert_eq!(
        lowered
            .plan
            .output(&"dd_metrics_encode".parse().unwrap())
            .unwrap()
            .recipients,
        [
            "dd_out/additional/0/primary.example.com".parse().unwrap(),
            "dd_out/primary/0/https://primary.example.com".parse().unwrap(),
        ]
    );
    assert!(validate(&lowered.plan, &lowered.catalog).findings().is_empty());
}
