//! The whole default program, with every native kernel, run over a small trace of DogStatsD-like and OTLP input,
//! against outputs derived by hand from the fixture's parameters: with aggregation in its host component, through the
//! default aggregation adapter, and as a stateful kernel behind an isolation fence, in the no-queueing mode, where
//! both give the same outputs.
//!
//! The fixture (`fixtures/default-program.json`) maps `raw.requests.*` to `mapped.requests` with a `route` tag,
//! prefixes names with `tenant.` except those starting with `internal.` or `system.`, drops `tenant.raw.blocked` at the
//! listener, splits timestamped points around aggregation, removes `user_id` from `tenant.checkout.latency` and
//! restricts its `env` to `prod` or `staging` (others become `other`), aggregates over 10-second windows, drops the
//! series `tenant.checkout.latency.max` after aggregation, taps the result for client telemetry, adds the host tags
//! `cloud_provider:example` and `zone:a` to batches admitted before `B + 300`, and fans out to the primary endpoint,
//! two routing groups (`checkout.requests` and `checkout.`; `catalog.`), multi-region failover (on, with an empty
//! allowlist, so everything), and autoscaling failover (`requests.count`, scalar series only).

use std::{collections::BTreeMap, num::NonZeroU64};

use saluki_core::{
    data_model::event::metric::{context::Context, Metric, MetricValues, ScalarPoints},
    execution::{AdmissionTime, LossClass, LossCount},
};

use crate::{
    interpret::{InstallOutcome, Interpreter, RunOutput, Trace},
    ir::{NodeId, Plan},
    operators::OperatorRegistry,
    testing::{default_program_catalog, DEFAULT_PROGRAM_JSON},
};

/// The base time, a multiple of the 10-second window.
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

/// Returns what each output received, as sorted descriptions: delivery order is not observable.
fn received(run: &RunOutput) -> BTreeMap<String, Vec<String>> {
    run.outputs()
        .map(|(output, records)| {
            let mut described = records
                .iter()
                .map(|record| describe(&record.metric))
                .collect::<Vec<_>>();
            described.sort();
            (output.to_string(), described)
        })
        .collect()
}

fn sorted(records: &[&str]) -> Vec<String> {
    let mut records = records.iter().map(|record| record.to_string()).collect::<Vec<_>>();
    records.sort();
    records
}

fn gauge_at(name: &'static str, relative: u64, value: f64) -> Metric {
    Metric::gauge(Context::from_static_name(name), (B + relative, value))
}

fn counter_at(name: &'static str, relative: u64, value: f64) -> Metric {
    Metric::counter(Context::from_static_name(name), (B + relative, value))
}

