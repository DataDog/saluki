use saluki_components::forwarders::{
    delivery::{PayloadClasses as NativeClasses, RecipientProtocol},
    MetricsRecipientRoute,
};
use saluki_metric_plan::ir::decode_params;

use super::{
    params::{decode_encoder, decode_protocol, ENCODER},
    *,
};
use crate::programmable_metrics::test_support::{configuration, facts};

/// Resolves the resources of the configuration `yaml`, booted with it.
async fn resolved(yaml: &str) -> NativeResources {
    let config = configuration(yaml).await;
    let facts = facts(&config);
    NativeResources::resolve(&config, &config, &facts, &FixedHosts::at_boot(&facts)).unwrap()
}

async fn outputs(yaml: &str) -> NativeOutputs {
    resolved(yaml).await.outputs
}

fn recipients(outputs: &NativeOutputs, id: &str) -> Vec<String> {
    outputs
        .get(id)
        .unwrap_or_else(|| panic!("no output `{}`", id))
        .decl
        .recipients
        .iter()
        .map(|recipient| recipient.to_string())
        .collect()
}

fn ids(outputs: &NativeOutputs) -> Vec<String> {
    outputs.iter().map(|output| output.decl.id.to_string()).collect()
}

fn recipient<'a>(resources: &'a NativeResources, id: &str) -> &'a RecipientDecl {
    resources
        .resources
        .recipient(&RecipientId::new(id).unwrap())
        .unwrap_or_else(|| panic!("no recipient `{}` in {:?}", id, resources.resources.recipients))
}

fn binding<'a>(outputs: &'a NativeOutputs, id: &str) -> &'a OutputBinding {
    outputs.get(id).unwrap().decl.binding.as_ref().unwrap()
}

#[test]
fn recipient_identities_spell_out_plain_endpoints_only() {
    // Identities depend on neither classes nor protocol.
    let recipient = |route, endpoint: &str, api_key_index| MetricsRecipient {
        route,
        configured_endpoint: endpoint.to_string(),
        api_key_index,
        classes: NativeClasses::All,
        protocol: RecipientProtocol::ClusterAgent,
    };
    // The digest form is the first 16 hexadecimal digits of the endpoint's SHA-256.
    let digest = |endpoint: &str| format!("sha256:{}", &PlanHash::of_bytes(endpoint.as_bytes()).to_string()[7..23]);
    let long = format!("https://{}.example.com", "a".repeat(400));
    let cases = [
        (
            recipient(MetricsRecipientRoute::Primary, "https://app.example.com", None),
            "dd_out/primary/0/https://app.example.com".to_string(),
        ),
        (
            recipient(MetricsRecipientRoute::Additional, "app.example.com", Some(3)),
            "dd_out/additional/3/app.example.com".to_string(),
        ),
        (
            recipient(MetricsRecipientRoute::MetricsOverride, "http://opw:8080", None),
            "dd_out/metrics_override/0/http://opw:8080".to_string(),
        ),
        // Userinfo may carry credentials, so an endpoint with `@` is never spelled out.
        (
            recipient(MetricsRecipientRoute::Primary, "https://user:pass@example.com", None),
            format!("dd_out/primary/0/{}", digest("https://user:pass@example.com")),
        ),
        // A primary endpoint is spelled as the forwarder resolves it, which writes a host in its ASCII form.
        (
            recipient(MetricsRecipientRoute::Primary, "https://exämple.com", None),
            "dd_out/primary/0/https://xn--exmple-cua.com".to_string(),
        ),
        (
            recipient(MetricsRecipientRoute::Primary, "https://a b.com", None),
            format!("dd_out/primary/0/{}", digest("https://a b.com")),
        ),
        (
            recipient(MetricsRecipientRoute::Primary, &long, None),
            format!("dd_out/primary/0/{}", digest(&long)),
        ),
        // An endpoint spelled like a digest is resolved as a host and port, so the two forms never collide.
        (
            recipient(MetricsRecipientRoute::Primary, "sha256:0123", None),
            "dd_out/primary/0/https://sha256:123".to_string(),
        ),
        (
            recipient(MetricsRecipientRoute::Additional, "sha256:0123", Some(0)),
            format!("dd_out/additional/0/{}", digest("sha256:0123")),
        ),
    ];
    for (recipient, expected) in cases {
        assert_eq!(recipient_id(DD_OUT, &recipient).to_string(), expected);
    }

    // The spellings of one primary endpoint share its queue directory, so they are one recipient; the spellings of one
    // additional endpoint have one directory each, so they are as many recipients.
    for configured in ["http://127.0.0.1:5000", "http://127.0.0.1:5000/"] {
        assert_eq!(
            recipient_id(DD_OUT, &recipient(MetricsRecipientRoute::Primary, configured, None)).to_string(),
            "dd_out/primary/0/http://127.0.0.1:5000",
            "{configured}"
        );
    }
    for configured in [
        "https://App.Example.com:443/",
        "app.example.com",
        "https://app.example.com",
    ] {
        assert_eq!(
            recipient_id(
                DD_OUT,
                &recipient(MetricsRecipientRoute::MetricsOverride, configured, None)
            )
            .to_string(),
            "dd_out/metrics_override/0/https://app.example.com",
            "{configured}"
        );
    }
    // A Datadog intake reads as configured, without the data plane's version label the forwarder resolves it with.
    for configured in [
        "https://app.datadoghq.com",
        "https://app.datadoghq.com/",
        "app.datadoghq.com",
    ] {
        assert_eq!(
            recipient_id(DD_OUT, &recipient(MetricsRecipientRoute::Primary, configured, None)).to_string(),
            "dd_out/primary/0/https://app.datadoghq.com",
            "{configured}"
        );
    }
    let additional = |configured| {
        recipient_id(
            DD_OUT,
            &recipient(MetricsRecipientRoute::Additional, configured, Some(0)),
        )
        .to_string()
    };
    assert_ne!(
        additional("https://b.example.com"),
        additional("https://b.example.com/")
    );
    assert_eq!(
        spelled_endpoint("https://user:pass@example.com"),
        digest("https://user:pass@example.com")
    );
}

