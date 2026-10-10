//! The lowered every-branch program, run through the plan crate's reference interpreter.
//!
//! The other lowering tests pin the plan's structure and parameters; this one executes them, with every native kernel,
//! the default aggregation adapter, and the gateways as the `split`s they lower to. The trace is the one the plan
//! crate's `the_default_program_delivers_what_native_processing_does` runs over its own default-program fixture, whose
//! parameters equal the lowered ones except where the configuration differs (the listener filterlist also names
//! `tenant.checkout.latency.max`, which no metric of the trace has, and the gateways select by this configuration's
//! lists). The expected outputs were derived by hand from the configuration and the native rules: the plan crate's
//! derivation for everything up to the gateways, and then the native gateways' rules over the configuration's own
//! lists: the routing groups (`catalog.` for the group at index 0; `orders.total` and `checkout.` for the group at
//! index 1) compare intake names, multi-region failover mirrors everything with an empty allowlist, the raw names its
//! allowlist names otherwise, and nothing with mirroring off, and autoscaling failover forwards the scalar series
//! `requests.count`.

use std::{num::NonZeroU64, sync::Arc};

use agent_data_plane_config::SalukiConfiguration;
use saluki_core::{
    data_model::event::metric::{context::Context, Metric, MetricValues, ScalarPoints},
    execution::{AdmissionTime, LossClass, LossCount},
};
use saluki_metric_plan::{
    interpret::{InstallOutcome, Interpreter, RunOutput, Trace},
    ir::{NodeId, OutputId},
    operators::OperatorRegistry,
};

use super::{ids, matrix::EVERY_BRANCH, Lowering};
use crate::programmable_metrics::test_support::{facts, layered_configuration};

/// The base time: the test process start, a multiple of the 10-second window.
const B: u64 = 1_700_000_000;

fn t(relative: u64) -> AdmissionTime {
    AdmissionTime::from_unix_secs(B + relative)
}

fn id(value: &str) -> NodeId {
    value.parse().unwrap()
}

fn ts(relative: u64) -> Option<NonZeroU64> {
    NonZeroU64::new(B + relative)
}

fn gauge_at(name: &'static str, relative: u64, value: f64) -> Metric {
    Metric::gauge(Context::from_static_name(name), (B + relative, value))
}

fn counter_at(name: &'static str, relative: u64, value: f64) -> Metric {
    Metric::counter(Context::from_static_name(name), (B + relative, value))
}

/// Describes a record by its name, sorted tags, kind, and points, with timestamps relative to `B`.
fn describe(metric: &Metric) -> String {
    let context = metric.context();
    let mut tags = context
        .tags()
        .into_iter()
        .map(|tag| tag.as_str().to_string())
        .collect::<Vec<_>>();
    tags.sort();
    let name = if tags.is_empty() {
        context.name().to_string()
    } else {
        format!("{}{{{}}}", context.name(), tags.join(","))
    };
    let at = |ts: Option<NonZeroU64>| match ts {
        Some(ts) => (ts.get() - B).to_string(),
        None => "none".to_string(),
    };
    let scalars = |points: &ScalarPoints| {
        points
            .into_iter()
            .map(|(ts, value)| format!("{}:{}", at(ts), value))
            .collect::<Vec<_>>()
    };
    let (kind, points) = match metric.values() {
        MetricValues::Counter(points) => ("counter".to_string(), scalars(points)),
        MetricValues::Rate(points, interval) => (format!("rate/{}s", interval.as_secs()), scalars(points)),
        MetricValues::Gauge(points) => ("gauge".to_string(), scalars(points)),
        MetricValues::Distribution(points) => (
            "distribution".to_string(),
            points
                .into_iter()
                .map(|(ts, sketch)| format!("{}:n={},sum={}", at(ts), sketch.count(), sketch.sum().unwrap_or(0.0)))
                .collect(),
        ),
        other => panic!("no output of this trace is a {:?}", other),
    };
    format!("{} {} [{}]", name, kind, points.join(", "))
}

/// Returns what an output received, as sorted descriptions: delivery order is not observable.
fn received(run: &RunOutput, output: &str) -> Vec<String> {
    let output = output.parse::<OutputId>().unwrap();
    let mut described = run
        .output(&output)
        .iter()
        .map(|record| describe(&record.metric))
        .collect::<Vec<_>>();
    described.sort();
    described
}

fn sorted(records: &[&str]) -> Vec<String> {
    let mut records = records.iter().map(|record| record.to_string()).collect::<Vec<_>>();
    records.sort();
    records
}

