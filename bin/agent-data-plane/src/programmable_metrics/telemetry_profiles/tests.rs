use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit};
use saluki_core::{components::ComponentContext, support::SubsystemIdentifier, topology::ComponentId};
use saluki_metric_plan::{
    ir::Plan,
    operators::{KernelBuildContext, OperatorRegistry},
    testing::DEFAULT_PROGRAM_JSON,
};
use serde_json::json;

use super::*;
use crate::{
    components::{
        dogstatsd_post_aggregate_filter::telemetry::FILTERED_METRICS_METRIC,
        dogstatsd_prefix_filter::{
            LISTENER_FILTERED_POINTS_METRIC, METRIC_FILTERLIST_SIZE_METRIC, METRIC_FILTERLIST_UPDATES_METRIC,
        },
        metric_filter::{ALLOWLIST_METRICS_FILTERED_METRIC, ALLOWLIST_SIZE_METRIC},
    },
    programmable_metrics::{
        lowering::{matrix::EVERY_BRANCH, Lowering},
        test_support::{configuration, facts},
    },
};

fn lookup(node: &str, operator: &str) -> Option<NativeTelemetry> {
    native_telemetry(&node.parse().unwrap(), operator)
}

#[test]
fn reserved_nodes_report_as_the_native_component_they_stand_for() {
    let telemetry = |component: &str, component_type, profile| {
        Some(NativeTelemetry {
            component_id: component.to_string(),
            component_type,
            profile,
        })
    };
    let cases = [
        (
            "dsd_prefix_filter_blocklist",
            names::SPLIT,
            telemetry(
                "dsd_prefix_filter",
                ComponentType::Transform,
                TelemetryProfile::ListenerFilterlist,
            ),
        ),
        (
            "dsd_no_agg_split",
            names::TIMESTAMP_SPLIT,
            telemetry(
                "dsd_no_agg_split",
                ComponentType::Transform,
                TelemetryProfile::TimestampSplit,
            ),
        ),
        (
            "trace_sampler",
            names::SOURCE,
            telemetry("trace_sampler", ComponentType::Transform, TelemetryProfile::Host),
        ),
        // Aggregation keeps the native aggregation component's identity, which its kernel reports under.
        (
            "dsd_agg",
            names::AGGREGATE,
            telemetry("dsd_agg", ComponentType::Transform, TelemetryProfile::Aggregation),
        ),
        ("policy/dsd_agg", names::AGGREGATE, None),
        (
            "metrics_fanout_0",
            names::EMIT,
            telemetry("dd_metrics_encode", ComponentType::Encoder, TelemetryProfile::Silent),
        ),
        // A routing group's index carries over to its component.
        (
            "metrics_routing_filter_12",
            names::SPLIT,
            telemetry(
                "metrics_routing_filter_12",
                ComponentType::Transform,
                TelemetryProfile::EndpointAllowlist,
            ),
        ),
        (
            "metrics_routing_filter_3_true",
            names::EMIT,
            telemetry(
                "metrics_routing_encode_3",
                ComponentType::Encoder,
                TelemetryProfile::Silent,
            ),
        ),
        (
            "metrics_routing_filter_3_false",
            names::DROP,
            telemetry(
                "metrics_routing_filter_3",
                ComponentType::Transform,
                TelemetryProfile::Silent,
            ),
        ),
        // The failover gateways report no filter telemetry natively.
        (
            "mrf_metrics_gateway",
            names::SPLIT,
            telemetry(
                "mrf_metrics_gateway",
                ComponentType::Transform,
                TelemetryProfile::Silent,
            ),
        ),
        (
            "af_metrics_gateway_true",
            names::EMIT,
            telemetry("af_metrics_encode", ComponentType::Encoder, TelemetryProfile::Silent),
        ),
        // A node keeps its native identity only while its operator is the profile's.
        ("dsd_no_agg_split", names::SPLIT, None),
        ("dsd_tag_filterlist", names::OPAQUE_HOST, None),
        // The gateways are routing operators: an opaque host in their place has no native identity.
        ("metrics_routing_filter_0", names::OPAQUE_HOST, None),
        ("af_metrics_gateway", names::OPAQUE_HOST, None),
        // Indices are written canonically.
        ("metrics_routing_filter_01", names::SPLIT, None),
        ("metrics_routing_filter_a", names::SPLIT, None),
        // Nodes in the policy namespace have no native identity, whatever their local ID.
        ("policy/dsd_tag_filterlist", names::TAG_PROJECTION, None),
        ("unknown_node", names::TEE, None),
    ];
    for (node, operator, expected) in cases {
        assert_eq!(lookup(node, operator), expected, "`{}` running `{}`", node, operator);
    }
}

