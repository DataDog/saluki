//! Isolated state owners in the reference interpreter: aggregation as a stateful kernel behind an isolation fence,
//! with its watermark and flush ticks from the shared time rules.
//!
//! The plan is `source -> aggregate -> emit`. Expected outputs are written by hand from the time rules: a point's
//! bucket is the one containing its admission time, and a tick flushes at its whole second once the watermark has
//! reached it. Times are relative to `B`, aligned to every width used.

use std::collections::BTreeMap;

use saluki_core::{
    data_model::event::metric::{context::Context, Metric, MetricValues},
    execution::AdmissionTime,
};
use serde_json::json;

use crate::{
    canonical::PlanHash,
    interpret::{Interpreter, RunOutput, Trace},
    ir::{NodeId, OutputId, Plan, RecipientId},
    model::PortType,
    operators::OperatorRegistry,
    validate::{CatalogOutput, MemoryCatalog, Validator},
};

/// The base time: a multiple of every width used.
const B: u64 = 1_700_000_000;

const OUTPUT: &str = "aggregated";

fn t(relative_millis: u64) -> AdmissionTime {
    AdmissionTime::from_unix_nanos((B * 1_000 + relative_millis) * 1_000_000)
}

fn src() -> NodeId {
    "src".parse().unwrap()
}

fn catalog() -> MemoryCatalog {
    MemoryCatalog::new().with_output(
        OutputId::new(OUTPUT).unwrap(),
        CatalogOutput {
            accepts: PortType::ANY,
            fingerprint: PlanHash::of_bytes(OUTPUT.as_bytes()),
            recipients: vec![RecipientId::new(format!("out/{}", OUTPUT)).unwrap()],
        },
    )
}

/// The plan, with a 10-second window and native defaults otherwise.
fn plan() -> Plan {
    let plan = Plan::from_value(json!({
        "ir_version": 1,
        "nodes": [
            {"id": "src", "op": "source", "params": {"binding": "dogstatsd", "timestamps": "untimestamped"},
             "provenance": {"native": "dsd_in"}},
            {"id": "agg", "op": "aggregate", "provenance": {"native": "dsd_agg"}, "params": {
                "window": "10s", "flush_interval": "15s", "context_limit": 1_000_000, "counter_expiry": "5m",
                "flush_open_windows": false, "histogram_aggregates": ["max", "median", "avg", "count"],
                "histogram_percentiles": [95], "copy_to_distribution": false, "copy_to_distribution_prefix": ""}},
            {"id": "out", "op": "emit", "params": {"output": OUTPUT}, "provenance": {"native": "dd_metrics_encode"}},
        ],
        "edges": [{"from": "src.metrics", "to": "agg.in"}, {"from": "agg.out", "to": "out.in"}],
        "outputs": [{"id": OUTPUT, "accepts": {"kinds": "all", "timestamps": "mixed"},
                     "fingerprint": PlanHash::of_bytes(OUTPUT.as_bytes()).to_string(),
                     "recipients": [format!("out/{}", OUTPUT)]}],
    }))
    .unwrap();
    let report = Validator::new(&OperatorRegistry::native(), &catalog()).validate(&plan);
    assert!(report.is_valid(), "the fixture plan is invalid:\n{}", report);
    plan
}

fn run(trace: Trace, heartbeats_at_ticks: bool) -> RunOutput {
    let registry = OperatorRegistry::native();
    let catalog = catalog();
    let mut events = Trace::new().install(plan());
    events.extend(trace.events().iter().cloned());
    let interpreter = Interpreter::new(&registry, &catalog);
    let interpreter = if heartbeats_at_ticks {
        interpreter.with_heartbeats_at_ticks()
    } else {
        interpreter
    };
    interpreter.run(&events).unwrap()
}

fn context(name: &'static str) -> Context {
    Context::from_static_parts(name, &[])
}

/// Describes a record as `name kind [timestamp:value, ...]`, timestamps relative to `B`.
fn describe(metric: &Metric) -> String {
    let (kind, points) = match metric.values() {
        MetricValues::Rate(points, interval) => (format!("rate/{}s", interval.as_secs()), points),
        MetricValues::Gauge(points) => ("gauge".to_string(), points),
        other => panic!("unexpected values {:?}", other),
    };
    let mut points = points
        .into_iter()
        .map(|(ts, value)| format!("{}:{}", ts.map_or(0, |ts| ts.get() - B), value))
        .collect::<Vec<_>>();
    points.sort();
    format!("{} {} [{}]", metric.context().name(), kind, points.join(", "))
}

/// Returns the records the output received, grouped by their admission time in milliseconds relative to `B`: the
/// flush time of the tick that emitted them.
fn flushes(run: &RunOutput) -> Vec<(u64, Vec<String>)> {
    let mut by_flush = BTreeMap::<u64, Vec<String>>::new();
    for record in run.output(&OutputId::new(OUTPUT).unwrap()) {
        let at = record.admitted_at.expect("flushed records are stamped").as_unix_nanos() / 1_000_000 - B * 1_000;
        by_flush.entry(at).or_default().push(describe(&record.metric));
    }
    by_flush
        .into_iter()
        .map(|(at, mut records)| {
            records.sort();
            (at, records)
        })
        .collect()
}

fn expected(flushes: &[(u64, &[&str])]) -> Vec<(u64, Vec<String>)> {
    flushes
        .iter()
        .map(|(at, records)| {
            let mut records = records.iter().map(|record| record.to_string()).collect::<Vec<_>>();
            records.sort();
            (*at, records)
        })
        .collect()
}

#[test]
fn with_heartbeats_at_ticks_every_tick_flushes_at_its_second_what_was_admitted_before() {
    // Points are bucketed by admission time. Each tick's heartbeat raises the watermark to the tick's clock, so the
    // tick fires at once, at its whole second: 15 closes [0, 10); 30 closes [10, 20) and [20, 30), where the idle
    // counter gains zeros.
    let run = run(
        Trace::new()
            .admit(
                src(),
                t(3_000),
                vec![Metric::counter(context("c"), 1.0), Metric::gauge(context("g"), 5.0)],
            )
            .admit(src(), t(8_500), vec![Metric::counter(context("c"), 4.0)])
            .advance(t(15_200))
            .advance(t(30_700)),
        true,
    );
    assert_eq!(
        flushes(&run),
        expected(&[
            (15_000, &["c rate/10s [0:5]", "g gauge [0:5]"]),
            (30_000, &["c rate/10s [10:0, 20:0]"]),
        ])
    );
}

#[test]
fn a_tick_whose_watermark_lags_waits_at_the_gate_for_a_later_stamp_or_the_end_of_its_grace() {
    // Without heartbeats the watermark is the latest admission time. The tick at 15.2 finds it at 3, so it waits; the
    // batch at 16 raises it past 15, which fires the tick at 15 before the batch is admitted. The tick at 31 waits
    // again; the tick at 34 comes after its 2-second grace, so the waiting tick fires at the watermark's second, 16,
    // which closes nothing, and the tick at 34 itself waits, until the batch at 40 fires it at 34.
    let run = run(
        Trace::new()
            .admit(src(), t(3_000), vec![Metric::gauge(context("g"), 1.0)])
            .advance(t(15_200))
            .admit(src(), t(16_000), vec![Metric::gauge(context("g"), 2.0)])
            .advance(t(31_000))
            .advance(t(34_000))
            .admit(src(), t(40_000), vec![Metric::gauge(context("g"), 3.0)]),
        false,
    );
    assert_eq!(
        flushes(&run),
        expected(&[(15_000, &["g gauge [0:1]"]), (34_000, &["g gauge [10:2]"])])
    );
}
