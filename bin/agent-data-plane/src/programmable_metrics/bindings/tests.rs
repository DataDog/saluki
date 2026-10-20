use std::collections::BTreeSet;

use saluki_metric_plan::{
    ir::{BindingName, ValueType},
    testing::default_program_catalog,
};

use super::*;
use crate::programmable_metrics::test_support::{configuration, facts};

fn name(name: &str) -> BindingName {
    name.parse().unwrap()
}

#[test]
fn the_registry_names_each_binding_once() {
    let mut names = BTreeSet::new();
    for spec in BINDINGS {
        assert!(names.insert(spec.name), "`{}` is registered once", spec.name);
        assert!(BindingName::new(spec.name).is_ok(), "`{}`", spec.name);
    }
}

/// Bindings read straight from configuration are named by the path of what they read in the typed model, below its
/// top-level section; facts of the host have names of their own.
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
            Resolve::Fact(_) => assert!(!path_exists(spec.name), "`{}` is not a typed-model path", spec.name),
        }
    }
}

/// Every binding is a fact or a signal, listed here, so that a new binding is classified on purpose. Policy settings
/// are never bindings: lowering writes them as literals.
#[test]
fn every_binding_is_a_fact_or_a_signal() {
    let classes = BINDINGS
        .iter()
        .map(|spec| (spec.name, (spec.class, spec.liveness)))
        .collect::<std::collections::BTreeMap<_, _>>();
    let (fact, signal) = (BindingClass::Fact, BindingClass::Signal);
    let (live, startup) = (Liveness::Live, Liveness::Startup);
    let expected = [
        // The switches of the output branches follow the current configuration, as the branches do.
        ("autoscaling_failover.enabled", (signal, live)),
        ("host.tags", (fact, startup)),
        ("host.tags_deadline", (fact, startup)),
        ("multi_region_failover.enabled", (fact, live)),
        ("multi_region_failover.metric_mirroring.enabled", (signal, live)),
    ];
    assert_eq!(classes, expected.into_iter().collect());

    // Every binding says what it is.
    for spec in BINDINGS {
        assert!(!spec.about.is_empty() && spec.about.ends_with('.'), "`{}`", spec.name);
    }
    assert_eq!(
        annotation(&name("host.tags")).as_deref(),
        Some("fact: The host's tags, fetched from the Core Agent at boot.")
    );
}

#[test]
fn policy_settings_datadog_keys_and_credentials_are_not_bindings() {
    for unknown in [
        // Policy settings, which lowering writes as literals.
        "dogstatsd.metric_filter.values",
        "dogstatsd.tag_filterlist",
        "multi_region_failover.metric_mirroring.allowlist",
        "metrics_endpoint_routing.group.0123456789abcdef.names",
        // Datadog keys are not binding names.
        "metric_filterlist",
        "autoscaling.failover.metrics",
        // Nor are credentials.
        "endpoints.api_key",
        "multi_region_failover.api_key",
    ] {
        assert!(lookup(&name(unknown)).is_none(), "`{}`", unknown);
    }
}

/// Every binding's value for a configuration that sets each one, derived by hand from the configuration.
#[tokio::test]
async fn bindings_resolve_to_native_values_written_as_plan_parameters() {
    let config = configuration(
        r#"
data_plane: {standalone_mode: false}
expected_tags_duration: 10m
multi_region_failover: {enabled: true, failover_metrics: true, metric_allowlist: [m1, m2]}
autoscaling: {failover: {enabled: true, metrics: [cpu]}}
"#,
    )
    .await;
    let host_facts = facts(&config);
    let snapshots = Snapshots {
        startup: &config,
        current: &config,
        facts: &host_facts,
    };
    let cases = [
        ("multi_region_failover.enabled", json!(true)),
        ("multi_region_failover.metric_mirroring.enabled", json!(true)),
        ("autoscaling_failover.enabled", json!(true)),
        // Facts of the host: the tags fetched at boot, and the process start plus `expected_tags_duration`.
        ("host.tags", json!(["cloud_provider:example", "zone:a"])),
        ("host.tags_deadline", json!("1700000600s")),
    ];
    let resolved = cases.iter().map(|(binding, _)| *binding).collect::<BTreeSet<_>>();
    for spec in BINDINGS {
        assert!(resolved.contains(spec.name), "`{}` has a case", spec.name);
    }
    for (binding, expected) in cases {
        assert_eq!(resolve(&name(binding), snapshots), Some(expected), "`{}`", binding);
    }
}

#[tokio::test]
async fn live_bindings_resolve_from_the_current_configuration_and_facts_from_startup() {
    let startup = configuration("multi_region_failover: {failover_metrics: false}\nexpected_tags_duration: 10m").await;
    let current = configuration("multi_region_failover: {failover_metrics: true}\nexpected_tags_duration: 1h").await;
    let host_facts = facts(&startup);
    let snapshots = Snapshots {
        startup: &startup,
        current: &current,
        facts: &host_facts,
    };
    assert_eq!(
        resolve(&name("multi_region_failover.metric_mirroring.enabled"), snapshots),
        Some(json!(true))
    );
    assert_eq!(
        resolve(&name("host.tags_deadline"), snapshots),
        Some(json!("1700000600s"))
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
    let host_facts = facts(&config);
    let snapshots = Snapshots {
        startup: &config,
        current: &config,
        facts: &host_facts,
    };
    for spec in BINDINGS {
        let value = spec
            .resolve(snapshots)
            .map(|value| value.to_string())
            .unwrap_or_default();
        for secret in secrets {
            assert!(!value.contains(secret), "`{}` carries `{}`", spec.name, secret);
        }
    }
}

/// The plan crate's hand-written default-program catalog names bindings as this registry does.
#[test]
fn the_plan_crates_default_program_catalog_follows_the_registry() {
    let document = serde_json::to_value(default_program_catalog()).unwrap();
    let bindings = document["bindings"].as_object().unwrap();
    assert_eq!(bindings.len(), BINDINGS.len());
    for (binding, declared) in bindings {
        let spec = lookup(&name(binding)).unwrap_or_else(|| panic!("`{}` is not in the registry", binding));
        let ty: ValueType = serde_json::from_value(declared["type"].clone()).unwrap();
        assert_eq!(ty, ValueType::from(&spec.ty), "`{}`", binding);
        let liveness: Liveness = serde_json::from_value(declared["liveness"].clone()).unwrap();
        assert_eq!(liveness, spec.liveness, "`{}`", binding);
        let class: BindingClass = serde_json::from_value(declared["class"].clone()).unwrap();
        assert_eq!(class, spec.class, "`{}`", binding);
    }
}