#[tokio::test]
async fn renamed_nodes_keep_the_native_telemetry_identity_of_their_former_ids() {
    // The nodes renamed to the IDs the pipeline syntax derives, each with its former ID, its operator, and the native
    // identity the former ID had: no native series moves with the rename.
    let cases = [
        (
            "dsd_prefix_filter_blocklist_namespace_prefix",
            "dsd_prefix_filter",
            names::NAMESPACE_PREFIX,
            ("dsd_prefix_filter", ComponentType::Transform, TelemetryProfile::Silent),
        ),
        (
            "metrics_in",
            "metrics_enrich_in",
            names::MERGE,
            ("metrics_enrich", ComponentType::Transform, TelemetryProfile::Silent),
        ),
        (
            "metrics_host_enrichment",
            "metrics_enrich",
            names::HOST_ENRICHMENT,
            ("metrics_enrich", ComponentType::Transform, TelemetryProfile::Silent),
        ),
        (
            "metrics",
            "metrics_enrich_host_tags",
            names::HOST_TAGS,
            ("metrics_enrich", ComponentType::Transform, TelemetryProfile::Silent),
        ),
        (
            "metrics_fanout",
            "metrics_policy",
            names::TEE,
            ("metrics_enrich", ComponentType::Transform, TelemetryProfile::Silent),
        ),
        (
            "metrics_fanout_0",
            "metrics_policy_0",
            names::EMIT,
            ("dd_metrics_encode", ComponentType::Encoder, TelemetryProfile::Silent),
        ),
    ];
    for (node, former, operator, (component, component_type, profile)) in cases {
        let expected = NativeTelemetry {
            component_id: component.to_string(),
            component_type,
            profile,
        };
        assert_eq!(
            lookup(node, operator),
            Some(expected),
            "`{}` running `{}`",
            node,
            operator
        );
        assert_eq!(
            lookup(former, operator),
            None,
            "the former ID `{}` running `{}`",
            former,
            operator
        );
    }

    // `metrics` is the enrichment chain's last node, whichever operator that is: host tags when enabled, host
    // enrichment otherwise. Each reports as metrics enrichment. Host enrichment is always a stage, so no merge is named
    // `metrics`.
    for operator in [names::HOST_TAGS, names::HOST_ENRICHMENT] {
        assert_eq!(
            lookup("metrics", operator),
            Some(NativeTelemetry {
                component_id: "metrics_enrich".to_string(),
                component_type: ComponentType::Transform,
                profile: TelemetryProfile::Silent,
            }),
            "`metrics` running `{}`",
            operator
        );
    }
    assert_eq!(lookup("metrics", names::MERGE), None);
    assert_eq!(lookup("metrics", names::TEE), None);

    // The default program's nodes are these, and each reports as its native component, as its provenance records.
    let config = configuration(EVERY_BRANCH).await;
    let lowered = Lowering::new(Arc::new(config.clone()), facts(&config))
        .unwrap()
        .unwrap()
        .lower(&config)
        .unwrap();
    let native_components = &lowered.plan.provenance.as_ref().unwrap().native_components;
    for (node, _, operator, (component, _, _)) in cases {
        let id = node.parse().unwrap();
        let lowered_node = lowered
            .plan
            .node(&id)
            .unwrap_or_else(|| panic!("the default program has `{}`", node));
        assert_eq!(lowered_node.op.as_str(), operator, "`{}`", node);
        let native = native_components.get(&id).map_or(node, |native| native.as_str());
        assert_eq!(native, component, "the native component of `{}`", node);
    }
}