/// Returns the every-branch configuration, with the multi-region failover settings `mirroring`.
fn configuration(mirroring: &str) -> SalukiConfiguration {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(layered_configuration(&[EVERY_BRANCH, mirroring]))
}

/// Lowers the configuration and runs its default program over the trace.
fn run(config: &SalukiConfiguration) -> RunOutput {
    let lowering = Lowering::new(Arc::new(config.clone()), facts(config)).unwrap().unwrap();
    let lowered = lowering.lower(config).unwrap();

    let registry = OperatorRegistry::native();
    let interpreter = Interpreter::new(&registry, &lowered.catalog);
    let dogstatsd = vec![
        // Mapped to `mapped.requests` with `route:login`, then prefixed; aggregated.
        Metric::counter(Context::from_static_parts("raw.requests.login", &["env:prod"]), 1.0),
        // Prefixed to the listener filterlist's first entry, and dropped.
        Metric::counter(Context::from_static_name("raw.blocked"), 1.0),
        // A distribution whose tags are projected: `user_id` removed, `env:dev` replaced by `env:other`.
        Metric::distribution(
            Context::from_static_parts("checkout.latency", &["user_id:42", "env:dev"]),
            10.0,
        ),
        // Excluded from prefixing; aggregated.
        Metric::gauge(Context::from_static_name("internal.queue"), 7.0),
        // A histogram, which tag projection leaves alone, expanded by aggregation; its `.max` series is dropped after
        // aggregation.
        Metric::histogram(
            Context::from_static_parts("checkout.latency", &["env:prod"]),
            [1.0, 2.0, 3.0, 4.0, 5.0],
        ),
        // Timestamped: bypasses aggregation, becoming a rate over the window.
        counter_at("checkout.requests", 1, 2.0),
        // Mixed: its timestamped point bypasses aggregation, its untimestamped one is aggregated.
        Metric::gauge(
            Context::from_static_name("system.load"),
            ScalarPoints::from_iter([(None, 1.0), (ts(2), 2.0)]),
        ),
    ];
    let otlp = vec![
        gauge_at("catalog.items", 4, 3.0),
        counter_at("requests.count", 4, 4.0),
        counter_at("checkout.requests", 4, 1.0),
    ];
    let trace = Trace::new()
        .install(lowered.plan)
        .admit(id(ids::DSD_IN), t(3), dogstatsd)
        .admit(id(ids::OTLP_IN), t(4), otlp)
        // Aggregation flushes the bucket starting at `B`, re-admitted at the tick.
        .advance(t(15))
        // Admitted after the host tags' deadline, five minutes after the process start.
        .admit(id(ids::OTLP_IN), t(301), vec![gauge_at("checkout.cpu", 301, 0.5)])
        .admit(id(ids::DSD_IN), t(302), vec![gauge_at("system.disk", 302, 9.0)]);
    let run = interpreter.run(&trace).unwrap();
    assert!(matches!(run.installs()[0].outcome, InstallOutcome::Active { .. }));
    run
}

/// What aggregation flushes at `B + 15`, after post-aggregation filtering, without host tags.
const FLUSHED: [&str; 8] = [
    "tenant.mapped.requests{env:prod,route:login} rate/10s [0:1]",
    "tenant.checkout.latency{env:other} distribution [0:n=1,sum=10]",
    "internal.queue gauge [0:7]",
    "tenant.checkout.latency.median{env:prod} gauge [0:3]",
    "tenant.checkout.latency.avg{env:prod} gauge [0:3]",
    "tenant.checkout.latency.count{env:prod} rate/10s [0:5]",
    "tenant.checkout.latency.95percentile{env:prod} gauge [0:5]",
    "system.load gauge [0:1]",
];

/// Everything the policy region receives: the timestamped DogStatsD points (admitted at `B + 3` and `B + 302`), the
/// OTLP metrics (at `B + 4` and `B + 301`), and the flush, with the host tags: the flush is re-admitted at `B + 15`,
/// before their deadline.
const EVERYTHING: [&str; 15] = [
    "tenant.checkout.requests{cloud_provider:example,zone:a} rate/10s [1:2]",
    "system.load{cloud_provider:example,zone:a} gauge [2:2]",
    "system.disk gauge [302:9]",
    "catalog.items{cloud_provider:example,zone:a} gauge [4:3]",
    "requests.count{cloud_provider:example,zone:a} counter [4:4]",
    "checkout.requests{cloud_provider:example,zone:a} counter [4:1]",
    "checkout.cpu gauge [301:0.5]",
    "tenant.mapped.requests{cloud_provider:example,env:prod,route:login,zone:a} rate/10s [0:1]",
    "tenant.checkout.latency{cloud_provider:example,env:other,zone:a} distribution [0:n=1,sum=10]",
    "internal.queue{cloud_provider:example,zone:a} gauge [0:7]",
    "tenant.checkout.latency.median{cloud_provider:example,env:prod,zone:a} gauge [0:3]",
    "tenant.checkout.latency.avg{cloud_provider:example,env:prod,zone:a} gauge [0:3]",
    "tenant.checkout.latency.count{cloud_provider:example,env:prod,zone:a} rate/10s [0:5]",
    "tenant.checkout.latency.95percentile{cloud_provider:example,env:prod,zone:a} gauge [0:5]",
    "system.load{cloud_provider:example,zone:a} gauge [0:1]",
];

