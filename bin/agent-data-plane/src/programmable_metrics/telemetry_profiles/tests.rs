use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit};
use saluki_core::{components::ComponentContext, support::SubsystemIdentifier, topology::ComponentId};
use saluki_metric_plan::{
    ir::{Node, Plan},
    operators::{KernelBuildContext, OperatorRegistry},
    testing::DEFAULT_PROGRAM_JSON,
    validate::Liveness,
};
use serde_json::{json, Value};

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
        bindings,
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
        (
            "metrics_policy_0",
            names::EMIT,
            telemetry("dd_metrics_encode", ComponentType::Encoder, TelemetryProfile::Silent),
        ),
        // A routing group's index carries over to its component.
        (
            "metrics_routing_filter_12",
            names::OPAQUE_HOST,
            telemetry(
                "metrics_routing_filter_12",
                ComponentType::Transform,
                TelemetryProfile::Host,
            ),
        ),
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
            "metrics_routing_filter_3_out",
            names::EMIT,
            telemetry(
                "metrics_routing_encode_3",
                ComponentType::Encoder,
                TelemetryProfile::Silent,
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
        // The failover gateways report no filter telemetry natively once they are routing operators.
        (
            "mrf_metrics_gateway",
            names::SPLIT,
            telemetry(
                "mrf_metrics_gateway",
                ComponentType::Transform,
                TelemetryProfile::Silent,
            ),
        ),
        // A node keeps its native identity only while its operator is the profile's.
        ("dsd_no_agg_split", names::SPLIT, None),
        ("dsd_tag_filterlist", names::OPAQUE_HOST, None),
        // Indices are written canonically.
        ("metrics_routing_filter_01", names::OPAQUE_HOST, None),
        ("metrics_routing_filter_a", names::OPAQUE_HOST, None),
        // Nodes in the policy namespace have no native identity, whatever their local ID.
        ("policy/dsd_tag_filterlist", names::TAG_PROJECTION, None),
        ("unknown_node", names::TEE, None),
    ];
    for (node, operator, expected) in cases {
        assert_eq!(lookup(node, operator), expected, "`{}` running `{}`", node, operator);
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

    let cases = [
        ("dsd_no_agg_split", names::TIMESTAMP_SPLIT, json!({"window": "10s"})),
        (
            "dsd_tag_filterlist",
            names::TAG_PROJECTION,
            json!({"rules": [], "value_rules": [], "cache_capacity": 10}),
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

/// Returns the bindings a node's parameters are bound to.
fn bound(node: &Node) -> BTreeSet<String> {
    fn visit(value: &Value, found: &mut BTreeSet<String>) {
        match value {
            Value::Object(members) => match members.get("config") {
                Some(Value::String(binding)) => {
                    found.insert(binding.clone());
                }
                _ => members.values().for_each(|value| visit(value, found)),
            },
            Value::Array(values) => values.iter().for_each(|value| visit(value, found)),
            _ => {}
        }
    }
    let mut found = BTreeSet::new();
    node.params.iter().for_each(|(_, value)| visit(value, &mut found));
    found
}

/// Returns `true` if the binding `name` is an instance of the registry binding `pattern`, which may be keyed.
fn instance_of(name: &str, pattern: &str) -> bool {
    bindings::lookup(&name.parse().unwrap()).is_some_and(|(spec, _)| spec.name == pattern)
}

/// Every metric a host records rather than a kernel names what it is computed from, on the nodes that keep it: a port
/// of the node, or bindings of its parameters; and only live bindings change.
#[tokio::test]
async fn metrics_recorded_outside_kernels_name_ports_and_bindings_of_their_nodes() {
    // The lowered default program has every branch; the plan crate's default program has the stage 4 gateway splits.
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
            let bound = bound(node);
            let has_binding = |pattern: &str| bound.iter().any(|binding| instance_of(binding, pattern));
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
                    MetricSource::BindingLength(binding) | MetricSource::MatcherEntries { binding, .. } => {
                        assert!(has_binding(binding), "{}: not bound to `{}`", what, binding)
                    }
                    MetricSource::TagProjectionRules => {
                        assert_eq!(node.op.as_str(), names::TAG_PROJECTION, "{}", what);
                        assert!(
                            has_binding("dogstatsd.tag_filterlist") && has_binding("dogstatsd.tag_value_allowlist"),
                            "{}: the rules are not bound",
                            what
                        );
                    }
                    MetricSource::BindingChanges(changed) => {
                        assert!(!changed.is_empty(), "{}", what);
                        for binding in changed {
                            assert!(has_binding(binding), "{}: not bound to `{}`", what, binding);
                            let spec = bindings::BINDINGS.iter().find(|spec| spec.name == *binding).unwrap();
                            assert_eq!(spec.liveness, Liveness::Live, "{}: `{}` never changes", what, binding);
                        }
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
