//! The plan-driven topology against the legacy one, running: the same input into the same sources, and every output
//! compared point by point.
//!
//! Each configuration of [`DIFFERENTIAL_MATRIX`] builds both primary topologies with `run.rs`'s builders, replaces the
//! sources with scripted ones, DogStatsD aggregation with native aggregation on a test clock, and every metrics
//! encoder with a collector (see [`test_topology`](crate::programmable_metrics::test_topology)), feeds both the same
//! DogStatsD-like and OTLP-like batches, stops them, which drains them and makes aggregation flush once, and compares
//! the multisets of flattened points each encoder received with the plan crate's equivalence oracle.
//!
//! [`GATEWAY_MATRIX`] does the same for the gateways in front of the outputs, which the legacy topology runs as
//! components of their own and the plan-driven one as `split`s of its policy region: zero, one, and two endpoint
//! routing groups (with a shared group and an endpoint with empty lists), Multi-Region Failover mirroring everything,
//! its allowlist, or nothing, and autoscaling failover, over input whose names the gateways' rules tell apart.
//!
//! Two native behaviors would make a comparison depend on timing rather than on what each path computes, and the
//! inputs avoid them: native filters reorder events within a batch (amendment 2), so no gauge context repeats within a
//! batch, where the last value wins; and the native no-aggregation split holds timestamped metrics until it has been
//! idle for `aggregate_passthrough_idle_flush_timeout`, discarding them if it stops sooner (amendment 12), so every
//! configuration sets that timeout to zero, which the plan-driven path does not read.

use std::collections::{BTreeMap, BTreeSet};

use saluki_core::data_model::event::metric::{context::Context, Metric};
use saluki_metric_plan::equivalence::compare;
use stringtheory::MetaString;

use crate::programmable_metrics::{
    test_support::layered_configuration_system,
    test_topology::{output_points, Aggregation, Delivery, MetricsPath, Options, TestTopology},
};

/// What every configuration of the matrix adds, so that the native no-aggregation split sends what it holds when it
/// stops.
const FLUSH_PASSTHROUGH_ON_STOP: &str = "aggregate_passthrough_idle_flush_timeout: 0s";

/// The configuration matrix the running topologies are compared over: a name, and the configuration.
const DIFFERENTIAL_MATRIX: &[(&str, &str)] = &[
    ("dogstatsd with defaults", ""),
    (
        "no-aggregation pipeline off",
        "dogstatsd_no_aggregation_pipeline: false",
    ),
    ("a mapper profile", MAPPER),
    ("a namespace prefix and filterlists", PREFIX_AND_FILTERLISTS),
    ("a tag filterlist and a tag value allowlist", TAG_FILTERLIST),
    ("one routing group", ONE_ROUTING_GROUP),
    ("two routing groups", TWO_ROUTING_GROUPS),
    ("multi-region failover", MULTI_REGION_FAILOVER),
    ("otlp beside dogstatsd", "data_plane: {otlp: {enabled: true}}"),
    (
        "otlp without dogstatsd",
        "data_plane: {dogstatsd: {enabled: false}, otlp: {enabled: true}}",
    ),
    ("every branch", EVERY_BRANCH),
    (
        "every branch without the no-aggregation pipeline",
        EVERY_BRANCH_NO_AGGREGATION_PIPELINE_OFF,
    ),
];

const MAPPER: &str = r#"
dogstatsd_mapper_profiles:
  - name: checkout
    prefix: raw.
    mappings:
      - match: raw.requests.*
        name: mapped.requests
        tags:
          route: $1
"#;

const PREFIX_AND_FILTERLISTS: &str = r#"
statsd_metric_namespace: tenant
statsd_metric_namespace_blacklist: [internal.]
metric_filterlist: [tenant.raw.blocked, tenant.request.duration.max]
"#;

const TAG_FILTERLIST: &str = r#"
metric_tag_filterlist:
  - metric_name: checkout.latency
    action: exclude
    tags: [user_id]
  - metric_name: requests.count
    action: include
    tags: [env]
metric_tag_value_allowlist:
  - metric_prefix: checkout.
    tag_name: env
    values: [prod, staging]
    on_miss: replace
"#;