#[tokio::test]
async fn a_removed_routing_group_renumbers_the_outputs_after_it_each_named_with_both_groups() {
    let three = r#"
dd_url: https://primary.example.com
additional_endpoints:
  https://b.example.com: [key-b]
  https://c.example.com: [key-c]
  https://d.example.com: [key-d]
experimental:
  metrics_endpoint_routing:
    metric_allowlist:
      https://b.example.com: [b.metric]
      https://c.example.com: [c.metric]
      https://d.example.com: [d.metric]
"#;
    let without_the_first = r#"
dd_url: https://primary.example.com
additional_endpoints:
  https://c.example.com: [key-c]
  https://d.example.com: [key-d]
experimental:
  metrics_endpoint_routing:
    metric_allowlist:
      https://c.example.com: [c.metric]
      https://d.example.com: [d.metric]
"#;
    let before = outputs(three).await;
    let after = outputs(without_the_first).await;
    let key = |outputs: &NativeOutputs, index: usize| {
        outputs
            .routing_groups()
            .find(|group| group.index == index)
            .unwrap()
            .key
            .clone()
    };
    // The groups are ordered by their lists: b's, c's, then d's; without b's, c's is the first and d's the second.
    let renumbered = after
        .renumbered_routing_groups(&before)
        .into_iter()
        .map(|(id, was, is)| (id.to_string(), was.to_string(), is.to_string()))
        .collect::<Vec<_>>();
    assert_eq!(
        renumbered,
        vec![
            ("metrics_routing_encode_0".to_string(), key(&before, 0), key(&before, 1)),
            ("metrics_routing_encode_1".to_string(), key(&before, 1), key(&before, 2)),
        ]
    );
    assert_eq!((key(&after, 0), key(&after, 1)), (key(&before, 1), key(&before, 2)));
    assert!(before.renumbered_routing_groups(&before).is_empty());
}

#[test]
fn routing_group_keys_depend_only_on_the_endpoint_set() {
    // The first 16 hexadecimal digits of the SHA-256 of `["https://b.example.com"]`, computed with `shasum -a 256`.
    assert_eq!(
        routing_group_key(&["https://b.example.com".to_string()]),
        "3b8a3a482e967bb0"
    );
    let b_c = routing_group_key(&["https://b.example.com".to_string(), "https://c.example.com".to_string()]);
    assert_eq!(
        routing_group_key(&[
            "https://c.example.com".to_string(),
            "https://b.example.com".to_string(),
            "https://c.example.com".to_string(),
        ]),
        b_c
    );
    assert_ne!(b_c, routing_group_key(&["https://b.example.com".to_string()]));
}

#[tokio::test]
async fn outputs_exist_exactly_when_their_branches_are_wired() {
    let cases: [(&str, &[&str]); 9] = [
        ("", &["dsd_client_telemetry", "dd_metrics_encode"]),
        // Without DogStatsD there is no client telemetry tap.
        (
            "data_plane: {dogstatsd: {enabled: false}, otlp: {enabled: true}}",
            &["dd_metrics_encode"],
        ),
        // Without a metrics pipeline there are no outputs at all.
        (
            "data_plane: {dogstatsd: {enabled: false}, otlp: {enabled: true, proxy: {enabled: true}}}",
            &[],
        ),
        (
            "multi_region_failover: {enabled: true, api_key: k, dd_url: https://mrf.example.com}",
            &["dsd_client_telemetry", "dd_metrics_encode", "mrf_metrics_encode"],
        ),
        // A site is enough for the failover endpoint, but the API key is required.
        (
            "multi_region_failover: {enabled: true, api_key: k, site: example.com}",
            &["dsd_client_telemetry", "dd_metrics_encode", "mrf_metrics_encode"],
        ),
        (
            "multi_region_failover: {enabled: true, site: example.com}",
            &["dsd_client_telemetry", "dd_metrics_encode"],
        ),
        (
            "autoscaling: {failover: {enabled: true, metrics: [m]}}\ncluster_agent: {enabled: true, url: \
             https://ca.example.com, auth_token: t}",
            &["dsd_client_telemetry", "dd_metrics_encode", "af_metrics_encode"],
        ),
        // A requested branch with an empty metric list is not wired.
        (
            "autoscaling: {failover: {enabled: true, metrics: []}}\ncluster_agent: {enabled: true, url: \
             https://ca.example.com, auth_token: t}",
            &["dsd_client_telemetry", "dd_metrics_encode"],
        ),
        // The Cluster Agent endpoint resolves without forwarding enabled, but the branch needs it.
        (
            "autoscaling: {failover: {enabled: true, metrics: [m]}}\ncluster_agent: {url: https://ca.example.com, \
             auth_token: t}",
            &["dsd_client_telemetry", "dd_metrics_encode"],
        ),
    ];
    for (yaml, expected) in cases {
        assert_eq!(ids(&outputs(yaml).await), expected, "{}", yaml);
    }
}

#[tokio::test]
async fn the_delivery_hosts_are_fixed_by_the_host_facts() {
    let without = configuration("").await;
    let hosts = FixedHosts::at_boot(&facts(&without));
    let delivery = |hosts: &FixedHosts| hosts.iter().map(|host| host.delivery).collect::<Vec<_>>();
    assert_eq!(delivery(&hosts), ["dd_out", "mrf_dd_out"]);
    assert_eq!(hosts.get("dd_out"), Some(DD_HOST));
    assert_eq!(hosts.get("cluster_agent_out"), None);

    // A Cluster Agent endpoint that resolves at boot gives its host, idle until autoscaling failover is enabled.
    let with = configuration("cluster_agent: {url: https://ca.example.com}").await;
    let hosts = FixedHosts::at_boot(&facts(&with));
    assert_eq!(delivery(&hosts), ["cluster_agent_out", "dd_out", "mrf_dd_out"]);
    let resolved = NativeResources::resolve(&with, &with, &facts(&with), &hosts).unwrap();
    // Every host has settings, idle or not; only `dd_out` has recipients.
    let host_ids = resolved
        .resources
        .hosts
        .iter()
        .map(|binding| binding.host.to_string())
        .collect::<Vec<_>>();
    assert_eq!(host_ids, ["cluster_agent_out", "dd_out", "mrf_dd_out"]);
    assert!(resolved
        .resources
        .recipients
        .iter()
        .all(|recipient| recipient.host.as_str() == "dd_out"));
}