#[test]
fn the_default_program_delivers_what_native_processing_does() {
    let plan = Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap();
    let dogstatsd = vec![
        // Mapped to `mapped.requests` with `route:login`, then prefixed; aggregated.
        Metric::counter(Context::from_static_parts("raw.requests.login", &["env:prod"]), 1.0),
        // Prefixed to the listener filterlist's only entry, and dropped.
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
        .install(plan)
        .admit(id("dsd_in"), t(3), dogstatsd)
        .admit(id("otlp_in"), t(4), otlp)
        // Aggregation flushes the bucket starting at `B`, re-admitted at the tick.
        .advance(t(15))
        // Admitted after the host tags' deadline.
        .admit(id("otlp_in"), t(301), vec![gauge_at("checkout.cpu", 301, 0.5)])
        .admit(id("dsd_in"), t(302), vec![gauge_at("system.disk", 302, 9.0)]);

    let catalog = default_program_catalog();
    let native = OperatorRegistry::native();
    let isolated = OperatorRegistry::native();
    for (what, interpreter) in [
        ("aggregation in its host component", Interpreter::new(&native, &catalog)),
        (
            "aggregation as a kernel",
            Interpreter::new(&isolated, &catalog).with_heartbeats_at_ticks(),
        ),
    ] {
        let run = interpreter.run(&trace).unwrap();
        assert!(
            matches!(run.installs()[0].outcome, InstallOutcome::Active { .. }),
            "{}",
            what
        );
        check_default_program_outputs(what, &run);
    }
}

/// Checks what the default program delivers over the trace of
/// [`the_default_program_delivers_what_native_processing_does`].
fn check_default_program_outputs(what: &str, run: &RunOutput) {
    // What aggregation flushes at `B + 15`, after post-aggregation filtering, without host tags.
    let flushed = [
        "tenant.mapped.requests{env:prod,route:login} rate/10s [0:1]",
        "tenant.checkout.latency{env:other} distribution [0:n=1,sum=10]",
        "internal.queue gauge [0:7]",
        "tenant.checkout.latency.median{env:prod} gauge [0:3]",
        "tenant.checkout.latency.avg{env:prod} gauge [0:3]",
        "tenant.checkout.latency.count{env:prod} rate/10s [0:5]",
        "tenant.checkout.latency.95percentile{env:prod} gauge [0:5]",
        "system.load gauge [0:1]",
    ];
    // The same, with the host tags: the flush is re-admitted at `B + 15`, before the deadline.
    let flushed_with_host_tags = [
        "tenant.mapped.requests{cloud_provider:example,env:prod,route:login,zone:a} rate/10s [0:1]",
        "tenant.checkout.latency{cloud_provider:example,env:other,zone:a} distribution [0:n=1,sum=10]",
        "internal.queue{cloud_provider:example,zone:a} gauge [0:7]",
        "tenant.checkout.latency.median{cloud_provider:example,env:prod,zone:a} gauge [0:3]",
        "tenant.checkout.latency.avg{cloud_provider:example,env:prod,zone:a} gauge [0:3]",
        "tenant.checkout.latency.count{cloud_provider:example,env:prod,zone:a} rate/10s [0:5]",
        "tenant.checkout.latency.95percentile{cloud_provider:example,env:prod,zone:a} gauge [0:5]",
        "system.load{cloud_provider:example,zone:a} gauge [0:1]",
    ];
    // Everything the policy region receives: the timestamped DogStatsD points (admitted at `B + 3` and `B + 302`), the
    // OTLP metrics (at `B + 4` and `B + 301`), and the flush.
    let mut everything = vec![
        "tenant.checkout.requests{cloud_provider:example,zone:a} rate/10s [1:2]",
        "system.load{cloud_provider:example,zone:a} gauge [2:2]",
        "system.disk gauge [302:9]",
        "catalog.items{cloud_provider:example,zone:a} gauge [4:3]",
        "requests.count{cloud_provider:example,zone:a} counter [4:4]",
        "checkout.requests{cloud_provider:example,zone:a} counter [4:1]",
        "checkout.cpu gauge [301:0.5]",
    ];
    everything.extend(flushed_with_host_tags);
    let mut client_telemetry = vec![
        "tenant.checkout.requests rate/10s [1:2]",
        "system.load gauge [2:2]",
        "system.disk gauge [302:9]",
    ];
    client_telemetry.extend(flushed);

    let expected = BTreeMap::from([
        ("dsd_client_telemetry".to_string(), sorted(&client_telemetry)),
        ("dd_metrics_encode".to_string(), sorted(&everything)),
        // `checkout.requests` by name, `checkout.cpu` by prefix; DogStatsD names carry the namespace.
        (
            "metrics_routing_encode_0".to_string(),
            sorted(&[
                "checkout.requests{cloud_provider:example,zone:a} counter [4:1]",
                "checkout.cpu gauge [301:0.5]",
            ]),
        ),
        (
            "metrics_routing_encode_1".to_string(),
            sorted(&["catalog.items{cloud_provider:example,zone:a} gauge [4:3]"]),
        ),
        ("mrf_metrics_encode".to_string(), sorted(&everything)),
        (
            "af_metrics_encode".to_string(),
            sorted(&["requests.count{cloud_provider:example,zone:a} counter [4:4]"]),
        ),
    ]);
    assert_eq!(received(run), expected, "{}", what);

    // The policy loss: the listener filterlist's drop, the post-aggregation filter's, and what each gateway does not
    // forward. Every record dropped here has one point.
    for (node, events) in [
        ("dsd_prefix_filter_blocklist_true", 1),
        ("dsd_post_agg_filter_true", 1),
        ("metrics_routing_filter_0_false", 13),
        ("metrics_routing_filter_1_false", 14),
        ("mrf_metrics_gateway_false", 0),
        ("af_metrics_gateway_false", 14),
    ] {
        assert_eq!(
            run.loss(&id(node)).loss(LossClass::Policy),
            LossCount { events, points: events },
            "{}: {}",
            what,
            node
        );
    }
}