const ONE_ROUTING_GROUP: &str = r#"
dd_url: https://primary.example.com
additional_endpoints: {https://b.example.com: [key-b]}
experimental: {metrics_endpoint_routing: {metric_allowlist: {https://b.example.com: [orders.total]}}}
"#;

const TWO_ROUTING_GROUPS: &str = r#"
dd_url: https://primary.example.com
additional_endpoints:
  https://b.example.com: [key-b]
  https://c.example.com: [key-c]
experimental:
  metrics_endpoint_routing:
    metric_allowlist:
      https://b.example.com: [orders.total]
    metric_prefix_allowlist:
      https://c.example.com: [catalog.]
"#;

const MULTI_REGION_FAILOVER: &str = r#"
multi_region_failover:
  enabled: true
  api_key: mrf-key
  dd_url: https://mrf.example.com
  failover_metrics: true
  metric_allowlist: [orders.total, otel.requests]
"#;

/// Every optional branch a standalone process can wire, with every DogStatsD parameter.
const EVERY_BRANCH: &str = r#"
data_plane:
  checks:
    enabled: true
  otlp:
    enabled: true
dd_url: https://primary.example.com
additional_endpoints:
  https://b.example.com: [key-b]
  https://c.example.com: [key-c]
experimental:
  metrics_endpoint_routing:
    metric_allowlist:
      https://b.example.com: [tenant.orders.total, orders.total]
    metric_prefix_allowlist:
      https://c.example.com: [tenant.catalog., catalog.]
multi_region_failover:
  enabled: true
  api_key: mrf-key
  dd_url: https://mrf.example.com
  failover_metrics: true
  metric_allowlist: [tenant.orders.total, otel.requests]
autoscaling:
  failover:
    enabled: true
    metrics: [tenant.requests.count, otel.requests]
cluster_agent:
  enabled: true
  url: https://cluster-agent.example.com:5005
  auth_token: cluster-agent-token
dogstatsd_mapper_profiles:
  - name: checkout
    prefix: raw.
    mappings:
      - match: raw.requests.*
        name: mapped.requests
        tags:
          route: $1
statsd_metric_namespace: tenant
statsd_metric_namespace_blacklist: [internal.]
metric_filterlist: [tenant.raw.blocked, tenant.request.duration.max]
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

const EVERY_BRANCH_NO_AGGREGATION_PIPELINE_OFF: &str = r#"
dogstatsd_no_aggregation_pipeline: false
"#;

/// Returns the layers of a matrix entry's configuration, over the base configuration.
fn layers(name: &str, yaml: &'static str) -> Vec<&'static str> {
    let mut layers = vec![FLUSH_PASSTHROUGH_ON_STOP];
    if name == "every branch without the no-aggregation pipeline" {
        layers.push(EVERY_BRANCH);
    }
    layers.push(yaml);
    layers
}

fn context(name: &'static str, tags: &[&'static str]) -> Context {
    Context::from_static_parts(name, tags)
}

/// The DogStatsD-like batches: untimestamped metrics of every kind, which aggregation aggregates, timestamped ones,
/// which the no-aggregation pipeline sends past it, names the mapper maps, the filterlists block, the namespace prefix
/// excludes, and the routing groups and failover gateways select, and tags the tag filterlist and the value allowlist
/// rewrite. Gauge contexts repeat only across batches.
fn dogstatsd_batches() -> Vec<Vec<Metric>> {
    vec![
        vec![
            Metric::counter(context("raw.requests.checkout", &["keep:a"]), 1.0),
            Metric::counter(context("orders.total", &["env:prod"]), 2.0),
            Metric::gauge(context("checkout.cart", &["user_id:7", "env:dev"]), 5.0),
            Metric::set(context("visitors", &["env:prod"]), "u1"),
            Metric::distribution(
                context("checkout.latency", &["user_id:1", "env:dev", "keep:c"]),
                &[1.0, 2.0][..],
            ),
            Metric::histogram(context("request.duration", &[]), [3.0, 4.0]),
            Metric::counter(context("raw.blocked", &[]), 1.0),
            Metric::gauge(context("internal.health", &["keep:b"]), 3.0),
            Metric::counter(context("requests.count", &["service:web"]), 4.0),
            Metric::gauge(context("catalog.items", &[]), 7.0),
        ],
        vec![
            Metric::counter(context("orders.total", &["env:prod"]), 3.0),
            Metric::gauge(context("checkout.cart", &["user_id:7", "env:dev"]), 6.0),
            Metric::set(context("visitors", &["env:prod"]), "u2"),
            Metric::distribution(context("checkout.latency", &["user_id:1", "env:dev", "keep:c"]), 3.0),
            Metric::histogram(context("request.duration", &[]), 5.0),
            Metric::counter(context("requests.count", &["service:web"]), 1.0),
            Metric::counter(
                context("checkout.latency", &["user_id:2", "env:qa"]),
                (1_700_000_000, 5.0),
            ),
            Metric::counter(context("raw.requests.ts", &[]), (1_700_000_010, 2.0)),
            Metric::gauge(context("raw.blocked", &[]), (1_700_000_020, 1.0)),
            Metric::gauge(context("catalog.ts", &["env:prod"]), (1_700_000_030, 4.0)),
            Metric::counter(context("orders.total", &["env:prod"]), (1_700_000_040, 1.0)),
        ],
        vec![
            Metric::gauge(context("checkout.cart", &["user_id:7", "env:dev"]), 2.0),
            Metric::counter(context("raw.requests.cart", &["keep:a"]), 2.0),
            Metric::gauge(context("internal.health", &["keep:b"]), (1_700_000_050, 1.0)),
            Metric::distribution(context("request.size", &["env:staging"]), (1_700_000_060, 8.0)),
        ],
    ]
}

/// The OTLP-like batches, which enter the metrics path after aggregation: timestamped, some with a host of their own.
fn otlp_batches() -> Vec<Vec<Metric>> {
    vec![
        vec![
            Metric::gauge(
                context("otel.cpu", &["service:api"]).with_host(MetaString::from_static("otel-host")),
                (1_700_000_100, 0.5),
            ),
            Metric::counter(context("otel.requests", &["service:api"]), (1_700_000_100, 10.0)),
            Metric::distribution(context("otel.latency", &["service:api"]), (1_700_000_100, 2.0)),
        ],
        vec![
            Metric::gauge(context("catalog.otel", &[]), (1_700_000_110, 1.0)),
            Metric::counter(context("orders.total", &["env:prod"]), (1_700_000_110, 4.0)),
            Metric::gauge(context("otel.memory", &[]), 64.0),
        ],
    ]
}

/// The batches of every other source of metrics: the checks source and the trace sampler.
fn other_batches(feed: &str) -> Vec<Vec<Metric>> {
    match feed {
        "checks_ipc_in.metrics" => vec![vec![
            Metric::gauge(context("check.uptime", &["check:disk"]), 1.0),
            Metric::counter(context("orders.total", &["env:check"]), (1_700_000_200, 1.0)),
        ]],
        "trace_sampler.metrics" => vec![vec![Metric::gauge(
            context("datadog.trace_agent.sampler.size", &["sampler:priority"]),
            0.0,
        )]],
        _ => Vec::new(),
    }
}

/// Returns the differential input of a source: what the source `feed` sends, in batches.
fn differential_batches(feed: &str) -> Vec<Vec<Metric>> {
    match feed {
        "dsd_in.metrics" => dogstatsd_batches(),
        "otlp_in.metrics" => otlp_batches(),
        other => other_batches(other),
    }
}

/// Runs one path of a configuration's primary topology over the differential input, returning what the outputs
/// received and which outputs the topology has.
async fn run(name: &str, yaml: &'static str, path: MetricsPath) -> (Vec<Delivery>, BTreeSet<String>) {
    run_layers(&layers(name, yaml), path, differential_batches).await
}

/// Runs one path of the primary topology of the configuration `layers`, feeding each source what `batches` returns
/// for it, and returns what the outputs received and which outputs the topology has.
async fn run_layers(
    layers: &[&str], path: MetricsPath, batches: fn(&str) -> Vec<Vec<Metric>>,
) -> (Vec<Delivery>, BTreeSet<String>) {
    run_layers_on(layers, path, batches, 1).await
}

/// Runs one path of the primary topology of the configuration `layers` as [`run_layers`] does, with the DogStatsD
/// source's metrics on `producers` producer lanes: each batch is split over the lanes' feeds by context, each context's
/// points on the lane [`producer_lane_of`] gives it, in order, as a stream connection's are.
async fn run_layers_on(
    layers: &[&str], path: MetricsPath, batches: fn(&str) -> Vec<Vec<Metric>>, producers: usize,
) -> (Vec<Delivery>, BTreeSet<String>) {
    let system = layered_configuration_system(layers).await;
    let topology = TestTopology::start(&system, Options::new(path, Aggregation::Clocked), |_| {}).await;
    let outputs = topology.outputs();
    for feed in topology.feeds() {
        // The producer lanes' own feeds are fed below, with the source's batches.
        if feed.starts_with("dsd_in.metrics_") {
            continue;
        }
        for batch in batches(&feed) {
            if feed != "dsd_in.metrics" || producers == 1 {
                topology.feed(&feed, batch);
                continue;
            }
            let mut shares = vec![Vec::new(); producers];
            for metric in batch {
                shares[producer_lane_of(&metric, producers)].push(metric);
            }
            for (lane, share) in shares.into_iter().enumerate().filter(|(_, share)| !share.is_empty()) {
                topology.feed(&format!("dsd_in.metrics_{}", lane), share);
            }
        }
    }
    (topology.stop().await, outputs)
}

/// Returns the producer lane, of `lanes`, that the points of a metric's name go to: one lane per name, so that every
/// context of a name, which a tag rewrite may merge, keeps its points in order on one lane.
fn producer_lane_of(metric: &Metric, lanes: usize) -> usize {
    metric.context().name().bytes().map(usize::from).sum::<usize>() % lanes
}

// The matrices on lanes: the lowering matrix's DogStatsD rows and the gateways, on the lane counts every part runs
// today, with one state lane, and on state lanes, which the plan crate's state lanes and keyed exits bring.

/// Three producer lanes, one lane of each other kind.
const THREE_ONE_ONE: &str =
    "dogstatsd_workers_count: 3\nexperimental: {programmable_metrics: {lanes: {mode: auto, state: 1, post: 1}}}";

/// Three producer lanes, one state lane, and two post-aggregation lanes.
const THREE_ONE_TWO: &str =
    "dogstatsd_workers_count: 3\nexperimental: {programmable_metrics: {lanes: {mode: auto, state: 1, post: 2}}}";

/// One producer lane, one state lane, and three post-aggregation lanes.
const ONE_ONE_THREE: &str =
    "dogstatsd_workers_count: 1\nexperimental: {programmable_metrics: {lanes: {mode: auto, state: 1, post: 3}}}";

/// Returns the configuration of four producer lanes, `state` state lanes, and `post` post-aggregation lanes.
fn four_producer_lanes(state: u16, post: u16) -> String {
    format!(
        "dogstatsd_workers_count: 4\nexperimental: {{programmable_metrics: {{lanes: {{mode: auto, state: {}, post: \
         {}}}}}}}",
        state, post
    )
}

/// Runs every configuration of [`DIFFERENTIAL_MATRIX`] through the legacy topology and through the plan-driven one on
/// the lanes `lanes` asks for, with `producers` producer lanes, and checks that every output received the same points.
async fn assert_matrix_on_lanes(lanes: &str, producers: usize) {
    for (name, yaml) in DIFFERENTIAL_MATRIX {
        let (legacy, legacy_outputs) = run(name, yaml, MetricsPath::Legacy).await;
        let mut plan_layers = layers(name, yaml);
        plan_layers.push(lanes);
        let (plan, plan_outputs) =
            run_layers_on(&plan_layers, MetricsPath::Plan, differential_batches, producers).await;
        assert_eq!(plan_outputs, legacy_outputs, "{} on `{}`: the outputs", name, lanes);
        if let Err(mismatch) = compare(&output_points(&legacy), &output_points(&plan)) {
            panic!(
                "{} on `{}`: the plan-driven topology on lanes differs from the legacy one:\n{}",
                name, lanes, mismatch
            );
        }
        let names = names_by_output(&plan);
        for output in &plan_outputs {
            assert!(
                names.contains_key(output),
                "{} on `{}`: `{}` received nothing",
                name,
                lanes,
                output
            );
        }
    }
}

/// Runs every configuration of [`GATEWAY_MATRIX`] through the legacy topology and through the plan-driven one on the
/// lanes `lanes` asks for, with `producers` producer lanes, and checks that the gateways route the same points.
async fn assert_gateways_on_lanes(lanes: &str, producers: usize) {
    for (name, gateway_layers, silent) in GATEWAY_MATRIX {
        let (legacy, legacy_outputs) = run_gateways(gateway_layers, MetricsPath::Legacy).await;
        let plan_layers = [&[FLUSH_PASSTHROUGH_ON_STOP, GATEWAY_BASE][..], gateway_layers, &[lanes]].concat();
        let (plan, plan_outputs) = run_layers_on(&plan_layers, MetricsPath::Plan, gateway_batches, producers).await;
        assert_eq!(plan_outputs, legacy_outputs, "{} on `{}`: the outputs", name, lanes);
        if let Err(mismatch) = compare(&output_points(&legacy), &output_points(&plan)) {
            panic!(
                "{} on `{}`: the plan-driven gateways on lanes differ from the legacy ones:\n{}",
                name, lanes, mismatch
            );
        }
        let names = names_by_output(&plan);
        for output in &plan_outputs {
            assert_eq!(
                names.contains_key(output),
                !silent.contains(&output.as_str()),
                "{} on `{}`: what `{}` received",
                name,
                lanes,
                output
            );
        }
    }
}

#[tokio::test]
async fn on_three_producer_lanes_the_plan_driven_topology_delivers_what_the_legacy_one_does_to_every_output() {
    assert_matrix_on_lanes(THREE_ONE_ONE, 3).await;
}

#[tokio::test]
async fn on_three_producer_and_two_post_aggregation_lanes_the_plan_driven_topology_delivers_what_legacy_does() {
    assert_matrix_on_lanes(THREE_ONE_TWO, 3).await;
}

#[tokio::test]
async fn on_three_post_aggregation_lanes_the_plan_driven_topology_delivers_what_the_legacy_one_does_to_every_output() {
    assert_matrix_on_lanes(ONE_ONE_THREE, 1).await;
}

#[tokio::test]
async fn on_three_producer_lanes_the_gateways_route_what_the_legacy_gateways_route() {
    assert_gateways_on_lanes(THREE_ONE_ONE, 3).await;
}

#[tokio::test]
async fn on_three_producer_and_two_post_aggregation_lanes_the_gateways_route_what_the_legacy_gateways_route() {
    assert_gateways_on_lanes(THREE_ONE_TWO, 3).await;
}

#[tokio::test]
async fn on_three_post_aggregation_lanes_the_gateways_route_what_the_legacy_gateways_route() {
    assert_gateways_on_lanes(ONE_ONE_THREE, 1).await;
}

#[tokio::test]
async fn on_state_lanes_the_plan_driven_topology_delivers_what_the_legacy_one_does_to_every_output() {
    // Four producer lanes, P in {2, 4} and M in {1, 4}, as the stage 9 design's differential asks.
    for state in [2, 4] {
        for post in [1, 4] {
            assert_matrix_on_lanes(&four_producer_lanes(state, post), 4).await;
        }
    }
}

#[tokio::test]
async fn on_state_lanes_the_gateways_route_what_the_legacy_gateways_route() {
    for state in [2, 4] {
        for post in [1, 4] {
            assert_gateways_on_lanes(&four_producer_lanes(state, post), 4).await;
        }
    }
}

/// Returns, for every output, how many points of each name it received.
fn names_by_output(deliveries: &[Delivery]) -> BTreeMap<String, BTreeMap<String, usize>> {
    let mut names = BTreeMap::<String, BTreeMap<String, usize>>::new();
    for delivery in deliveries {
        for metric in &delivery.metrics {
            *names
                .entry(delivery.output.clone())
                .or_default()
                .entry(metric.context().name().to_string())
                .or_default() += metric.values().len();
        }
    }
    names
}

#[tokio::test]
async fn the_plan_driven_topology_delivers_what_the_legacy_one_does_to_every_output() {
    for (name, yaml) in DIFFERENTIAL_MATRIX {
        let (legacy, legacy_outputs) = run(name, yaml, MetricsPath::Legacy).await;
        let (plan, plan_outputs) = run(name, yaml, MetricsPath::Plan).await;
        assert_eq!(plan_outputs, legacy_outputs, "{}: the outputs", name);
        if let Err(mismatch) = compare(&output_points(&legacy), &output_points(&plan)) {
            panic!(
                "{}: the plan-driven topology differs from the legacy one:\n{}",
                name, mismatch
            );
        }
        // Every output received something, so that the comparison compares something everywhere.
        let names = names_by_output(&plan);
        for output in &plan_outputs {
            assert!(names.contains_key(output), "{}: `{}` received nothing", name, output);
        }
    }
}

/// Runs the plan-driven path of a configuration's primary topology over the differential input, with aggregation as a
/// stateful kernel in its isolation slot, clocked as native aggregation is (see [`Aggregation::Clocked`]).
async fn run_isolated(name: &str, yaml: &'static str) -> (Vec<Delivery>, BTreeSet<String>) {
    let system = layered_configuration_system(&layers(name, yaml)).await;
    let options = Options::new(MetricsPath::Plan, Aggregation::Clocked);
    let topology = TestTopology::start(&system, options, |_| {}).await;
    let outputs = topology.outputs();
    for feed in topology.feeds() {
        for batch in differential_batches(&feed) {
            topology.feed(&feed, batch);
        }
    }
    (topology.stop().await, outputs)
}

#[tokio::test]
async fn aggregation_as_a_kernel_in_its_region_delivers_what_native_aggregation_does_to_every_output() {
    for (name, yaml) in DIFFERENTIAL_MATRIX {
        let (legacy, legacy_outputs) = run(name, yaml, MetricsPath::Legacy).await;
        let (plan, plan_outputs) = run_isolated(name, yaml).await;
        assert_eq!(plan_outputs, legacy_outputs, "{}: the outputs", name);
        if let Err(mismatch) = compare(&output_points(&legacy), &output_points(&plan)) {
            panic!(
                "{}: aggregation in its region differs from native aggregation:\n{}",
                name, mismatch
            );
        }
    }
}

/// Returns the expected points of each name per output, from a table of `(output, [(name, points)])`.
fn expected(table: &[(&str, &[(&str, usize)])]) -> BTreeMap<String, BTreeMap<String, usize>> {
    table
        .iter()
        .map(|(output, names)| {
            let names = names.iter().map(|(name, points)| (name.to_string(), *points)).collect();
            (output.to_string(), names)
        })
        .collect()
}

/// What DogStatsD with defaults delivers, written by hand from the input: every metric, aggregated into one bucket at
/// the start of the window holding [`ADMITTED_AT`](crate::programmable_metrics::test_topology::ADMITTED_AT) unless
/// timestamped, histograms expanded into the default statistics, and nothing blocked. A name has two points when an
/// aggregated and a timestamped metric share it.
const DOGSTATSD_DEFAULTS: &[(&str, usize)] = &[
    ("catalog.items", 1),
    ("catalog.ts", 1),
    ("checkout.cart", 1),
    ("checkout.latency", 2),
    ("internal.health", 2),
    ("orders.total", 2),
    ("raw.blocked", 2),
    ("raw.requests.cart", 1),
    ("raw.requests.checkout", 1),
    ("raw.requests.ts", 1),
    ("request.duration.95percentile", 1),
    ("request.duration.avg", 1),
    ("request.duration.count", 1),
    ("request.duration.max", 1),
    ("request.duration.median", 1),
    ("request.size", 1),
    ("requests.count", 1),
    ("visitors", 1),
];

/// What the DogStatsD pipeline delivers with every branch's configuration, written by hand: the mapper maps the
/// `raw.requests.*` names, the namespace prefix prefixes everything but `internal.*`, the filterlists block
/// `tenant.raw.blocked` (both of its metrics) and the `max` statistic, and nothing else is dropped.
const EVERY_BRANCH_DOGSTATSD: &[(&str, usize)] = &[
    ("internal.health", 2),
    ("tenant.catalog.items", 1),
    ("tenant.catalog.ts", 1),
    ("tenant.checkout.cart", 1),
    ("tenant.checkout.latency", 2),
    ("tenant.mapped.requests", 3),
    ("tenant.orders.total", 2),
    ("tenant.request.duration.95percentile", 1),
    ("tenant.request.duration.avg", 1),
    ("tenant.request.duration.count", 1),
    ("tenant.request.duration.median", 1),
    ("tenant.request.size", 1),
    ("tenant.requests.count", 1),
    ("tenant.visitors", 1),
];

/// Asserts that `output` received a point whose display starts with `point`: a whole point, or a sketch up to its bins.
#[track_caller]
fn assert_point(deliveries: &[Delivery], output: &str, point: &str) {
    let points = output_points(deliveries);
    let output_id = saluki_metric_plan::ir::OutputId::new(output).unwrap();
    let found = points
        .points(&output_id)
        .map(|(received, _)| received.to_string())
        .collect::<Vec<_>>();
    assert!(
        found.iter().any(|received| received.starts_with(point)),
        "`{}` did not receive `{}`; it received:\n{}",
        output,
        point,
        found.join("\n")
    );
}

#[tokio::test]
async fn every_output_receives_what_the_configuration_says_it_does() {
    for path in [MetricsPath::Legacy, MetricsPath::Plan] {
        // DogStatsD with defaults: the main output and the client telemetry tap receive the same, and the values are
        // aggregated as natively: counters become rates over the 10 s window (carrying their sum), the last gauge wins,
        // histograms expand. Host enrichment leaves metrics without a host as they are.
        let (deliveries, _) = run("dogstatsd with defaults", "", path).await;
        assert_eq!(
            names_by_output(&deliveries),
            expected(&[
                ("dd_metrics_encode", DOGSTATSD_DEFAULTS),
                ("dsd_client_telemetry_out", DOGSTATSD_DEFAULTS),
            ]),
            "{:?}",
            path
        );
        for point in [
            r#"rate(10s) "orders.total" host=<unset> tags=[env:prod] ts=1800000000 value=5.0"#,
            r#"rate(10s) "orders.total" host=<unset> tags=[env:prod] ts=1700000040 value=1.0"#,
            r#"gauge "checkout.cart" host=<unset> tags=[env:dev, user_id:7] ts=1800000000 value=2.0"#,
            r#"set "visitors" host=<unset> tags=[env:prod] ts=1800000000 value={"u1", "u2"}"#,
            r#"gauge "request.duration.avg" host=<unset> tags=[] ts=1800000000 value=4.0"#,
            r#"gauge "request.duration.max" host=<unset> tags=[] ts=1800000000 value=5.0"#,
            r#"rate(10s) "request.duration.count" host=<unset> tags=[] ts=1800000000 value=3.0"#,
            r#"rate(10s) "checkout.latency" host=<unset> tags=[env:qa, user_id:2] ts=1700000000 value=5.0"#,
        ] {
            assert_point(&deliveries, "dd_metrics_encode", point);
        }

        // Every branch: the post-enrichment outputs also receive the other sources' metrics, the gateways select
        // theirs, and the tag filterlist and value allowlist rewrite what is aggregated, not what is timestamped.
        let (deliveries, _) = run("every branch", EVERY_BRANCH, path).await;
        let mut main = EVERY_BRANCH_DOGSTATSD.to_vec();
        main.extend([
            ("catalog.otel", 1),
            ("check.uptime", 1),
            ("datadog.trace_agent.sampler.size", 1),
            ("orders.total", 2),
            ("otel.cpu", 1),
            ("otel.latency", 1),
            ("otel.memory", 1),
            ("otel.requests", 1),
        ]);
        main.sort();
        assert_eq!(
            names_by_output(&deliveries),
            expected(&[
                (
                    "af_metrics_encode",
                    &[("otel.requests", 1), ("tenant.requests.count", 1)]
                ),
                ("dd_metrics_encode", &main),
                ("dsd_client_telemetry_out", EVERY_BRANCH_DOGSTATSD),
                (
                    "metrics_routing_encode_0",
                    &[
                        ("catalog.otel", 1),
                        ("tenant.catalog.items", 1),
                        ("tenant.catalog.ts", 1)
                    ]
                ),
                (
                    "metrics_routing_encode_1",
                    &[("orders.total", 2), ("tenant.orders.total", 2)]
                ),
                (
                    "mrf_metrics_encode",
                    &[("otel.requests", 1), ("tenant.orders.total", 2)]
                ),
            ]),
            "{:?}",
            path
        );
        for point in [
            // Tag projection rewrites counters and distributions only: the gauge keeps its tags.
            r#"gauge "tenant.checkout.cart" host=<unset> tags=[env:dev, user_id:7] ts=1800000000 value=2.0"#,
            concat!(
                r#"distribution "tenant.checkout.latency" host=<unset> tags=[env:other, keep:c] ts=1800000000 "#,
                "value=sketch(count=3 min=1.0 max=3.0 sum=6.0 avg=2.0 "
            ),
            r#"rate(10s) "tenant.checkout.latency" host=<unset> tags=[env:qa, user_id:2] ts=1700000000 value=5.0"#,
            r#"rate(10s) "tenant.mapped.requests" host=<unset> tags=[keep:a, route:checkout] ts=1800000000 value=1.0"#,
            r#"gauge "otel.cpu" host="otel-host" tags=[service:api] ts=1700000100 value=0.5"#,
            r#"gauge "otel.memory" host=<unset> tags=[] ts=none value=64.0"#,
        ] {
            assert_point(&deliveries, "dd_metrics_encode", point);
        }
        assert_point(
            &deliveries,
            "af_metrics_encode",
            r#"counter "otel.requests" host=<unset> tags=[service:api] ts=1700000100 value=10.0"#,
        );
    }
}

/// What every gateway configuration adds: the OTLP source beside DogStatsD, and four additional endpoints for routing
/// policies to select.
const GATEWAY_BASE: &str = r#"
data_plane: {otlp: {enabled: true}}
dd_url: https://primary.example.com
additional_endpoints:
  https://b.example.com: [key-b]
  https://c.example.com: [key-c]
  https://d.example.com: [key-d]
  https://e.example.com: [key-e]
"#;

/// Endpoint routing with two groups: `b` and `c` have equal lists, written differently, and share one; `d` has empty
/// lists, so it is excluded from the main output and has no group; `e` has prefixes (including one, `9`, that only a
/// name the intake rejects would start with) and names, one of them covered by a prefix.
const ROUTING_GROUPS: &str = r#"
experimental:
  metrics_endpoint_routing:
    metric_allowlist:
      https://b.example.com: [orders.total, my_service.requests]
      https://c.example.com: [my_service.requests, orders.total, orders.total]
      https://d.example.com: []
      https://e.example.com: [visitors, billing.total]
    metric_prefix_allowlist:
      https://e.example.com: [billing., '9']
"#;

const AUTOSCALING_FAILOVER: &str = r#"
autoscaling:
  failover:
    enabled: true
    metrics: [orders.total, request.duration.count, visitors, checkout.latency, billing.otel]
cluster_agent:
  enabled: true
  url: https://cluster-agent.example.com:5005
  auth_token: cluster-agent-token
"#;

/// The configurations the gateways are compared over: a name, the configuration over [`GATEWAY_BASE`], and the
/// outputs that receive nothing.
const GATEWAY_MATRIX: &[(&str, &[&str], &[&str])] = &[
    ("no routing group", &[], &[]),
    (
        "one routing group",
        &["experimental: {metrics_endpoint_routing: {metric_allowlist: {https://b.example.com: [orders.total]}}}"],
        &[],
    ),
    (
        "two routing groups, a shared group, and an endpoint with empty lists",
        &[ROUTING_GROUPS],
        &[],
    ),
    (
        "multi-region failover mirroring everything",
        &[
            "multi_region_failover: {enabled: true, api_key: mrf-key, dd_url: https://mrf.example.com, \
           failover_metrics: true}",
        ],
        &[],
    ),
    (
        "multi-region failover mirroring its allowlist",
        &[
            "multi_region_failover: {enabled: true, api_key: mrf-key, dd_url: https://mrf.example.com, \
           failover_metrics: true, metric_allowlist: [orders.total, my-service.requests]}",
        ],
        &[],
    ),
    (
        "multi-region failover with mirroring off",
        &[
            "multi_region_failover: {enabled: true, api_key: mrf-key, dd_url: https://mrf.example.com, \
           failover_metrics: false, metric_allowlist: [orders.total]}",
        ],
        &["mrf_metrics_encode"],
    ),
    ("autoscaling failover", &[AUTOSCALING_FAILOVER], &[]),
    (
        "every gateway",
        &[
            ROUTING_GROUPS,
            AUTOSCALING_FAILOVER,
            "multi_region_failover: {enabled: true, api_key: mrf-key, dd_url: https://mrf.example.com, \
             failover_metrics: true, metric_allowlist: [orders.total, billing.ts]}",
        ],
        &[],
    ),
];

/// The input of the gateway configurations: metrics of every kind, aggregated and timestamped, from DogStatsD and
/// OTLP, with names that the intake normalizes (`my-service.requests`), rejects (`9lives.count`), or stores as they
/// are, so that the gateways' name rules make a difference. Gauge contexts repeat only across batches.
fn gateway_batches(feed: &str) -> Vec<Vec<Metric>> {
    match feed {
        "dsd_in.metrics" => vec![
            vec![
                Metric::counter(context("orders.total", &["env:prod"]), 1.0),
                Metric::counter(context("my-service.requests", &[]), 2.0),
                Metric::counter(context("my_service.requests", &[]), 3.0),
                Metric::gauge(context("billing.balance", &[]), 4.0),
                Metric::set(context("visitors", &[]), "u1"),
                Metric::distribution(context("checkout.latency", &[]), 5.0),
                Metric::histogram(context("request.duration", &[]), [1.0, 2.0]),
                Metric::counter(context("9lives.count", &[]), 1.0),
            ],
            vec![
                Metric::counter(context("orders.total", &["env:prod"]), (1_700_000_000, 2.0)),
                Metric::gauge(context("billing.ts", &[]), (1_700_000_010, 6.0)),
                Metric::gauge(context("billing.balance", &[]), 7.0),
            ],
        ],
        "otlp_in.metrics" => vec![vec![
            Metric::counter(context("orders.total", &["env:otel"]), (1_700_000_100, 3.0)),
            Metric::gauge(context("billing.otel", &[]), (1_700_000_100, 8.0)),
            Metric::distribution(context("otel.latency", &[]), (1_700_000_100, 9.0)),
        ]],
        _ => Vec::new(),
    }
}

/// Runs one path of a gateway configuration over the gateway input.
async fn run_gateways(layers: &[&str], path: MetricsPath) -> (Vec<Delivery>, BTreeSet<String>) {
    let layers = [&[FLUSH_PASSTHROUGH_ON_STOP, GATEWAY_BASE][..], layers].concat();
    run_layers(&layers, path, gateway_batches).await
}

#[tokio::test]
async fn the_gateways_route_what_the_legacy_gateways_route() {
    for (name, layers, silent) in GATEWAY_MATRIX {
        let (legacy, legacy_outputs) = run_gateways(layers, MetricsPath::Legacy).await;
        let (plan, plan_outputs) = run_gateways(layers, MetricsPath::Plan).await;
        assert_eq!(plan_outputs, legacy_outputs, "{}: the outputs", name);
        if let Err(mismatch) = compare(&output_points(&legacy), &output_points(&plan)) {
            panic!(
                "{}: the plan-driven topology differs from the legacy one:\n{}",
                name, mismatch
            );
        }
        let names = names_by_output(&plan);
        for output in &plan_outputs {
            assert_eq!(
                names.contains_key(output),
                !silent.contains(&output.as_str()),
                "{}: what `{}` received",
                name,
                output
            );
        }
    }
}

#[tokio::test]
async fn each_gateway_selects_what_its_native_rules_select() {
    // Everything the policy region receives, by name and points: aggregation's flush (counters and the set aggregate
    // to one point, the gauge's last value wins, the histogram expands into the default statistics), the timestamped
    // DogStatsD points, and the OTLP metrics.
    const EVERYTHING: &[(&str, usize)] = &[
        ("9lives.count", 1),
        ("billing.balance", 1),
        ("billing.otel", 1),
        ("billing.ts", 1),
        ("checkout.latency", 1),
        ("my-service.requests", 1),
        ("my_service.requests", 1),
        ("orders.total", 3),
        ("otel.latency", 1),
        ("request.duration.95percentile", 1),
        ("request.duration.avg", 1),
        ("request.duration.count", 1),
        ("request.duration.max", 1),
        ("request.duration.median", 1),
        ("visitors", 1),
    ];
    let (_, every_gateway, _) = GATEWAY_MATRIX
        .iter()
        .find(|(name, ..)| *name == "every gateway")
        .unwrap();
    for path in [MetricsPath::Legacy, MetricsPath::Plan] {
        let (deliveries, _) = run_gateways(every_gateway, path).await;
        let mut names = names_by_output(&deliveries);
        names.remove("dsd_client_telemetry_out");
        assert_eq!(
            names,
            expected(&[
                // The scalar series of the configured names: not the distribution, and not the histogram, whose
                // statistics have names of their own, one of which is configured.
                (
                    "af_metrics_encode",
                    &[
                        ("billing.otel", 1),
                        ("orders.total", 3),
                        ("request.duration.count", 1),
                        ("visitors", 1),
                    ],
                ),
                // Every endpoint but the routed ones, so everything.
                ("dd_metrics_encode", EVERYTHING),
                // The shared group of `b` and `c` compares intake names, so both spellings of the service match.
                (
                    "metrics_routing_encode_0",
                    &[
                        ("my-service.requests", 1),
                        ("my_service.requests", 1),
                        ("orders.total", 3)
                    ],
                ),
                // The group of `e`: `visitors` by name, the `billing.` names by prefix, and nothing by the `9` prefix,
                // since the intake has no name for `9lives.count`.
                (
                    "metrics_routing_encode_1",
                    &[
                        ("billing.balance", 1),
                        ("billing.otel", 1),
                        ("billing.ts", 1),
                        ("visitors", 1),
                    ],
                ),
                // Raw names, exactly.
                ("mrf_metrics_encode", &[("billing.ts", 1), ("orders.total", 3)]),
            ]),
            "{:?}",
            path
        );
    }
}

// Tick parity: native aggregation's flush ticks against the isolation slot's, on one wall clock.

/// The configurations the tick parity runs: the defaults, histograms with a distribution copy, and counters that never
/// expire with a context limit of two.
const TICK_PARITY: &[(&str, &str)] = &[
    ("defaults", ""),
    (
        "histograms with a distribution copy",
        "histogram_copy_to_distribution: true\nhistogram_copy_to_distribution_prefix: dist.",
    ),
    (
        "no counter expiry and two contexts",
        "dogstatsd_expiry_seconds: 0\naggregate_context_limit: 2",
    ),
];

/// The tick parity configurations on several state lanes: [`TICK_PARITY`]'s, without a context limit that binds.
///
/// Which contexts win at a limit is first come in stamp order on one state lane, and the race of the lanes'
/// admissions on several (capacity behavior, 6.1): the script's first batch spreads three contexts over the lanes, so
/// which two a limit of two admits depends on which lane admits first, and only equivalence below capacity is exact.
const TICK_PARITY_ON_STATE_LANES: &[(&str, &str)] = &[
    TICK_PARITY[0],
    TICK_PARITY[1],
    ("no counter expiry", "dogstatsd_expiry_seconds: 0"),
];

/// One step of the tick parity's script: the wall clock moves to `B + at` seconds, then the source sends `batch`, or
/// a flush interval of the paused clock passes, which runs one flush tick.
enum TickStep {
    Send(u64, Vec<Metric>),
    Tick(u64),
}

/// The base time of the tick parity, a multiple of the 10-second window.
const TICKS_AT: u64 = 1_800_000_000;

fn tick_script() -> Vec<TickStep> {
    let counter = |name: &'static str, value: f64| Metric::counter(Context::from_static_name(name), value);
    let gauge = |name: &'static str, value: f64| Metric::gauge(Context::from_static_name(name), value);
    let histogram = |name: &'static str| Metric::histogram(Context::from_static_name(name), [1.0, 2.0, 5.0]);
    vec![
        TickStep::Send(
            3,
            vec![counter("requests", 1.0), gauge("load", 0.5), histogram("latency")],
        ),
        TickStep::Send(8, vec![counter("requests", 2.0), counter("errors", 1.0)]),
        // Closes [0, 10); `requests` and `errors` stay as counters that may synthesize zeros.
        TickStep::Tick(15),
        TickStep::Send(22, vec![counter("requests", 4.0), gauge("load", 0.7)]),
        // Closes [10, 20) and [20, 30): `errors` gains zeros, `requests` one for [10, 20).
        TickStep::Tick(30),
        TickStep::Send(41, vec![histogram("latency"), counter("late", 1.0)]),
        TickStep::Tick(45),
        // Idle ticks: counters keep synthesizing zeros until they expire, unless expiry is off.
        TickStep::Tick(60),
        TickStep::Tick(75),
    ]
}

/// Runs the tick script through one path's topology of `yaml`, with the source's admission point, the isolation
/// slot, and native aggregation all on one manual wall clock, on paused Tokio time, and returns what every output
/// received.
async fn run_ticks(yaml: &str, path: MetricsPath) -> (Vec<Delivery>, BTreeSet<String>) {
    run_ticks_on(yaml, "", path, "dsd_in.metrics").await
}

/// Runs the tick script as [`run_ticks`] does, with the lanes the configuration `lanes` asks for, the source sending
/// on `feed`.
async fn run_ticks_on(yaml: &str, lanes: &str, path: MetricsPath, feed: &str) -> (Vec<Delivery>, BTreeSet<String>) {
    use std::{sync::Arc, time::Duration};

    use saluki_core::execution::{AdmissionTime, ManualWallClock};

    let clock = ManualWallClock::new(AdmissionTime::from_unix_secs(TICKS_AT));
    let system = layered_configuration_system(&[FLUSH_PASSTHROUGH_ON_STOP, yaml, lanes]).await;
    let mut options = Options::new(path, Aggregation::Native);
    options.aggregation_clock = Some(clock.clone());
    options.before_topology = Some(Box::new({
        let clock = clock.clone();
        move |boot| {
            boot.state_admission_clock = Some(clock.clone());
            boot.wall_clock = Arc::new(clock.clone());
        }
    }));
    let topology = TestTopology::start(&system, options, |_| {}).await;
    let outputs = topology.outputs();
    let mut sent = 0;
    for step in tick_script() {
        match step {
            TickStep::Send(at, batch) => {
                clock.set(AdmissionTime::from_unix_secs(TICKS_AT + at));
                topology.feed(feed, batch);
                sent += 1;
                while topology.sent(feed) < sent {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
            TickStep::Tick(at) => {
                // The wall clock reads the tick's time before the tick: native aggregation flushes at it, and the
                // source's heartbeat, every second, raises the slot's watermark to it before the tick, so the tick gate
                // fires at once, at the same second.
                clock.set(AdmissionTime::from_unix_secs(TICKS_AT + at));
                tokio::time::sleep(Duration::from_secs(15)).await;
            }
        }
    }
    (topology.stop().await, outputs)
}

#[tokio::test(start_paused = true)]
async fn the_isolation_slots_flush_ticks_deliver_what_native_aggregations_do_tick_by_tick() {
    assert_tick_parity(TICK_PARITY, "", "dsd_in.metrics").await;
}

#[tokio::test(start_paused = true)]
async fn on_producer_lanes_the_isolation_slots_ticks_deliver_what_native_aggregations_do_tick_by_tick() {
    // One scripted producer on lane 1 of three: the idle lanes' heartbeats carry their points to the tick times, so
    // the watermark over the three points reaches each tick's second at once.
    let lanes =
        "dogstatsd_workers_count: 3\nexperimental: {programmable_metrics: {lanes: {mode: auto, state: 1, post: 2}}}";
    assert_tick_parity(TICK_PARITY, lanes, "dsd_in.metrics_1").await;
}

#[tokio::test(start_paused = true)]
async fn on_four_state_lanes_the_isolation_slots_ticks_deliver_what_native_aggregations_do_tick_by_tick() {
    // One scripted producer, every context on one of four state lanes: each tick at the second the shared tick record
    // holds, idle zeros from the lineage's latest non-empty flush in any lane, and expiry, exactly as native.
    let lanes =
        "dogstatsd_workers_count: 1\nexperimental: {programmable_metrics: {lanes: {mode: auto, state: 4, post: 2}}}";
    assert_tick_parity(TICK_PARITY_ON_STATE_LANES, lanes, "dsd_in.metrics").await;
}

#[tokio::test(start_paused = true)]
async fn an_idle_producer_lane_holds_a_flush_tick_of_the_busy_ones_back_by_at_most_the_progress_period() {
    // Three producer lanes into one state lane, on a wall clock 50 ms ahead of Tokio's from the start: lane 1 sends a
    // counter every 50 ms, and lanes 0 and 2 send one each at 0.9 s on the wall clock and nothing after, so that each
    // heartbeats 0.9 s into every second from then on, one period after its latest stamp. The state lane's watermark is
    // the minimum over the three points, so the first flush tick, due 15 s after the start, at 15.05 s on the wall
    // clock, waits for the idle lanes to stamp in second 15: at their heartbeat 100 ms into it, the progress period,
    // 50 ms after the tick was due, rather than at their periodic one, 850 ms after it.
    use std::{sync::Arc, time::Duration};

    use saluki_core::execution::{AdmissionTime, ManualWallClock};
    use saluki_metric_plan::limits::values::PROGRESS_PERIOD;

    const AHEAD: Duration = Duration::from_millis(50);
    let lanes =
        "dogstatsd_workers_count: 3\nexperimental: {programmable_metrics: {lanes: {mode: auto, state: 1, post: 1}}}";
    let start = AdmissionTime::from_unix_secs(TICKS_AT).saturating_add(AHEAD);
    let clock = ManualWallClock::new(start);
    let system = layered_configuration_system(&[FLUSH_PASSTHROUGH_ON_STOP, lanes]).await;
    let mut options = Options::new(MetricsPath::Plan, Aggregation::Native);
    options.before_topology = Some(Box::new({
        let clock = clock.clone();
        move |boot| {
            boot.state_admission_clock = Some(clock.clone());
            boot.wall_clock = Arc::new(clock.clone());
        }
    }));
    // The wall clock follows Tokio's paused clock, a millisecond at a time.
    let started = tokio::time::Instant::now();
    let following = tokio::spawn({
        let clock = clock.clone();
        async move {
            loop {
                clock.set(start.saturating_add(started.elapsed()));
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    });
    let topology = TestTopology::start(&system, options, |_| {}).await;
    let counter = |name: &'static str| Metric::counter(Context::from_static_name(name), 1.0);
    let mut idle_sent = false;
    let mut at = Duration::from_millis(25);
    while at < Duration::from_millis(16_000) {
        tokio::time::sleep_until(started + at).await;
        topology.feed("dsd_in.metrics_1", vec![counter("busy")]);
        if !idle_sent && at + AHEAD >= Duration::from_millis(900) {
            topology.feed("dsd_in.metrics_0", vec![counter("idle.zero")]);
            topology.feed("dsd_in.metrics_2", vec![counter("idle.two")]);
            idle_sent = true;
        }
        at += Duration::from_millis(50);
    }
    let deliveries = topology.stop().await;
    following.abort();
    let due = started + Duration::from_secs(15);
    let flushed = deliveries
        .iter()
        .filter(|delivery| delivery.output == "dd_metrics_encode" && !delivery.metrics.is_empty())
        .map(|delivery| delivery.at)
        .min()
        .expect("the tick due at 15 s flushed");
    let delivered = |name: &str| {
        deliveries
            .iter()
            .filter(|delivery| delivery.output == "dd_metrics_encode" && delivery.at == flushed)
            .flat_map(|delivery| &delivery.metrics)
            .any(|metric| metric.context().name() == name)
    };
    assert!(
        ["busy", "idle.zero", "idle.two"].into_iter().all(delivered),
        "the tick flushed every lane's bucket"
    );
    assert!(flushed >= due, "the tick flushed {:?} before it was due", due - flushed);
    assert!(
        flushed - due <= PROGRESS_PERIOD,
        "the tick due at 15 s flushed {:?} late",
        flushed - due
    );
}

/// Runs the tick script through both paths of every tick parity configuration of `configurations`, the plan-driven
/// one on the lanes `lanes` asks for and fed on `feed`, and checks that the outputs are equal.
async fn assert_tick_parity(configurations: &[(&str, &str)], lanes: &str, feed: &str) {
    for (name, yaml) in configurations {
        let (legacy, legacy_outputs) = run_ticks(yaml, MetricsPath::Legacy).await;
        let (plan, plan_outputs) = run_ticks_on(yaml, lanes, MetricsPath::Plan, feed).await;
        assert_eq!(plan_outputs, legacy_outputs, "{}: the outputs", name);
        let expected = output_points(&legacy);
        assert!(
            expected.count(&"dd_metrics_encode".parse().unwrap()) > 5,
            "{}: the script delivers",
            name
        );
        if let Err(mismatch) = compare(&expected, &output_points(&plan)) {
            panic!(
                "{}: the isolation slot's ticks differ from native aggregation's:\n{}",
                name, mismatch
            );
        }
        // The ticks ran before the end of input: the main output received a flush at each of the first three ticks
        // (aggregated output only, as the script has no timestamped points), on both paths.
        let flushes = |deliveries: &[Delivery]| {
            deliveries
                .iter()
                .filter(|delivery| delivery.output == "dd_metrics_encode" && !delivery.metrics.is_empty())
                .map(|delivery| delivery.at)
                .collect::<BTreeSet<_>>()
                .len()
        };
        assert!(
            flushes(&legacy) >= 3,
            "{}: native aggregation flushed at its ticks",
            name
        );
        assert!(flushes(&plan) >= 3, "{}: the isolation slot flushed at its ticks", name);
    }
}