#[tokio::test]
async fn recipients_follow_native_endpoint_routing() {
    let resolved = resolved(
        r#"
dd_url: https://primary.example.com
additional_endpoints:
  https://b.example.com: [key-b1, key-b2]
  https://c.example.com: [key-c]
  https://d.example.com: [key-d]
  https://e.example.com: [key-e]
experimental:
  metrics_endpoint_routing:
    metric_allowlist:
      https://b.example.com: [orders.total]
      https://c.example.com: [orders.total]
      https://d.example.com: []
"#,
    )
    .await;
    let outputs = &resolved.outputs;
    // b and c share one group; d is selected with empty lists, so it is removed from the main output and gets no
    // group; e has no policy and receives the main output.
    assert_eq!(
        ids(outputs),
        ["dsd_client_telemetry", "dd_metrics_encode", "metrics_routing_encode_0"]
    );
    assert_eq!(
        recipients(outputs, "dd_metrics_encode"),
        [
            "dd_out/additional/0/https://e.example.com",
            "dd_out/primary/0/https://primary.example.com",
        ]
    );
    assert_eq!(
        recipients(outputs, "metrics_routing_encode_0"),
        [
            "dd_out/additional/0/https://b.example.com",
            "dd_out/additional/0/https://c.example.com",
            "dd_out/additional/1/https://b.example.com",
        ]
    );
    let groups = outputs.routing_groups().collect::<Vec<_>>();
    assert_eq!(
        groups,
        [&RoutingGroup {
            index: 0,
            key: routing_group_key(&["https://b.example.com".to_string(), "https://c.example.com".to_string()]),
            endpoints: vec!["https://b.example.com".to_string(), "https://c.example.com".to_string()],
        }]
    );
    let main = outputs.get("dd_metrics_encode").unwrap();
    assert_eq!(main.component, "dd_metrics_encode");
    assert_eq!(main.forwarder, Some(DD_OUT));
    assert_eq!(main.recipients.len(), 2);
    let telemetry = outputs.get("dsd_client_telemetry").unwrap();
    assert_eq!(telemetry.component, "dsd_client_telemetry_out");
    assert_eq!(telemetry.forwarder, None);
    assert!(telemetry.decl.recipients.is_empty());
    assert!(telemetry.decl.binding.is_none());

    // The native set holds every endpoint `dd_out` sends to, d included, which no output names: logs and traces
    // still reach it.
    let all = resolved
        .resources
        .recipients
        .iter()
        .map(|recipient| recipient.id.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        all,
        [
            "dd_out/additional/0/https://b.example.com",
            "dd_out/additional/0/https://c.example.com",
            "dd_out/additional/0/https://d.example.com",
            "dd_out/additional/0/https://e.example.com",
            "dd_out/additional/1/https://b.example.com",
            "dd_out/primary/0/https://primary.example.com",
        ]
    );
    let b1 = recipient(&resolved, "dd_out/additional/1/https://b.example.com");
    assert_eq!(b1.credential.as_str(), additional_api_key("https://b.example.com", 1));
    assert_eq!(b1.classes, PayloadClasses::All);
    assert_eq!(b1.endpoint, "https://b.example.com");
    assert_eq!(
        recipient(&resolved, "dd_out/primary/0/https://primary.example.com")
            .credential
            .as_str(),
        "native:api_key"
    );
}

