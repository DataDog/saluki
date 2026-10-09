//! The lowered every-branch program, run through the plan crate's reference interpreter.
//!
//! The other lowering tests pin the plan's structure and parameters; this one executes them, with every native kernel,
//! the default aggregation adapter, and an adapter for each opaque gateway that applies the selection its native
//! component applies, through the `saluki_metric_kernels::selection` types that component calls, configured from
//! the same configuration. The trace is the one the plan crate's
//! `the_default_program_delivers_what_native_processing_does` runs over its own default-program fixture, whose
//! parameters equal the lowered ones except where the configuration differs (the listener filterlist also names
//! `tenant.checkout.latency.max`, which no metric of the trace has) and in the gateways (here opaque hosts, there
//! splits). The expected outputs were derived by hand from the configuration and the native rules: the plan crate's
//! derivation for everything up to the gateways, and the configuration's own routing groups (`catalog.` for the group
//! at index 0; `orders.total` and `checkout.` for the group at index 1), multi-region failover mirroring everything,
//! and autoscaling failover forwarding the scalar series `requests.count`.

use std::{collections::BTreeMap, num::NonZeroU64, sync::Arc};

use agent_data_plane_config::SalukiConfiguration;
use saluki_core::{
    data_model::event::metric::{context::Context, Metric, MetricValues, ScalarPoints},
    execution::{AdmissionTime, LossClass, LossCount, PortIndex},
};
use saluki_metric_kernels::selection::{is_scalar_series, IntakeNameAllowlist, RawNameSet};
use saluki_metric_plan::{
    interpret::{HostBatch, HostOutput, InstallOutcome, Interpreter, OpaqueHostAdapter, RunOutput, Trace},
    ir::NodeId,
    operators::OperatorRegistry,
};

use super::{ids, matrix::EVERY_BRANCH, Lowering};
use crate::programmable_metrics::{
    bindings::routing_groups,
    test_support::{facts, layered_configuration},
};

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

/// An opaque gateway: forwards what its native component's selection keeps on its only port, and records the rest as
/// policy loss, one event per metric with its points.
struct Gateway {
    forwards: Box<dyn Fn(&Metric) -> bool + Send>,
}

impl Gateway {
    fn boxed(forwards: impl Fn(&Metric) -> bool + Send + 'static) -> Box<dyn OpaqueHostAdapter> {
        Box::new(Self {
            forwards: Box::new(forwards),
        })
    }
}

impl OpaqueHostAdapter for Gateway {
    fn admit(&mut self, batch: HostBatch, output: &mut HostOutput) {
        for metric in batch.events {
            if (self.forwards)(&metric) {
                output.push(PortIndex::PRIMARY, metric);
            } else {
                output.record_loss(LossClass::Policy, 1, metric.values().len() as u64);
            }
        }
    }
}

/// Returns the configuration: every branch, with multi-region failover mirroring every metric.
fn configuration() -> SalukiConfiguration {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(layered_configuration(&[
        EVERY_BRANCH,
        "multi_region_failover: {failover_metrics: true}",
    ]))
}

#[test]
fn the_lowered_default_program_delivers_what_native_processing_does() {
    let config = configuration();
    let lowering = Lowering::new(Arc::new(config.clone()), facts(&config))
        .unwrap()
        .unwrap();
    let lowered = lowering.lower(&config).unwrap();

    // The gateways' selections, as their native components build them from this configuration.
    let groups = routing_groups(&config).unwrap();
    let group = |index: usize| {
        let group = &groups.policy_groups()[index];
        IntakeNameAllowlist::new(&group.metric_allowlist, &group.metric_prefix_allowlist)
    };
    let (group_0, group_1) = (group(0), group(1));
    let mrf = &config.domains.multi_region_failover;
    assert!(mrf.enabled && mrf.metric_mirroring.enabled && mrf.metric_mirroring.allowlist.is_empty());
    let autoscaling = RawNameSet::new(&config.shared.autoscaling_failover.metrics);

    let registry = OperatorRegistry::native();
    let interpreter = Interpreter::new(&registry, &lowered.catalog)
        .with_opaque_host(id(&ids::routing_filter(0)), move |_| {
            let group = group_0.clone();
            Ok(Gateway::boxed(move |metric| group.matches(metric.context().name())))
        })
        .with_opaque_host(id(&ids::routing_filter(1)), move |_| {
            let group = group_1.clone();
            Ok(Gateway::boxed(move |metric| group.matches(metric.context().name())))
        })
        // Enabled, mirroring, with an empty allowlist: everything.
        .with_opaque_host(id(ids::MRF_METRICS_GATEWAY), |_| Ok(Gateway::boxed(|_| true)))
        .with_opaque_host(id(ids::AF_METRICS_GATEWAY), move |_| {
            let allowlist = autoscaling.clone();
            Ok(Gateway::boxed(move |metric| {
                is_scalar_series(metric.values()) && allowlist.contains(metric.context().name())
            }))
        });

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
        // `catalog.` by prefix.
        (
            "metrics_routing_encode_0".to_string(),
            sorted(&["catalog.items{cloud_provider:example,zone:a} gauge [4:3]"]),
        ),
        // `checkout.` by prefix (no `orders.total`); DogStatsD names carry the namespace, so they do not match.
        (
            "metrics_routing_encode_1".to_string(),
            sorted(&[
                "checkout.requests{cloud_provider:example,zone:a} counter [4:1]",
                "checkout.cpu gauge [301:0.5]",
            ]),
        ),
        ("mrf_metrics_encode".to_string(), sorted(&everything)),
        (
            "af_metrics_encode".to_string(),
            sorted(&["requests.count{cloud_provider:example,zone:a} counter [4:4]"]),
        ),
    ]);
    assert_eq!(received(&run), expected);

    // The policy loss: the listener filterlist's drop, the post-aggregation filter's, and what each gateway does not
    // forward, of the 15 records reaching the policy region. Every record dropped here has one point.
    for (node, events) in [
        ("dsd_prefix_filter_blocklist_true", 1),
        ("dsd_post_agg_filter_true", 1),
        ("metrics_routing_filter_0", 14),
        ("metrics_routing_filter_1", 13),
        ("mrf_metrics_gateway", 0),
        ("af_metrics_gateway", 14),
    ] {
        assert_eq!(
            run.loss(&id(node)).loss(LossClass::Policy),
            LossCount { events, points: events },
            "{}",
            node
        );
    }
}
