use saluki_components::forwarders::MetricsRecipientRoute;

use super::*;
use crate::programmable_metrics::test_support::{configuration, facts};

async fn outputs(yaml: &str) -> NativeOutputs {
    let config = configuration(yaml).await;
    NativeOutputs::from_configuration(&config, &facts(&config)).unwrap()
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

#[test]
fn recipient_identities_spell_out_plain_endpoints_only() {
    let recipient = |route, endpoint: &str, api_key_index| MetricsRecipient {
        route,
        configured_endpoint: endpoint.to_string(),
        api_key_index,
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
        (
            recipient(MetricsRecipientRoute::Primary, "https://exämple.com", None),
            format!("dd_out/primary/0/{}", digest("https://exämple.com")),
        ),
        (
            recipient(MetricsRecipientRoute::Primary, "https://a b.com", None),
            format!("dd_out/primary/0/{}", digest("https://a b.com")),
        ),
        (
            recipient(MetricsRecipientRoute::Primary, &long, None),
            format!("dd_out/primary/0/{}", digest(&long)),
        ),
        // An endpoint spelled like a digest is written as one, so the two forms never collide.
        (
            recipient(MetricsRecipientRoute::Primary, "sha256:0123", None),
            format!("dd_out/primary/0/{}", digest("sha256:0123")),
        ),
    ];
    for (recipient, expected) in cases {
        assert_eq!(recipient_id(DD_OUT, &recipient).to_string(), expected);
    }
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
    let cases: [(&str, &[&str]); 8] = [
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
    ];
    for (yaml, expected) in cases {
        assert_eq!(ids(&outputs(yaml).await), expected, "{}", yaml);
    }
}

#[tokio::test]
async fn recipients_follow_native_endpoint_routing() {
    let outputs = outputs(
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
    // b and c share one group; d is selected with empty lists, so it is removed from the main output and gets no
    // group; e has no policy and receives the main output.
    assert_eq!(
        ids(&outputs),
        ["dsd_client_telemetry", "dd_metrics_encode", "metrics_routing_encode_0"]
    );
    assert_eq!(
        recipients(&outputs, "dd_metrics_encode"),
        [
            "dd_out/additional/0/https://e.example.com",
            "dd_out/primary/0/https://primary.example.com",
        ]
    );
    assert_eq!(
        recipients(&outputs, "metrics_routing_encode_0"),
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
async fn a_metrics_override_replaces_the_primary_recipient() {
    let outputs =
        outputs("observability_pipelines_worker: {metrics: {enabled: true, url: http://opw.example.com}}").await;
    assert_eq!(
        recipients(&outputs, "dd_metrics_encode"),
        ["dd_out/metrics_override/0/http://opw.example.com"]
    );
}

#[tokio::test]
async fn autoscaling_failover_accepts_only_scalar_series() {
    let outputs = outputs(
        "autoscaling: {failover: {enabled: true}}\ncluster_agent: {enabled: true, url: https://ca.example.com, \
         auth_token: t}",
    )
    .await;
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
        recipients(&outputs, AF_METRICS_ENCODE),
        ["cluster_agent_out/primary/0/https://ca.example.com"]
    );
    // Every other output accepts everything.
    for output in outputs
        .iter()
        .filter(|output| output.decl.id.as_str() != AF_METRICS_ENCODE)
    {
        assert_eq!(output.decl.accepts, PortType::ANY, "{}", output.decl.id);
    }
}

#[tokio::test]
async fn fingerprints_change_with_where_an_output_delivers() {
    let base = "dd_url: https://primary.example.com\nadditional_endpoints: {https://b.example.com: [key-b]}";
    let fingerprint = |outputs: &NativeOutputs| outputs.get(DD_METRICS_ENCODE).unwrap().decl.fingerprint;
    let before = fingerprint(&outputs(base).await);

    // Encoder tuning does not change where the output delivers.
    let tuned = outputs(&format!("{}\nserializer_max_payload_size: 1000", base)).await;
    assert_eq!(fingerprint(&tuned), before);

    // A new recipient, a recipient removed by routing, and another endpoint do.
    for changed in [
        "dd_url: https://primary.example.com\nadditional_endpoints: {https://b.example.com: [key-b, key-b2]}",
        "dd_url: https://primary.example.com\nadditional_endpoints: {https://b.example.com: [key-b]}\nexperimental: \
         {metrics_endpoint_routing: {metric_allowlist: {https://b.example.com: [a]}}}",
        "dd_url: https://other.example.com\nadditional_endpoints: {https://b.example.com: [key-b]}",
    ] {
        assert_ne!(fingerprint(&outputs(changed).await), before, "{}", changed);
    }

    // Rotating an API key changes nothing: keys are not part of recipient identity.
    let rotated = outputs(&format!("{}\napi_key: rotated-key", base)).await;
    assert_eq!(fingerprint(&rotated), before);
}