#[test]
fn profiles_kept_by_native_components_name_their_metrics() {
    let names = |profile: TelemetryProfile| profile.metrics().iter().map(|metric| metric.name).collect::<Vec<_>>();
    assert_eq!(
        names(TelemetryProfile::ListenerFilterlist),
        [
            METRIC_FILTERLIST_SIZE_METRIC,
            METRIC_FILTERLIST_UPDATES_METRIC,
            LISTENER_FILTERED_POINTS_METRIC
        ]
    );
    assert_eq!(names(TelemetryProfile::PostAggregateFilter), [FILTERED_METRICS_METRIC]);
    // What the Datadog Agent's `aggregator.*` metrics are remapped from, and the context limit's drops.
    let aggregation = names(TelemetryProfile::Aggregation)
        .into_iter()
        .collect::<BTreeSet<_>>();
    assert_eq!(
        aggregation,
        [
            "aggregate_active_contexts",
            "aggregate_active_contexts_by_type",
            "aggregate_active_contexts_bytes_by_type",
            "aggregate_flushed_total",
            "aggregate_flushes_total",
            "component_events_dropped_total",
        ]
        .into_iter()
        .collect()
    );
    assert!(TelemetryProfile::Aggregation
        .metrics()
        .iter()
        .all(|metric| metric.source == MetricSource::Kernel));
    assert_eq!(
        names(TelemetryProfile::EndpointAllowlist),
        [
            ALLOWLIST_METRICS_FILTERED_METRIC,
            ALLOWLIST_SIZE_METRIC,
            ALLOWLIST_SIZE_METRIC
        ]
    );
    for profile in [
        TelemetryProfile::Silent,
        TelemetryProfile::Host,
        TelemetryProfile::Mapper,
    ] {
        assert!(profile.metrics().is_empty(), "{:?}", profile);
    }
}

/// A metric registration: its kind, name, and labels, sorted.
type Registration = (&'static str, String, Vec<(String, String)>);

/// A recorder that keeps every metric registration and records nothing.
#[derive(Clone, Default)]
struct Registrations(Arc<Mutex<BTreeSet<Registration>>>);

impl Registrations {
    fn record(&self, kind: &'static str, key: &Key) {
        let mut labels = key
            .labels()
            .map(|label| (label.key().to_string(), label.value().to_string()))
            .collect::<Vec<_>>();
        labels.sort();
        self.0.lock().unwrap().insert((kind, key.name().to_string(), labels));
    }

    /// Returns the registrations labeled with the component `component_id` of type `component_type`.
    fn of(&self, component_id: &str, component_type: &str) -> BTreeSet<Registration> {
        let has =
            |labels: &[(String, String)], key: &str, value: &str| labels.iter().any(|(k, v)| k == key && v == value);
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, labels)| {
                has(labels, "component_id", component_id) && has(labels, "component_type", component_type)
            })
            .cloned()
            .collect()
    }
}

impl Recorder for Registrations {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        self.record("counter", key);
        Counter::noop()
    }

    fn register_gauge(&self, key: &Key, _: &Metadata<'_>) -> Gauge {
        self.record("gauge", key);
        Gauge::noop()
    }

    fn register_histogram(&self, key: &Key, _: &Metadata<'_>) -> Histogram {
        self.record("histogram", key);
        Histogram::noop()
    }
}

