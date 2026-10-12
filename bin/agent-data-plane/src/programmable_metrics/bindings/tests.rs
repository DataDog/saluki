use std::collections::BTreeSet;

use saluki_metric_plan::{
    ir::{BindingName, ValueType},
    testing::default_program_catalog,
};

use super::*;
use crate::programmable_metrics::test_support::{configuration, layered_configuration};

fn name(name: &str) -> BindingName {
    name.parse().unwrap()
}

#[test]
fn the_registry_names_typed_model_paths_once() {
    let mut names = BTreeSet::new();
    for spec in BINDINGS {
        assert!(names.insert(spec.name), "`{}` is registered once", spec.name);
        match spec.key {
            None => assert!(BindingName::new(spec.name).is_ok(), "`{}`", spec.name),
            Some(key) => {
                assert!(spec.name.contains(BindingKey::PLACEHOLDER), "`{}`", spec.name);
                let instance = spec.instance_name("0123456789abcdef");
                assert!(BindingName::new(&instance).is_ok(), "`{}`", instance);
                assert!(key.accepts("0123456789abcdef"));
            }
        }
    }
}

/// Bindings read straight from configuration are named by the path of what they read in the typed model, below its
/// top-level section; derived and keyed bindings have names of their own.
#[test]
fn bindings_read_from_configuration_are_named_by_their_typed_model_path() {
    let model = serde_json::to_value(SalukiConfiguration::default()).unwrap();
    let path_exists = |path: &str| {
        ["control", "shared", "domains"].iter().any(|section| {
            path.split('.')
                .try_fold(&model[*section], |value, segment| value.get(segment))
                .is_some()
        })
    };
    for spec in BINDINGS {
        match spec.resolve {
            Resolve::Value(_) => assert!(path_exists(spec.name), "`{}` is a typed-model path", spec.name),
            Resolve::Derived(_) | Resolve::Keyed(_) => {
                assert!(!path_exists(spec.name), "`{}` is not a typed-model path", spec.name)
            }
        }
    }
}

#[test]
fn live_bindings_are_exactly_the_natively_live_inputs_and_those_of_the_output_branches() {
    let live = BINDINGS
        .iter()
        .filter(|spec| spec.liveness == Liveness::Live)
        .map(|spec| spec.name)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        live,
        [
            // The output branches follow the current configuration, and their gateways' inputs with them.
            "autoscaling_failover.metrics",
            "dogstatsd.metric_filter.match_prefix",
            "dogstatsd.metric_filter.values",
            // Derived from the live filterlist.
            "dogstatsd.post_aggregate_filter.values",
            "dogstatsd.tag_filterlist",
            "metrics_endpoint_routing.group.{key}.names",
            "metrics_endpoint_routing.group.{key}.prefixes",
            "multi_region_failover.enabled",
            "multi_region_failover.metric_mirroring.allowlist",
            "multi_region_failover.metric_mirroring.enabled",
        ]
        .into_iter()
        .collect()
    );
}

#[test]
fn keyed_names_are_looked_up_by_their_pattern() {
    let (spec, key) = lookup(&name("metrics_endpoint_routing.group.0123456789abcdef.names")).unwrap();
    assert_eq!(spec.name, "metrics_endpoint_routing.group.{key}.names");
    assert_eq!(key, "0123456789abcdef");
    let (spec, key) = lookup(&name("dogstatsd.tag_filterlist")).unwrap();
    assert_eq!(spec.name, "dogstatsd.tag_filterlist");
    assert_eq!(key, "");

    for unknown in [
        // Keys are 16 lowercase hexadecimal digits.
        "metrics_endpoint_routing.group.0123456789abcde.names",
        "metrics_endpoint_routing.group.0123456789abcdeg.names",
        "metrics_endpoint_routing.group.0123456789abcdef.other",
        // Datadog keys are not binding names.
        "metric_filterlist",
        "autoscaling.failover.metrics",
        // Nor are paths the registry does not have, such as credentials.
        "endpoints.api_key",
        "multi_region_failover.api_key",
    ] {
        assert!(lookup(&name(unknown)).is_none(), "`{}`", unknown);
    }
}