#[tokio::test]
async fn a_repeated_primary_endpoint_is_a_separate_recipient() {
    // Natively, every payload goes to both, and a policy on the endpoint selects both.
    let outputs = outputs(
        r#"
dd_url: https://primary.example.com
additional_endpoints: {https://primary.example.com: [key-again]}
"#,
    )
    .await;
    assert_eq!(
        recipients(&outputs, "dd_metrics_encode"),
        [
            "dd_out/additional/0/https://primary.example.com",
            "dd_out/primary/0/https://primary.example.com",
        ]
    );
}

#[tokio::test]
async fn a_metrics_override_replaces_the_primary_recipient_for_metrics_only() {
    let resolved =
        resolved("observability_pipelines_worker: {metrics: {enabled: true, url: http://opw.example.com}}").await;
    assert_eq!(
        recipients(&resolved.outputs, "dd_metrics_encode"),
        ["dd_out/metrics_override/0/http://opw.example.com"]
    );
    // The primary still receives every other payload, so it stays in the native set; the override receives metrics
    // only, with the primary key.
    let primary = recipient(&resolved, "dd_out/primary/0/https://app.datadoghq.com");
    assert_eq!(primary.classes, PayloadClasses::All);
    let opw = recipient(&resolved, "dd_out/metrics_override/0/http://opw.example.com");
    assert_eq!(opw.classes, PayloadClasses::MetricsOnly);
    assert_eq!(opw.credential.as_str(), "native:api_key");
}

#[tokio::test]
async fn autoscaling_failover_accepts_only_scalar_series_on_its_own_host() {
    let resolved = resolved(
        "autoscaling: {failover: {enabled: true}}\ncluster_agent: {enabled: true, url: https://ca.example.com, \
         auth_token: t}",
    )
    .await;
    let outputs = &resolved.outputs;
    let output = outputs.get(AF_METRICS_ENCODE).unwrap();
    assert_eq!(
        output.decl.accepts,
        PortType {
            kinds: KindSet::of(&[Kind::Counter, Kind::Rate, Kind::Gauge, Kind::Set]),
            timestamps: TimestampForm::Mixed,
        }
    );
    assert_eq!(output.forwarder, Some(CLUSTER_AGENT_OUT));
    assert_eq!(
        recipients(outputs, AF_METRICS_ENCODE),
        ["cluster_agent_out/primary/0/https://ca.example.com"]
    );
    let ca = recipient(&resolved, "cluster_agent_out/primary/0/https://ca.example.com");
    assert_eq!(ca.credential.as_str(), "native:cluster_agent.auth_token");
    assert_eq!(decode_protocol(&ca.protocol).unwrap(), RecipientProtocol::ClusterAgent);
    // V2 series only.
    let encoder = decode_encoder(&binding(outputs, AF_METRICS_ENCODE).encoder).unwrap();
    assert!(encoder.variants.v2_series && !encoder.variants.v3_series);
    // Every other output accepts everything.
    for output in outputs
        .iter()
        .filter(|output| output.decl.id.as_str() != AF_METRICS_ENCODE)
    {
        assert_eq!(output.decl.accepts, PortType::ANY, "{}", output.decl.id);
    }
}

#[tokio::test]
async fn selectors_say_what_each_output_delivers_to() {
    let resolved = resolved(
        r#"
additional_endpoints:
  https://b.example.com: [key-b]
  https://c.example.com: [key-c]
multi_region_failover: {enabled: true, api_key: k, dd_url: https://mrf.example.com}
experimental:
  metrics_endpoint_routing:
    metric_allowlist:
      https://b.example.com: [orders.total]
      https://c.example.com: [orders.total]
"#,
    )
    .await;
    let role = |name: &str| Selector::Role(RoleName::new(name).unwrap());
    assert_eq!(binding(&resolved.outputs, DD_METRICS_ENCODE).selector, role("main"));
    // The group's selector is the SHA-256 digest of `["https://b.example.com","https://c.example.com"]`, computed with
    // `shasum -a 256`.
    assert_eq!(
        binding(&resolved.outputs, "metrics_routing_encode_0").selector,
        Selector::Group(
            "sha256:e3488d4119bf8279f7bf3bafd9aa915e0f5d08a6764fb956c239445e53804ddd"
                .parse()
                .unwrap()
        )
    );
    assert_eq!(
        binding(&resolved.outputs, MRF_METRICS_ENCODE).selector,
        role("multi_region_failover")
    );
    let mrf = recipient(&resolved, "mrf_dd_out/primary/0/https://mrf.example.com");
    assert_eq!(mrf.credential.as_str(), "native:multi_region_failover.api_key");
}

#[tokio::test]
async fn a_dual_ship_endpoint_or_a_rotated_key_changes_no_selector_and_a_renumbered_group_does() {
    let base = "dd_url: https://primary.example.com\nadditional_endpoints: {https://b.example.com: [key-b]}";
    let selectors = |outputs: &NativeOutputs| {
        outputs
            .iter()
            .map(|output| {
                let binding = output.decl.binding.as_ref();
                (
                    output.decl.id.to_string(),
                    output.decl.accepts,
                    binding.map(|binding| (binding.host.clone(), binding.selector.clone())),
                )
            })
            .collect::<Vec<_>>()
    };
    let before = selectors(&outputs(base).await);
    for unchanged in [
        // Another key for an endpoint, another endpoint, and a new primary all stay under the main role.
        "dd_url: https://primary.example.com\nadditional_endpoints: {https://b.example.com: [key-b, key-b2]}",
        "dd_url: https://primary.example.com\nadditional_endpoints: {https://b.example.com: [key-b], \
         https://c.example.com: [key-c]}",
        "dd_url: https://other.example.com\nadditional_endpoints: {https://b.example.com: [key-b]}",
        "dd_url: https://primary.example.com\nadditional_endpoints: {https://b.example.com: [key-b]}\napi_key: \
         rotated",
    ] {
        assert_eq!(selectors(&outputs(unchanged).await), before, "{}", unchanged);
    }

    // A group's selector is its endpoint set's: removing the first of two groups gives the second one's selector to
    // `metrics_routing_encode_0`.
    let two = r#"
additional_endpoints: {https://b.example.com: [key-b], https://c.example.com: [key-c]}
experimental:
  metrics_endpoint_routing:
    metric_allowlist: {https://b.example.com: [a], https://c.example.com: [b]}
"#;
    let one = r#"
additional_endpoints: {https://b.example.com: [key-b], https://c.example.com: [key-c]}
experimental:
  metrics_endpoint_routing:
    metric_allowlist: {https://c.example.com: [b]}
"#;
    let two = outputs(two).await;
    let one = outputs(one).await;
    let group = |outputs: &NativeOutputs, id: &str| outputs.get(id).map(|_| binding(outputs, id).selector.clone());
    assert_ne!(
        group(&two, "metrics_routing_encode_0"),
        group(&two, "metrics_routing_encode_1")
    );
    assert_eq!(
        group(&one, "metrics_routing_encode_0"),
        group(&two, "metrics_routing_encode_1"),
        "the remaining group keeps its selector under its new position"
    );
}

#[tokio::test]
async fn bindings_carry_each_outputs_encoder_its_hosts_settings_and_never_a_credentials_value() {
    let resolved = resolved(
        r#"
api_key: secret-primary-key
additional_endpoints: {https://dual.example.com: [secret-dual-key]}
use_v3_api: {series: {endpoints: {"https://dual.example.com": "false"}}}
serializer_max_series_points_per_payload: 1234
proxy: {https: "http://user:secret-password@proxy.example.com:3129"}
"#,
    )
    .await;
    let main = binding(&resolved.outputs, DD_METRICS_ENCODE);
    assert_eq!(main.host.as_str(), "dd_out");
    assert_eq!(main.telemetry.as_str(), "dd_metrics_encode");
    assert!(decode_params(ENCODER, &main.encoder).is_ok());
    let encoder = decode_encoder(&main.encoder).unwrap();
    assert_eq!(encoder.max_series_points_per_payload, 1234);
    // The primary, a Datadog intake, takes V3 series by default, and the dual-ship endpoint is configured for V2: the
    // main output produces both.
    assert!(encoder.variants.v2_series && encoder.variants.v3_series);
    let dual = recipient(&resolved, "dd_out/additional/0/https://dual.example.com");
    assert!(!decode_protocol(&dual.protocol).unwrap().v3().series);
    let primary = recipient(&resolved, "dd_out/primary/0/https://app.datadoghq.com");
    assert!(decode_protocol(&primary.protocol).unwrap().v3().series);

    let settings =
        super::specs::host_settings(resolved.resources.host(&HostId::new("dd_out").unwrap()).unwrap()).unwrap();
    let https = settings.proxy.https.expect("the https proxy is configured");
    assert_eq!(https.url, "http://proxy.example.com:3129/");
    assert_eq!(&*https.credential, "native:proxy.https");

    // The plan holds references only.
    let written = serde_json::to_string(&resolved.resources).unwrap()
        + &serde_json::to_string(&resolved.outputs.decls()).unwrap();
    for secret in ["secret-primary-key", "secret-dual-key", "secret-password"] {
        assert!(!written.contains(secret), "the plan holds `{}`", secret);
    }
}

#[tokio::test]
async fn outputs_follow_the_current_configuration_and_restart_only_settings_the_startup_one() {
    let startup = configuration("additional_endpoints: {https://b.example.com: [key-b]}").await;
    let current = configuration(
        r#"
additional_endpoints: {https://b.example.com: [key-b]}
experimental:
  metrics_endpoint_routing:
    metric_allowlist: {https://b.example.com: [a]}
multi_region_failover: {enabled: true, api_key: k, dd_url: https://mrf.example.com}
serializer_compressor_kind: zlib
serializer_max_series_payload_size: 1000
"#,
    )
    .await;
    let facts = facts(&startup);
    let hosts = FixedHosts::at_boot(&facts);
    let resolved = NativeResources::resolve(&startup, &current, &facts, &hosts).unwrap();
    // A routing group and the failover branch appear without a restart.
    assert_eq!(
        ids(&resolved.outputs),
        [
            "dsd_client_telemetry",
            "dd_metrics_encode",
            "metrics_routing_encode_0",
            "mrf_metrics_encode"
        ]
    );
    // A metrics-only limit follows the current configuration; the compressor, which every encoder reads, stays as
    // the process started with it.
    let encoder = decode_encoder(&binding(&resolved.outputs, DD_METRICS_ENCODE).encoder).unwrap();
    assert_eq!(encoder.max_series_payload_size, 1000);
    assert_eq!(
        &*encoder.compressor_kind,
        startup.shared.endpoints.compression.compressor_kind
    );
    assert_ne!(&*encoder.compressor_kind, "zlib");
}

#[test]
fn effective_shared_settings_keep_disk_persistence_and_storage_as_at_startup() {
    let mut startup = SharedConfiguration::default();
    startup.endpoints.forwarder.storage_max_size_in_bytes = 0;
    startup.endpoints.forwarder.storage_path = "/var/lib/startup".into();
    startup.metrics_encoding.flush_timeout = std::time::Duration::from_secs(2);
    let mut current = startup.clone();
    current.endpoints.forwarder.storage_max_size_in_bytes = 4096;
    current.endpoints.forwarder.storage_path = "/var/lib/current".into();
    current.endpoints.forwarder.timeout = 99;
    current.metrics_encoding.flush_timeout = std::time::Duration::from_secs(9);
    current.metrics_encoding.max_series_points_per_payload = 7;

    let effective = effective_shared(&startup, &current);
    // Turning persistence on needs a restart; so do the storage path and the flush timeout.
    assert_eq!(effective.endpoints.forwarder.storage_max_size_in_bytes, 0);
    assert_eq!(
        effective.endpoints.forwarder.storage_path,
        startup.endpoints.forwarder.storage_path
    );
    assert_eq!(
        effective.metrics_encoding.flush_timeout,
        std::time::Duration::from_secs(2)
    );
    // The rest follows the current configuration.
    assert_eq!(effective.endpoints.forwarder.timeout, 99);
    assert_eq!(effective.metrics_encoding.max_series_points_per_payload, 7);

    // A disk limit changes live between nonzero values.
    startup.endpoints.forwarder.storage_max_size_in_bytes = 1024;
    let effective = effective_shared(&startup, &current);
    assert_eq!(effective.endpoints.forwarder.storage_max_size_in_bytes, 4096);
}

/// The configuration the membership tests boot with: two keys for one additional endpoint, multi-region failover, and
/// autoscaling failover through the Cluster Agent.
const KEYED: &str = r#"
dd_url: https://primary.example.com
api_key: primary-key
additional_endpoints: {https://dual.example.com: [key-1, key-2]}
multi_region_failover: {enabled: true, api_key: mrf-key, dd_url: https://mrf.example.com}
autoscaling: {failover: {enabled: true}}
cluster_agent: {enabled: true, url: https://ca.example.com, auth_token: ca-token}
"#;

/// Resolves the resources of the configuration `yaml`, for a process booted with `boot`, following the resolution of
/// `previous`, which booted with `boot` too.
async fn resolved_after(boot: &str, previous: &NativeResources, yaml: &str) -> NativeResources {
    resolved_after_config(boot, previous, &configuration(yaml).await).await
}

/// Resolves the resources of the configuration `current` as [`resolved_after`] does.
async fn resolved_after_config(
    boot: &str, previous: &NativeResources, current: &SalukiConfiguration,
) -> NativeResources {
    let startup = configuration(boot).await;
    let facts = facts(&startup);
    NativeResources::resolve_after(
        &startup,
        current,
        &facts,
        &FixedHosts::at_boot(&facts),
        Demanded::default(),
        &previous.resources,
    )
    .unwrap()
}

fn recipient_ids(resolved: &NativeResources) -> Vec<String> {
    resolved
        .resources
        .recipients
        .iter()
        .map(|recipient| recipient.id.to_string())
        .collect()
}

#[tokio::test]
async fn a_key_rotated_to_a_blank_duplicate_or_invalid_value_keeps_every_recipient_and_output() {
    let before = resolved(KEYED).await;
    assert_eq!(
        recipient_ids(&before),
        [
            "cluster_agent_out/primary/0/https://ca.example.com",
            "dd_out/additional/0/https://dual.example.com",
            "dd_out/additional/1/https://dual.example.com",
            "dd_out/primary/0/https://primary.example.com",
            "mrf_dd_out/primary/0/https://mrf.example.com",
        ]
    );
    // Each is a credential's new value, as a configuration update may carry it, which the recipient's cell takes or
    // ignores with the native rules: none adds or removes a recipient, and none fails the lowering. A line feed cannot
    // be sent in a header.
    fn dual(config: &mut SalukiConfiguration) -> &mut Vec<String> {
        config
            .shared
            .endpoints
            .additional_endpoints
            .get_mut("https://dual.example.com")
            .unwrap()
    }
    type Rotate = fn(&mut SalukiConfiguration);
    let rotations: [(&str, Rotate); 8] = [
        ("a blank additional key", |config| dual(config)[0] = String::new()),
        ("a duplicate additional key", |config| {
            dual(config)[1] = "key-1".to_string()
        }),
        ("an invalid additional key", |config| {
            dual(config)[1] = "bad\nkey".to_string()
        }),
        ("a blank primary key", |config| {
            config.shared.endpoints.api_key = " ".to_string()
        }),
        ("an invalid primary key", |config| {
            config.shared.endpoints.api_key = "bad\nkey".to_string()
        }),
        ("a cleared failover key", |config| {
            config.domains.multi_region_failover.api_key = None
        }),
        ("an invalid failover key", |config| {
            config.domains.multi_region_failover.api_key = Some("bad\nkey".to_string())
        }),
        ("an invalid Cluster Agent token", |config| {
            config.shared.cluster_agent.auth_token = Some("bad\nkey".to_string())
        }),
    ];
    for (rotation, rotate) in rotations {
        let mut current = configuration(KEYED).await;
        rotate(&mut current);
        let after = resolved_after_config(KEYED, &before, &current).await;
        assert_eq!(after, before, "{}", rotation);
    }
}

#[tokio::test]
async fn a_key_position_or_branch_that_never_had_a_usable_key_joins_once_it_has_one() {
    // At startup, as natively: a blank key, and a key an earlier position holds, are no recipients, and failover
    // without a key is no branch.
    let boot = r#"
additional_endpoints: {https://dual.example.com: ["", key-2, key-2]}
multi_region_failover: {enabled: true, dd_url: https://mrf.example.com}
"#;
    let booted = resolved(boot).await;
    assert_eq!(
        recipient_ids(&booted),
        [
            "dd_out/additional/1/https://dual.example.com",
            "dd_out/primary/0/https://app.datadoghq.com",
        ]
    );
    assert!(booted.outputs.get(MRF_METRICS_ENCODE).is_none());

    // A blank position joins with a key of its own, not with the key a kept position holds; failover joins with its
    // key.
    let keyed = r#"
additional_endpoints: {https://dual.example.com: [key-0, key-2, key-2]}
multi_region_failover: {enabled: true, api_key: mrf-key, dd_url: https://mrf.example.com}
"#;
    let joined = resolved_after(boot, &booted, keyed).await;
    assert_eq!(
        recipient_ids(&joined),
        [
            "dd_out/additional/0/https://dual.example.com",
            "dd_out/additional/1/https://dual.example.com",
            "dd_out/primary/0/https://app.datadoghq.com",
            "mrf_dd_out/primary/0/https://mrf.example.com",
        ]
    );
    let duplicate = "additional_endpoints: {https://dual.example.com: [key-2, key-2, key-2]}";
    assert_eq!(
        recipient_ids(&resolved_after(boot, &booted, duplicate).await),
        [
            "dd_out/additional/1/https://dual.example.com",
            "dd_out/primary/0/https://app.datadoghq.com",
        ]
    );

    // An invalid Cluster Agent token is no recipient, and fails no lowering, until it can be sent.
    let ca = "autoscaling: {failover: {enabled: true}}\ncluster_agent: {enabled: true, url: https://ca.example.com, \
              auth_token: t}";
    let ca_booted = resolved(ca).await;
    let invalid = "autoscaling: {failover: {enabled: true}}\ncluster_agent: {enabled: true, url: \
                   https://ca.example.com, auth_token: \"bad\\nkey\"}";
    let fresh = resolved_after(ca, &NativeResources::default(), invalid).await;
    assert!(fresh.outputs.get(AF_METRICS_ENCODE).is_none());
    assert_eq!(resolved_after(ca, &fresh, ca).await, ca_booted);
}

#[tokio::test]
async fn a_removed_key_position_or_branch_leaves_whatever_its_key() {
    let before = resolved(KEYED).await;
    let removed = r#"
dd_url: https://primary.example.com
api_key: primary-key
additional_endpoints: {https://dual.example.com: [key-1]}
multi_region_failover: {enabled: false, api_key: mrf-key, dd_url: https://mrf.example.com}
autoscaling: {failover: {enabled: true}}
cluster_agent: {enabled: true, url: https://ca.example.com}
"#;
    let after = resolved_after(KEYED, &before, removed).await;
    // The second key position is gone, failover is disabled, and the Cluster Agent has no token any more.
    assert_eq!(
        recipient_ids(&after),
        [
            "dd_out/additional/0/https://dual.example.com",
            "dd_out/primary/0/https://primary.example.com",
        ]
    );
    assert!(after.outputs.get(MRF_METRICS_ENCODE).is_none());
    assert!(after.outputs.get(AF_METRICS_ENCODE).is_none());
}

/// Returns the recipient directory of `resolved` as `(name, host, recipients, endpoint)`, in name order.
fn directory(resolved: &NativeResources) -> Vec<(String, String, Vec<String>, String)> {
    resolved
        .directory
        .iter()
        .map(|(name, entry)| {
            (
                name.to_string(),
                entry.host.to_string(),
                entry.recipients.iter().map(ToString::to_string).collect(),
                entry.endpoint.clone(),
            )
        })
        .collect()
}

#[tokio::test]
async fn the_directory_names_the_primary_endpoint_failover_and_each_additional_endpoint_by_its_url() {
    let named = resolved(
        r#"
dd_url: https://primary.example.com
additional_endpoints: {"https://b.example.com": [key-b, key-b2], "https://b.example.com/": [key-b3], "http://127.0.0.1:17102": [key-c]}
experimental: {metrics_endpoint_routing: {metric_allowlist: {"https://b.example.com": [b]}}}
multi_region_failover: {enabled: true, api_key: k, dd_url: https://mrf.example.com}
"#,
    )
    .await;
    let entry = |name: &str, host: &str, ids: &[&str], endpoint: &str| {
        (
            name.to_string(),
            host.to_string(),
            ids.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
            endpoint.to_string(),
        )
    };
    assert_eq!(
        directory(&named),
        [
            // An additional endpoint's resolved URL names every API key's recipient of it, whether or not endpoint
            // routing sends it anything, under every spelling of it: two spellings are two endpoints natively, each
            // with its own recipients, and one name.
            entry(
                "http://127.0.0.1:17102",
                "dd_out",
                &["dd_out/additional/0/http://127.0.0.1:17102"],
                "http://127.0.0.1:17102"
            ),
            entry(
                "https://b.example.com",
                "dd_out",
                &[
                    "dd_out/additional/0/https://b.example.com",
                    "dd_out/additional/0/https://b.example.com/",
                    "dd_out/additional/1/https://b.example.com"
                ],
                "https://b.example.com"
            ),
            entry(
                "mrf",
                "mrf_dd_out",
                &["mrf_dd_out/primary/0/https://mrf.example.com"],
                "https://mrf.example.com"
            ),
            entry(
                "primary",
                "dd_out",
                &["dd_out/primary/0/https://primary.example.com"],
                "https://primary.example.com"
            ),
        ]
    );
    // The directory never holds a credential.
    let written = format!("{:?}", named.directory);
    for key in ["key-b", "key-c", "\"k\""] {
        assert!(!written.contains(key), "the directory carries `{}`", key);
    }

    // Endpoints configured without a scheme are sent to over `https`, and named so: an endpoint named like a recipient
    // lowering names (`mrf`, `primary`) never takes its name, nor merges with it.
    let scheme_less = resolved(
        r#"
additional_endpoints: {mrf: [k1], primary: [k2], team_d: [k3]}
multi_region_failover: {enabled: true, api_key: k, dd_url: https://mrf.example.com}
"#,
    )
    .await;
    let names = directory(&scheme_less)
        .into_iter()
        .map(|(name, host, ids, ..)| (name, host, ids.len()))
        .collect::<Vec<_>>();
    let name = |name: &str, host: &str| (name.to_string(), host.to_string(), 1);
    assert_eq!(
        names,
        [
            name("https://mrf", "dd_out"),
            name("https://primary", "dd_out"),
            name("https://team_d", "dd_out"),
            name("mrf", "mrf_dd_out"),
            name("primary", "dd_out"),
        ]
    );

    // With an alternate metrics intake, `primary` is where metrics go.
    let overridden =
        resolved("observability_pipelines_worker: {metrics: {enabled: true, url: http://opw.example.com}}").await;
    assert_eq!(
        directory(&overridden),
        [entry(
            "primary",
            "dd_out",
            &["dd_out/metrics_override/0/http://opw.example.com"],
            "http://opw.example.com"
        )]
    );
}

#[tokio::test]
async fn the_cluster_agent_is_available_once_forwarding_is_configured_and_idle_until_an_output_names_it() {
    let cluster_agent = |resolved: &NativeResources| {
        resolved
            .directory
            .get(&RecipientName::new("cluster_agent").unwrap())
            .map(|entry| entry.recipients.iter().map(ToString::to_string).collect::<Vec<_>>())
    };
    let id = "cluster_agent_out/primary/0/https://ca.example.com".to_string();
    // Requested: the branch's output names it, so the plan declares it.
    let requested = resolved(
        "autoscaling: {failover: {enabled: true, metrics: [m]}}\ncluster_agent: {enabled: true, url: \
         https://ca.example.com, auth_token: t}",
    )
    .await;
    assert_eq!(cluster_agent(&requested), Some(vec![id.clone()]));
    assert!(requested
        .resources
        .recipient(&RecipientId::new(id.as_str()).unwrap())
        .is_some());

    // Not requested: available by name, with no output and no recipient declared, so no recipient runs.
    let available = resolved("cluster_agent: {enabled: true, url: https://ca.example.com, auth_token: t}").await;
    assert_eq!(cluster_agent(&available), Some(vec![id.clone()]));
    assert!(available.outputs.get(AF_METRICS_ENCODE).is_none());
    assert!(available
        .resources
        .recipient(&RecipientId::new(id.as_str()).unwrap())
        .is_none());

    // Not configured: forwarding off, or no token.
    for yaml in [
        "cluster_agent: {url: https://ca.example.com, auth_token: t}",
        "cluster_agent: {enabled: true, url: https://ca.example.com}",
    ] {
        assert_eq!(cluster_agent(&resolved(yaml).await), None, "{}", yaml);
    }
}

#[test]
fn a_name_never_stands_for_recipients_of_two_hosts() {
    let name = || RecipientName::new("mrf").unwrap();
    let id = |id: &str| RecipientId::new(id).unwrap();
    let mut directory = Directory::new();
    name_recipient(&mut directory, name(), &MRF_HOST, id("mrf_dd_out/a"), "https://a").unwrap();
    // The same host merges, deterministically; another host is refused as the bug it would be.
    name_recipient(&mut directory, name(), &MRF_HOST, id("mrf_dd_out/b"), "https://a").unwrap();
    assert_eq!(directory[&name()].recipients.len(), 2);
    let error = name_recipient(&mut directory, name(), &DD_HOST, id("dd_out/c"), "https://c").unwrap_err();
    assert!(error.to_string().contains("this is a lowering bug"), "{}", error);
    assert_eq!(directory[&name()].recipients.len(), 2);
}

/// Resolves the resources of the configuration `yaml` with the demand `demand`, following the resolution `previous`.
async fn resolved_with_demand(yaml: &str, demand: &[Demand], previous: &NativeResources) -> NativeResources {
    let startup = configuration(yaml).await;
    let facts = facts(&startup);
    NativeResources::resolve_after(
        &startup,
        &startup,
        &facts,
        &FixedHosts::at_boot(&facts),
        Demanded {
            demand,
            previous: &previous.outputs,
        },
        &previous.resources,
    )
    .unwrap()
}

/// The demand of an `emit` with local ID `emit` addressed to the directory names `names` of `resolved`.
fn demand_of(resolved: &NativeResources, names: &[&str], emit: &str) -> Demand {
    let entry = |name: &str| &resolved.directory[&name.parse::<RecipientName>().unwrap()];
    let mut recipients = names
        .iter()
        .flat_map(|name| entry(name).recipients.clone())
        .collect::<Vec<_>>();
    recipients.sort();
    recipients.dedup();
    Demand {
        host: entry(names[0]).host.clone(),
        recipients,
        emit: emit.parse().unwrap(),
    }
}

/// Returns each implied output of `resolved`: its ID and recipients, in ID order.
fn implied(resolved: &NativeResources) -> Vec<(String, Vec<RecipientId>)> {
    let mut implied = resolved
        .outputs
        .iter()
        .filter(|output| matches!(output.kind, OutputKind::Implied { .. }))
        .map(|output| (output.decl.id.to_string(), output.decl.recipients.clone()))
        .collect::<Vec<_>>();
    implied.sort();
    implied
}

/// Two additional endpoints, `b` with one key and `c` with two, and Multi-Region Failover, without endpoint routing.
const TEAMS: &str = r#"
dd_url: https://primary.example.com
api_key: primary-key
additional_endpoints: {https://b.example.com: [key-b], https://c.example.com: [key-c1, key-c2]}
multi_region_failover: {enabled: true, api_key: mrf-key, dd_url: https://mrf.example.com}
"#;

#[tokio::test]
async fn lowering_adds_an_implied_output_for_each_set_of_recipients_no_output_delivers_to() {
    let native = resolved(TEAMS).await;
    // The main output dual-ships to every endpoint: `[primary, b, c]` is its set, and needs no implied output.
    let main = demand_of(
        &native,
        &["primary", "https://b.example.com", "https://c.example.com"],
        "metrics_policy_0",
    );
    assert_eq!(
        main.recipients,
        native.outputs.get(DD_METRICS_ENCODE).unwrap().decl.recipients
    );
    let to_b = demand_of(&native, &["https://b.example.com"], "to_team_b_true");
    let to_c = demand_of(&native, &["https://c.example.com"], "to_team_c_true");
    let primary = demand_of(&native, &["primary"], "metrics_policy_0");
    let demand = [main, to_b.clone(), to_c.clone(), primary.clone()];
    let resolved = resolved_with_demand(TEAMS, &demand, &NativeResources::default()).await;

    // One implied output per set no output delivers to, named after its first emit and the set's digest, never after
    // a recipient name, with the recipients as its selector.
    assert_eq!(
        implied(&resolved),
        [
            (
                "implied_metrics_policy_0_dbd87652".to_string(),
                primary.recipients.clone()
            ),
            ("implied_to_team_b_true_e7b845f5".to_string(), to_b.recipients.clone()),
            ("implied_to_team_c_true_ff043b9d".to_string(), to_c.recipients.clone()),
        ]
    );
    let output = resolved.outputs.get("implied_to_team_c_true_ff043b9d").unwrap();
    let binding = output.decl.binding.as_ref().unwrap();
    assert_eq!(binding.selector, Selector::Recipients(to_c.recipients.clone()));
    assert_eq!(binding.host.as_str(), DD_OUT);
    assert_eq!(output.recipients.len(), 2, "both of `c`'s keys");
    assert!(resolved.refused_demand.is_empty());

    // Lowered again with one more set, the implied outputs keep their IDs, whatever the new set's emit is called.
    let renamed = Demand {
        emit: "aaa_first_true".parse().unwrap(),
        ..to_b.clone()
    };
    let again = resolved_with_demand(TEAMS, &[renamed, to_c.clone()], &resolved).await;
    assert_eq!(
        implied(&again),
        [
            ("implied_to_team_b_true_e7b845f5".to_string(), to_b.recipients.clone()),
            ("implied_to_team_c_true_ff043b9d".to_string(), to_c.recipients.clone()),
        ]
    );
    // An emit whose recipients change asks for another set, which gets another ID, though its emit is the same: an
    // ID's telemetry always means one set of recipients.
    let widened = demand_of(
        &native,
        &["https://b.example.com", "https://c.example.com"],
        "to_team_b_true",
    );
    let changed = resolved_with_demand(TEAMS, std::slice::from_ref(&widened), &again).await;
    let ids = implied(&changed);
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0].1, widened.recipients);
    assert!(
        ids[0].0.starts_with("implied_to_team_b_true_") && ids[0].0 != "implied_to_team_b_true_e7b845f5",
        "{:?}",
        ids
    );

    // Without demand, none: an implied output retires once no program asks for it.
    assert!(implied(&resolved_with_demand(TEAMS, &[], &again).await).is_empty());
}

/// Six additional endpoints, `e0` to `e5`, one key each, beside the primary endpoint: enough sets of recipients for
/// more implied outputs than the limit.
const SIX_ENDPOINTS: &str = r#"
dd_url: https://primary.example.com
api_key: primary-key
additional_endpoints: {https://e0.example.com: [k0], https://e1.example.com: [k1], https://e2.example.com: [k2],
  https://e3.example.com: [k3], https://e4.example.com: [k4], https://e5.example.com: [k5]}
"#;

#[tokio::test]
async fn implied_outputs_are_refused_beyond_the_limit_and_off_the_main_outputs_host() {
    // Seventeen sets no output delivers to: each endpoint alone, the primary endpoint alone, and pairs of endpoints.
    let six = resolved(SIX_ENDPOINTS).await;
    let endpoints = (0..6)
        .map(|index| format!("https://e{}.example.com", index))
        .collect::<Vec<_>>();
    let mut sets = vec![vec!["primary".to_string()]];
    sets.extend(endpoints.iter().map(|endpoint| vec![endpoint.clone()]));
    for (first, endpoint) in endpoints.iter().enumerate() {
        for other in &endpoints[first + 1..] {
            sets.push(vec![endpoint.clone(), other.clone()]);
        }
    }
    sets.truncate(17);
    let demand = sets
        .iter()
        .enumerate()
        .map(|(index, names)| {
            let names = names.iter().map(String::as_str).collect::<Vec<_>>();
            demand_of(&six, &names, &format!("e_{:02}", index))
        })
        .collect::<Vec<_>>();
    assert!(demand.iter().all(|wanted| !six
        .outputs
        .iter()
        .any(|output| output.decl.recipients == wanted.recipients)));
    // Sixteen fit. The seventeenth, last in recipient order, is refused, with why.
    let beyond = resolved_with_demand(SIX_ENDPOINTS, &demand, &NativeResources::default()).await;
    assert_eq!(implied(&beyond).len(), 16);
    assert_eq!(beyond.refused_demand.len(), 1, "{:?}", beyond.refused_demand);
    let why = beyond.refused_demand.values().next().unwrap();
    assert!(why.contains("one more than the limit of 16 outputs"), "{}", why);
    assert!(why.contains("every program the installer holds"), "{}", why);
    assert!(!why.contains("  "), "{}", why);
    // Sixteen alone fit.
    let fits = resolved_with_demand(SIX_ENDPOINTS, &demand[..16], &NativeResources::default()).await;
    assert_eq!(implied(&fits).len(), 16);
    assert!(fits.refused_demand.is_empty());

    let native = resolved(TEAMS).await;
    let mut both = demand_of(&native, &["https://b.example.com", "https://c.example.com"], "e_bc");

    // Recipients of another host than the main output's are refused, with why.
    let mrf = demand_of(&native, &["mrf"], "e_mrf");
    assert!(native
        .outputs
        .iter()
        .any(|output| output.decl.recipients == mrf.recipients));
    let mut off_host = mrf.clone();
    off_host.recipients.push("mrf_dd_out/other/0".parse().unwrap());
    let refused = resolved_with_demand(TEAMS, &[off_host.clone()], &NativeResources::default()).await;
    assert!(
        refused.refused_demand[&off_host.recipients].contains("recipients of `mrf_dd_out`"),
        "{:?}",
        refused.refused_demand
    );

    // So is a set that is not every key of its endpoints, which one encoder cannot send to alone.
    both.recipients.pop();
    let partial = resolved_with_demand(TEAMS, &[both.clone()], &NativeResources::default()).await;
    assert!(partial.refused_demand.contains_key(&both.recipients));
}