/// Kernels built for a reserved node, as the identity its profile gives, register exactly the profile's metrics that
/// kernels record, with the native labels: no other metric reports under the node's identity, not even one that would
/// stay zero.
#[test]
fn kernels_built_as_their_profile_says_register_exactly_its_kernel_metrics() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _runtime = runtime.enter();
    let recorder = Registrations::default();
    let _recorder = metrics::set_default_local_recorder(&recorder);

    // Aggregation's parameters as the default program binds them.
    let default_program = Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap();
    let aggregation = default_program
        .node(&"dsd_agg".parse().unwrap())
        .expect("the default program aggregates");
    let cases = [
        ("dsd_no_agg_split", names::TIMESTAMP_SPLIT, json!({"window": "10s"})),
        (
            "dsd_tag_filterlist",
            names::TAG_PROJECTION,
            json!({"rules": [], "value_rules": [], "cache_capacity": 10}),
        ),
        (
            "dsd_agg",
            names::AGGREGATE,
            serde_json::to_value(&aggregation.params).unwrap(),
        ),
    ];
    for (node, operator, params) in cases {
        let telemetry = lookup(node, operator).unwrap();
        let configured = OperatorRegistry::native()
            .configure(operator, &serde_json::from_value(params).unwrap())
            .unwrap();
        let topology = SubsystemIdentifier::from_segments(["primary"]);
        let component = ComponentId::try_from(telemetry.component_id.as_str()).unwrap();
        let context = KernelBuildContext::new(node.parse().unwrap(), ComponentContext::transform(&topology, component));
        let _kernel = configured.kernel().unwrap().build(&context).unwrap();

        let component_type = telemetry.component_type.as_str();
        let expected = telemetry
            .profile
            .metrics()
            .iter()
            .filter(|metric| metric.source == MetricSource::Kernel)
            .map(|metric| {
                let kind = match metric.kind {
                    NativeMetricKind::Counter => "counter",
                    NativeMetricKind::Gauge => "gauge",
                };
                let mut labels = vec![
                    ("component_id".to_string(), telemetry.component_id.clone()),
                    ("component_type".to_string(), component_type.to_string()),
                ];
                for tag in metric.tags {
                    let (key, value) = tag.split_once(':').unwrap();
                    labels.push((key.to_string(), value.to_string()));
                }
                labels.sort();
                (kind, metric.name.to_string(), labels)
            })
            .collect::<BTreeSet<_>>();
        assert!(!expected.is_empty(), "`{}` has kernel metrics", node);
        assert_eq!(
            recorder.of(&telemetry.component_id, component_type),
            expected,
            "`{}` registers exactly its profile's kernel metrics",
            node
        );
    }
}

/// Every metric a host records rather than a kernel names what it is computed from, on the nodes that keep it: a port
/// of the node, the names its predicate matches, or its parameters.
#[tokio::test]
async fn metrics_recorded_outside_kernels_name_ports_and_parameters_of_their_nodes() {
    // The lowered default program and the plan crate's default program both have every branch.
    let config = configuration(EVERY_BRANCH).await;
    let lowered = Lowering::new(Arc::new(config.clone()), facts(&config))
        .unwrap()
        .unwrap()
        .lower(&config)
        .unwrap();
    let plans = [lowered.plan, Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap()];

    let mut checked = BTreeSet::new();
    for plan in &plans {
        for node in &plan.nodes {
            let Some(telemetry) = native_telemetry(&node.id, node.op.as_str()) else {
                continue;
            };
            for metric in telemetry.profile.metrics() {
                let what = format!("`{}` of `{}`", metric.name, node.id);
                match metric.source {
                    MetricSource::Kernel => continue,
                    MetricSource::PortEvents(port) => assert!(
                        plan.edges
                            .iter()
                            .any(|edge| edge.from.node == node.id && edge.from.port.as_str() == port),
                        "{}: no port `{}`",
                        what,
                        port
                    ),
                    MetricSource::ListLength | MetricSource::MatcherEntries { .. } => {
                        assert_eq!(node.op.as_str(), names::SPLIT, "{}: not a `split`", what);
                        assert!(node.params.get("predicate").is_some(), "{}: no predicate", what);
                    }
                    MetricSource::TagProjectionRules => {
                        assert_eq!(node.op.as_str(), names::TAG_PROJECTION, "{}", what);
                        assert!(
                            node.params.get("rules").is_some() && node.params.get("value_rules").is_some(),
                            "{}: no rules",
                            what
                        );
                    }
                    MetricSource::ParamChanges => {
                        assert!(node.params.iter().next().is_some(), "{}: no parameters", what);
                    }
                }
                checked.insert(format!("{:?}", telemetry.profile));
            }
        }
    }
    assert_eq!(
        checked,
        [
            "EndpointAllowlist",
            "ListenerFilterlist",
            "PostAggregateFilter",
            "TagFilterlist"
        ]
        .into_iter()
        .map(String::from)
        .collect(),
        "every profile with metrics a host records was checked"
    );
}
