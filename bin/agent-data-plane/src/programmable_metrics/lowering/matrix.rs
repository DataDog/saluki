//! The configuration matrix the lowering tests run over.
//!
//! Each entry is a YAML configuration, merged over the standalone base configuration of
//! [`test_support`](crate::programmable_metrics::test_support). Together they walk every presence gate of the native
//! metrics path: the DogStatsD pipeline with and without the no-aggregation pipeline, every source of metrics
//! enrichment alone and together (including the trace sampler without the OTLP source, in proxy mode with traces not
//! proxied), connected and standalone mode, host tags, endpoint routing groups (none, one, two, a shared group, a
//! selected endpoint with empty lists, a policy on the primary endpoint), multi-region and autoscaling failover wired
//! or not, and every DogStatsD parameter that lowering converts.

/// The configuration of a process with every optional branch of the native metrics path wired.
pub(in crate::programmable_metrics) const EVERY_BRANCH: &str = r#"
data_plane:
  standalone_mode: false
  checks:
    enabled: true
  otlp:
    enabled: true
dd_url: https://primary.example.com
additional_endpoints:
  https://b.example.com: [key-b1, key-b2]
  https://c.example.com: [key-c]
experimental:
  metrics_endpoint_routing:
    metric_allowlist:
      https://b.example.com: [orders.total]
    metric_prefix_allowlist:
      https://b.example.com: [checkout.]
      https://c.example.com: [catalog.]
multi_region_failover:
  enabled: true
  api_key: mrf-key
  dd_url: https://mrf.example.com
autoscaling:
  failover:
    enabled: true
    metrics: [requests.count]
cluster_agent:
  enabled: true
  url: https://cluster-agent.example.com:5005
  auth_token: cluster-agent-token
expected_tags_duration: 5m
dogstatsd_mapper_profiles:
  - name: checkout
    prefix: raw.
    mappings:
      - match: raw.requests.*
        name: mapped.requests
        tags:
          route: $1
statsd_metric_namespace: tenant
statsd_metric_namespace_blacklist: [internal., system.]
metric_filterlist: [tenant.raw.blocked, tenant.checkout.latency.max]
metric_tag_filterlist:
  - metric_name: tenant.checkout.latency
    action: exclude
    tags: [user_id]
metric_tag_value_allowlist:
  - metric_prefix: tenant.checkout.
    tag_name: env
    values: [prod, staging]
    on_miss: replace
"#;