#[test]
fn the_lowered_default_program_delivers_what_native_processing_does() {
    let run = run(&configuration("multi_region_failover: {failover_metrics: true}"));

    let mut client_telemetry = vec![
        "tenant.checkout.requests rate/10s [1:2]",
        "system.load gauge [2:2]",
        "system.disk gauge [302:9]",
    ];
    client_telemetry.extend(FLUSHED);
    let expected: [(&str, Vec<&str>); 6] = [
        ("dsd_client_telemetry", client_telemetry),
        ("dd_metrics_encode", EVERYTHING.to_vec()),
        // `catalog.` by prefix.
        (
            "metrics_routing_encode_0",
            vec!["catalog.items{cloud_provider:example,zone:a} gauge [4:3]"],
        ),
        // `checkout.` by prefix (no `orders.total`); DogStatsD names carry the namespace, so they do not match.
        (
            "metrics_routing_encode_1",
            vec![
                "checkout.requests{cloud_provider:example,zone:a} counter [4:1]",
                "checkout.cpu gauge [301:0.5]",
            ],
        ),
        // Mirroring with an empty allowlist: everything.
        ("mrf_metrics_encode", EVERYTHING.to_vec()),
        // The one scalar series named `requests.count`.
        (
            "af_metrics_encode",
            vec!["requests.count{cloud_provider:example,zone:a} counter [4:4]"],
        ),
    ];
    for (output, records) in expected {
        assert_eq!(received(&run, output), sorted(&records), "{}", output);
    }

    // The policy loss: the listener filterlist's drop, the post-aggregation filter's, and what each gateway does not
    // select, of the 15 records reaching the policy region. Every record dropped here has one point.
    for (node, events) in [
        ("dsd_prefix_filter_blocklist_true", 1),
        ("dsd_post_agg_filter_true", 1),
        ("metrics_routing_filter_0_false", 14),
        ("metrics_routing_filter_1_false", 13),
        ("mrf_metrics_gateway_false", 0),
        ("af_metrics_gateway_false", 14),
    ] {
        assert_eq!(
            run.loss(&id(node)).loss(LossClass::Policy),
            LossCount { events, points: events },
            "{}",
            node
        );
    }
}

#[test]
fn the_multi_region_failover_gateway_mirrors_what_its_allowlist_names_and_nothing_while_mirroring_is_off() {
    // (settings, what the output receives) for mirroring with an allowlist, which names raw names after the mapper
    // and the namespace (`tenant.checkout.requests`, not `checkout.requests` from DogStatsD), and for mirroring off,
    // whatever the allowlist.
    let allowlist = "multi_region_failover: {failover_metrics: true, metric_allowlist: [requests.count, checkout.cpu, \
                     tenant.checkout.requests, tenant.missing]}";
    let cases: [(&str, Vec<&str>); 3] = [
        (
            allowlist,
            vec![
                "requests.count{cloud_provider:example,zone:a} counter [4:4]",
                "checkout.cpu gauge [301:0.5]",
                "tenant.checkout.requests{cloud_provider:example,zone:a} rate/10s [1:2]",
            ],
        ),
        ("multi_region_failover: {failover_metrics: false}", vec![]),
        (
            "multi_region_failover: {failover_metrics: false, metric_allowlist: [requests.count]}",
            vec![],
        ),
    ];
    for (mirroring, expected) in cases {
        let run = run(&configuration(mirroring));
        assert_eq!(received(&run, "mrf_metrics_encode"), sorted(&expected), "{}", mirroring);
        let dropped = (EVERYTHING.len() - expected.len()) as u64;
        assert_eq!(
            run.loss(&id("mrf_metrics_gateway_false")).loss(LossClass::Policy),
            LossCount {
                events: dropped,
                points: dropped
            },
            "{}",
            mirroring
        );
        // The other outputs are unchanged.
        assert_eq!(
            received(&run, "dd_metrics_encode"),
            sorted(&EVERYTHING),
            "{}",
            mirroring
        );
    }
}