/// Every binding's value for a configuration that sets each setting, derived by hand from the configuration.
#[tokio::test]
async fn bindings_resolve_to_native_values_written_as_plan_parameters() {
    let config = configuration(
        r#"
dd_url: https://primary.example.com
additional_endpoints: {https://b.example.com: [key-b], https://c.example.com: [key-c]}
experimental:
  metrics_endpoint_routing:
    metric_allowlist: {https://b.example.com: [orders.total, checkout.cart, orders.total]}
    metric_prefix_allowlist: {https://b.example.com: [checkout., checkout.a.], https://c.example.com: [catalog.]}
dogstatsd_mapper_profiles:
  - name: p
    prefix: a.
    mappings: [{match: a.*, name: b, tags: {z: $1, y: two}}]
dogstatsd_mapper_cache_size: 7
dogstatsd_mapper_string_interner_size: 2048
statsd_metric_namespace: ns
statsd_metric_namespace_blacklist: [skip.]
metric_filterlist: [blocked, h.max, h.95percentile, h.99percentile]
metric_filterlist_match_prefix: true
metric_tag_filterlist: [{metric_name: m, action: include, tags: [env]}]
metric_tag_value_allowlist: [{metric_prefix: m., tag_name: env, values: [prod], on_miss: replace, replacement: o}]
data_plane: {dogstatsd: {aggregator_tag_filter_cache_capacity: 9}}
aggregate_window_duration_seconds: 300
aggregate_flush_interval: 1500ms
aggregate_context_limit: 42
dogstatsd_expiry_seconds: 90
dogstatsd_flush_incomplete_buckets: true
histogram_aggregates: [max, count, max]
histogram_percentiles: ['0.955', '0.95', '0.951']
histogram_copy_to_distribution: true
histogram_copy_to_distribution_prefix: d.
multi_region_failover: {enabled: true, failover_metrics: true, metric_allowlist: [m1, m2]}
autoscaling: {failover: {metrics: [cpu]}}
"#,
    )
    .await;
    let snapshots = Snapshots {
        startup: &config,
        current: &config,
    };
    let b_key = super::super::resources::routing_group_key(&["https://b.example.com".to_string()]);
    let c_key = super::super::resources::routing_group_key(&["https://c.example.com".to_string()]);
    let cases = [
        (
            "dogstatsd.mapper.profiles".to_string(),
            json!([{"name": "p", "prefix": "a.", "mappings": [
                {"match": "a.*", "match_type": "wildcard", "name": "b",
                 "tags": [{"name": "y", "value": "two"}, {"name": "z", "value": "$1"}]}
            ]}]),
        ),
        ("dogstatsd.mapper.cache_size".to_string(), json!(7)),
        ("dogstatsd.mapper.string_interner_size_bytes".to_string(), json!(2048)),
        ("dogstatsd.prefix_filter.metric_namespace".to_string(), json!("ns")),
        (
            "dogstatsd.prefix_filter.metric_namespace_blocklist".to_string(),
            json!(["skip."]),
        ),
        (
            "dogstatsd.metric_filter.values".to_string(),
            json!(["blocked", "h.max", "h.95percentile", "h.99percentile"]),
        ),
        ("dogstatsd.metric_filter.match_prefix".to_string(), json!(true)),
        // Only entries naming a configured statistic's series: `95percentile` and `96percentile` are configured, `99`
        // is not.
        (
            "dogstatsd.post_aggregate_filter.values".to_string(),
            json!(["h.max", "h.95percentile"]),
        ),
        (
            "dogstatsd.tag_filterlist".to_string(),
            json!([{"metric_name": "m", "action": "include", "tags": ["env"]}]),
        ),
        (
            "dogstatsd.tag_value_allowlist".to_string(),
            json!([{"metric_prefix": "m.", "tag_name": "env", "values": ["prod"], "on_miss": "replace",
                    "replacement": "o"}]),
        ),
        (
            "dogstatsd.aggregation.aggregator_tag_filter_cache_capacity".to_string(),
            json!(9),
        ),
        ("dogstatsd.aggregation.window_duration_seconds".to_string(), json!("5m")),
        ("dogstatsd.aggregation.flush_interval".to_string(), json!("1500ms")),
        ("dogstatsd.aggregation.context_limit".to_string(), json!(42)),
        ("dogstatsd.aggregation.counter_expiry_seconds".to_string(), json!("90s")),
        ("dogstatsd.aggregation.flush_open_windows".to_string(), json!(true)),
        // Each statistic once, in configured order; `0.955` rounds up to 96, and `0.951` repeats 95.
        (
            "metrics_encoding.histogram.aggregates".to_string(),
            json!(["max", "count"]),
        ),
        ("metrics_encoding.histogram.percentiles".to_string(), json!([96, 95])),
        (
            "metrics_encoding.histogram.copy_to_distribution".to_string(),
            json!(true),
        ),
        (
            "metrics_encoding.histogram.copy_to_distribution_prefix".to_string(),
            json!("d."),
        ),
        // The canonical lists: names sorted and deduplicated without those a prefix covers, prefixes compacted.
        (
            format!("metrics_endpoint_routing.group.{}.names", b_key),
            json!(["orders.total"]),
        ),
        (
            format!("metrics_endpoint_routing.group.{}.prefixes", b_key),
            json!(["checkout."]),
        ),
        (format!("metrics_endpoint_routing.group.{}.names", c_key), json!([])),
        (
            format!("metrics_endpoint_routing.group.{}.prefixes", c_key),
            json!(["catalog."]),
        ),
        ("multi_region_failover.enabled".to_string(), json!(true)),
        (
            "multi_region_failover.metric_mirroring.enabled".to_string(),
            json!(true),
        ),
        (
            "multi_region_failover.metric_mirroring.allowlist".to_string(),
            json!(["m1", "m2"]),
        ),
        ("autoscaling_failover.metrics".to_string(), json!(["cpu"])),
    ];
    let resolved = cases
        .iter()
        .map(|(binding, _)| binding.clone())
        .collect::<BTreeSet<_>>();
    for spec in BINDINGS {
        let instances = match spec.key {
            None => vec![spec.name.to_string()],
            Some(key) => key.keys(&config).iter().map(|key| spec.instance_name(key)).collect(),
        };
        for instance in instances {
            assert!(resolved.contains(&instance), "`{}` has a case", instance);
        }
    }
    for (binding, expected) in cases {
        assert_eq!(resolve(&name(&binding), snapshots), Some(expected), "`{}`", binding);
    }

    // A routing group key that the configuration does not have resolves to nothing.
    assert_eq!(
        resolve(
            &name("metrics_endpoint_routing.group.0123456789abcdef.names"),
            snapshots
        ),
        None
    );
}

#[tokio::test]
async fn an_unset_or_zero_counter_expiry_never_expires() {
    for (yaml, expected) in [
        ("dogstatsd_expiry_seconds: 0", "0s"),
        ("dogstatsd_expiry_seconds: 300", "5m"),
        ("", "5m"),
    ] {
        let config = configuration(yaml).await;
        let snapshots = Snapshots {
            startup: &config,
            current: &config,
        };
        assert_eq!(
            resolve(&name("dogstatsd.aggregation.counter_expiry_seconds"), snapshots),
            Some(json!(expected)),
            "{:?}",
            yaml
        );
    }
    // The typed model's unset expiry, which translation never leaves, also never expires.
    let mut config = configuration("").await;
    config.domains.dogstatsd.aggregation.counter_expiry_seconds = None;
    let snapshots = Snapshots {
        startup: &config,
        current: &config,
    };
    assert_eq!(
        resolve(&name("dogstatsd.aggregation.counter_expiry_seconds"), snapshots),
        Some(json!("0s"))
    );
}

#[tokio::test]
async fn bindings_resolve_from_the_snapshot_their_liveness_names() {
    let startup = configuration("metric_filterlist: [h.max]\nstatsd_metric_namespace: old").await;
    let current = layered_configuration(&[
        "metric_filterlist: [h.max]\nstatsd_metric_namespace: old",
        "metric_filterlist: [h.min, h.max]\nstatsd_metric_namespace: new\nhistogram_aggregates: [min]",
    ])
    .await;
    let snapshots = Snapshots {
        startup: &startup,
        current: &current,
    };
    // A live binding follows the current configuration.
    assert_eq!(
        resolve(&name("dogstatsd.metric_filter.values"), snapshots),
        Some(json!(["h.min", "h.max"]))
    );
    // A startup binding keeps the startup configuration.
    assert_eq!(
        resolve(&name("dogstatsd.prefix_filter.metric_namespace"), snapshots),
        Some(json!("old"))
    );
    // A derived binding takes its live input from the current configuration and the statistics from startup: `min`
    // was added only to the current configuration, so `h.min` names no statistic yet.
    assert_eq!(
        resolve(&name("dogstatsd.post_aggregate_filter.values"), snapshots),
        Some(json!(["h.max"]))
    );
}

#[tokio::test]
async fn bindings_have_no_value_for_settings_aggregation_cannot_compute() {
    let config = configuration("histogram_aggregates: [p99]").await;
    let snapshots = Snapshots {
        startup: &config,
        current: &config,
    };
    for binding in [
        "metrics_encoding.histogram.aggregates",
        "metrics_encoding.histogram.percentiles",
        "dogstatsd.post_aggregate_filter.values",
    ] {
        assert_eq!(resolve(&name(binding), snapshots), None, "`{}`", binding);
    }
    assert_eq!(
        histogram_params(&config).unwrap_err().to_string(),
        "Unknown histogram aggregate: p99"
    );
}

#[tokio::test]
async fn credentials_are_never_bindable() {
    let secrets = [
        "secret-primary-key",
        "secret-additional-key",
        "secret-mrf-key",
        "secret-cluster-agent-token",
        "secret-backend-command",
    ];
    let config = configuration(
        r#"
api_key: secret-primary-key
dd_url: https://primary.example.com
additional_endpoints: {https://b.example.com: [secret-additional-key]}
experimental: {metrics_endpoint_routing: {metric_allowlist: {https://b.example.com: [orders.total]}}}
multi_region_failover: {enabled: true, api_key: secret-mrf-key, dd_url: https://mrf.example.com}
cluster_agent: {enabled: true, url: https://ca.example.com, auth_token: secret-cluster-agent-token}
autoscaling: {failover: {enabled: true}}
secret_backend_command: secret-backend-command
"#,
    )
    .await;
    let snapshots = Snapshots {
        startup: &config,
        current: &config,
    };
    for spec in BINDINGS {
        let keys = spec
            .key
            .map(|key| key.keys(&config))
            .unwrap_or_else(|| vec![String::new()]);
        for key in keys {
            let value = spec
                .resolve(snapshots, &key)
                .map(|value| value.to_string())
                .unwrap_or_default();
            for secret in secrets {
                assert!(
                    !value.contains(secret),
                    "`{}` carries `{}`",
                    spec.instance_name(&key),
                    secret
                );
            }
        }
    }
}

/// The plan crate's hand-written default-program catalog names bindings as this registry does.
#[test]
fn the_plan_crates_default_program_catalog_follows_the_registry() {
    let document = serde_json::to_value(default_program_catalog()).unwrap();
    let bindings = document["bindings"].as_object().unwrap();
    assert!(!bindings.is_empty());
    for (binding, declared) in bindings {
        let (spec, _) = lookup(&name(binding)).unwrap_or_else(|| panic!("`{}` is not in the registry", binding));
        let ty: ValueType = serde_json::from_value(declared["type"].clone()).unwrap();
        assert_eq!(ty, ValueType::from(&spec.ty), "`{}`", binding);
        let liveness: Liveness = serde_json::from_value(declared["liveness"].clone()).unwrap();
        assert_eq!(liveness, spec.liveness, "`{}`", binding);
    }
}