/// The configuration matrix: a name, and the configuration.
pub(in crate::programmable_metrics) const MATRIX: &[(&str, &str)] = &[
    ("dogstatsd with defaults", ""),
    (
        "no-aggregation pipeline off",
        "dogstatsd_no_aggregation_pipeline: false",
    ),
    (
        "otlp without dogstatsd",
        "data_plane: {dogstatsd: {enabled: false}, otlp: {enabled: true}}",
    ),
    (
        "otlp proxy mode, connected",
        "data_plane: {standalone_mode: false, dogstatsd: {enabled: false}, otlp: {enabled: true, proxy: {enabled: \
         true}}}",
    ),
    (
        "otlp proxy mode with traces not proxied",
        "data_plane: {dogstatsd: {enabled: false}, otlp: {enabled: true, proxy: {enabled: true, traces: {enabled: \
         false}}}}",
    ),
    ("checks", "data_plane: {checks: {enabled: true}}"),
    (
        "checks without dogstatsd",
        "data_plane: {dogstatsd: {enabled: false}, checks: {enabled: true}}",
    ),
    ("connected without host tags", "data_plane: {standalone_mode: false}"),
    (
        "connected with host tags",
        "data_plane: {standalone_mode: false}\nexpected_tags_duration: 5m",
    ),
    (
        "one routing group",
        r#"
dd_url: https://primary.example.com
additional_endpoints: {https://b.example.com: [key-b]}
experimental: {metrics_endpoint_routing: {metric_allowlist: {https://b.example.com: [orders.total]}}}
"#,
    ),
    (
        "two routing groups, a shared group, and an endpoint with empty lists",
        r#"
dd_url: https://primary.example.com
additional_endpoints:
  https://b.example.com: [key-b]
  https://c.example.com: [key-c]
  https://d.example.com: [key-d]
  https://e.example.com: [key-e]
experimental:
  metrics_endpoint_routing:
    metric_allowlist:
      https://b.example.com: [orders.total]
      https://c.example.com: [orders.total]
      https://d.example.com: []
    metric_prefix_allowlist:
      https://e.example.com: [catalog.]
"#,
    ),
    (
        "a routing policy on the primary endpoint",
        r#"
dd_url: https://primary.example.com
additional_endpoints: {https://b.example.com: [key-b]}
experimental: {metrics_endpoint_routing: {metric_prefix_allowlist: {https://primary.example.com: [app.]}}}
"#,
    ),
    (
        "multi-region failover",
        "multi_region_failover: {enabled: true, api_key: mrf-key, dd_url: https://mrf.example.com}",
    ),
    (
        "multi-region failover without an api key",
        "multi_region_failover: {enabled: true, dd_url: https://mrf.example.com}",
    ),
    (
        "autoscaling failover",
        r#"
autoscaling: {failover: {enabled: true, metrics: [requests.count]}}
cluster_agent: {enabled: true, url: https://cluster-agent.example.com:5005, auth_token: cluster-agent-token}
"#,
    ),
    (
        "autoscaling failover without a cluster agent",
        "autoscaling: {failover: {enabled: true, metrics: [requests.count]}}",
    ),
    (
        "a cluster agent without autoscaling failover",
        "cluster_agent: {enabled: true, url: https://cluster-agent.example.com:5005, auth_token: cluster-agent-token}",
    ),
    (
        "mapper profiles, a namespace prefix, and filterlists",
        r#"
dogstatsd_mapper_profiles:
  - name: wildcards
    prefix: raw.
    mappings:
      - {match: raw.requests.*, name: mapped.requests, tags: {route: $1, kind: request}}
      - {match: raw.errors.*, match_type: wildcard, name: mapped.errors}
  - name: regexes
    prefix: re.
    mappings:
      - {match: 're\.(\w+)\.latency', match_type: regex, name: latency, tags: {service: $1}}
dogstatsd_mapper_cache_size: 0
statsd_metric_namespace: tenant.
statsd_metric_namespace_blacklist: [internal.]
metric_filterlist: [tenant.blocked., tenant.requests.max]
metric_filterlist_match_prefix: true
metric_tag_filterlist:
  - {metric_name: tenant.requests, action: include, tags: [env]}
  - {metric_name: tenant.latency, tags: [user_id]}
metric_tag_value_allowlist:
  - {metric_prefix: tenant., tag_name: env, values: [prod]}
"#,
    ),
    (
        "the legacy metric blocklist",
        "statsd_metric_blocklist: [blocked.metric]\nstatsd_metric_blocklist_match_prefix: true",
    ),
    (
        "a metrics override intake",
        "observability_pipelines_worker: {metrics: {enabled: true, url: http://opw.example.com:8080}}",
    ),
    (
        "the primary endpoint repeated as an additional endpoint",
        r#"
dd_url: https://primary.example.com
additional_endpoints: {https://primary.example.com: [key-again]}
experimental: {metrics_endpoint_routing: {metric_allowlist: {https://primary.example.com: [orders.total]}}}
"#,
    ),
    (
        "repeated histogram statistics",
        "histogram_aggregates: [max, max, count]\nhistogram_percentiles: ['0.95', '0.951']",
    ),
    (
        "aggregation settings",
        r#"
aggregate_window_duration_seconds: 60
aggregate_flush_interval: 1500ms
aggregate_context_limit: 5000
dogstatsd_expiry_seconds: 0
dogstatsd_flush_incomplete_buckets: true
histogram_copy_to_distribution: true
histogram_copy_to_distribution_prefix: dist.
"#,
    ),
    ("every branch", EVERY_BRANCH),
];

/// Returns the configuration of the matrix entry named `name`.
#[track_caller]
pub(in crate::programmable_metrics) fn matrix_entry(name: &str) -> &'static str {
    MATRIX
        .iter()
        .find(|(entry, _)| *entry == name)
        .map(|(_, yaml)| *yaml)
        .unwrap_or_else(|| panic!("no matrix entry `{}`", name))
}
