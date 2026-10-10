//! Bucket assignment by admission time, against native aggregation by arrival time: the exit gate's property test.
//!
//! Native aggregation buckets a batch by its clock when the batch arrives, and flushes at each tick's clock. As a
//! kernel, aggregation buckets a batch by its admission time `t_a`, and a tick flushes at its wall-clock second once
//! the watermark has reached it (the tick gate), waiting at most a grace. The two differ only where the specification
//! says they may:
//!
//! - **(a)** a point whose `t_a` and arrival fall in different buckets lands in its `t_a` bucket instead of its arrival
//!   bucket, so both buckets of its context may differ;
//! - **(b)** a batch stamped before a tick's wall-clock second that arrives after the tick is admitted before the
//!   tick's flush instead of after it, which can change its context's zeros, expiry, and metadata (amendment 86);
//! - **(c)** a tick whose grace expires flushes at the watermark instead of the tick's second, which changes the zeros
//!   idle counters gain.
//!
//! Every other point is equal. The traces are random: batches of counters and gauges with non-decreasing admission
//! times, random queueing delays that keep each source's batches in order, ticks every 15 seconds, and heartbeats
//! that the source sends around each tick. A first counter, admitted before any tick, keeps aggregation's state
//! non-empty, so every tick is a flush in both, and the flushes of both start from the same previous one. A variant
//! whose delays stay within a batch's bucket and well below the grace, with a heartbeat soon after every tick, has no
//! difference at all: there, every point, zeros included, is equal.

use std::collections::BTreeSet;

use proptest::{collection::vec, prelude::*};
use saluki_core::{
    data_model::event::metric::{context::Context, Metric, MetricValues},
    execution::AdmissionTime,
};
use saluki_metric_kernels::aggregate::{AggregateCore, AggregateTelemetry};
use serde_json::json;

use crate::{
    canonical::PlanHash,
    interpret::{Interpreter, Trace},
    ir::{OutputId, Plan, RecipientId},
    model::PortType,
    operators::{aggregate_params, OperatorRegistry},
    validate::{CatalogOutput, MemoryCatalog},
};

/// The base time, a multiple of the window.
const B: u64 = 1_700_000_000;

const WINDOW_MS: u64 = 10_000;
const TICK_MS: u64 = 15_000;
const GRACE_MS: u64 = 2_000;

/// The contexts batches carry: counters, the first of which keeps state non-empty, and gauges. There are enough
/// counters that some are first seen after a few flushes, when their leading zeros depend on when the last flush ran.
const CONTEXTS: [&str; 8] = ["c0", "c1", "c2", "c3", "c4", "c5", "g0", "g1"];

fn is_counter(context: usize) -> bool {
    context < 6
}

fn plan() -> Plan {
    Plan::from_value(json!({
        "ir_version": 1,
        "nodes": [
            {"id": "src", "op": "source", "params": {"binding": "dogstatsd", "timestamps": "untimestamped"},
             "provenance": {"native": "dsd_in"}},
            {"id": "agg", "op": "aggregate", "provenance": {"native": "dsd_agg"}, "params": {
                "window": "10s", "flush_interval": "15s", "context_limit": 1000, "counter_expiry": "5m",
                "flush_open_windows": false, "histogram_aggregates": ["max", "count"], "histogram_percentiles": [],
                "copy_to_distribution": false, "copy_to_distribution_prefix": ""}},
            {"id": "out", "op": "emit", "params": {"output": "a"}, "provenance": {"native": "out"}},
        ],
        "edges": [{"from": "src.metrics", "to": "agg.in"}, {"from": "agg.out", "to": "out.in"}],
        "outputs": [{"id": "a", "accepts": {"kinds": "all", "timestamps": "mixed"},
                     "fingerprint": PlanHash::of_bytes(b"a").to_string(), "recipients": ["out/a"]}],
    }))
    .unwrap()
}

fn catalog() -> MemoryCatalog {
    MemoryCatalog::new().with_output(
        OutputId::new("a").unwrap(),
        CatalogOutput {
            accepts: PortType::ANY,
            fingerprint: PlanHash::of_bytes(b"a"),
            recipients: vec![RecipientId::new("out/a").unwrap()],
        },
    )
}

/// One batch the source stamps: how long after the previous stamp, how long it waits before it arrives, and its
/// points as `(context, value)`; a heartbeat when it has none.
#[derive(Clone, Debug)]
struct Stamped {
    gap_ms: u64,
    delay_ms: u64,
    points: Vec<(usize, u8)>,
}

#[derive(Clone, Debug)]
struct Case {
    /// Where in each second the ticks fall.
    phase_ms: u64,
    batches: Vec<Stamped>,
    /// How long the heartbeat the source sends at each tick's clock waits before it arrives.
    tick_heartbeat_delays_ms: Vec<u64>,
}

fn arb_case(max_gap_ms: u64, max_delay_ms: u64, max_tick_delay_ms: u64) -> impl Strategy<Value = Case> {
    let batch = (0..max_gap_ms, 0..=max_delay_ms, vec((0..CONTEXTS.len(), 1u8..4), 0..4)).prop_map(
        |(gap_ms, delay_ms, points)| Stamped {
            gap_ms,
            delay_ms,
            points,
        },
    );
    (0u64..1_000, vec(batch, 1..40), vec(0..=max_tick_delay_ms, 16)).prop_map(
        |(phase_ms, batches, tick_heartbeat_delays_ms)| Case {
            phase_ms,
            batches,
            tick_heartbeat_delays_ms,
        },
    )
}

/// One event of the timeline, at the time it happens, in milliseconds relative to `B`.
#[derive(Clone, Debug)]
enum Event {
    /// A batch, or a heartbeat without points, stamped `stamp` and arriving at `at`.
    Arrive {
        at: u64,
        stamp: u64,
        points: Vec<(usize, u8)>,
    },

    /// A flush tick at wall time `at`.
    Tick { at: u64 },
}

impl Event {
    fn at(&self) -> u64 {
        match self {
            Self::Arrive { at, .. } | Self::Tick { at } => *at,
        }
    }
}

/// Lays a case out as a timeline, in arrival order: batches arrive in the order they were stamped, each no earlier
/// than its stamp plus its delay, and a batch arriving at a tick's time arrives before it.
///
/// With `contained`, a batch's delay never takes its arrival past the end of its bucket, nor past the first tick whose
/// wall-clock second is later than its stamp.
fn timeline(case: &Case, contained: bool) -> Vec<Event> {
    let mut stamps = Vec::new();
    let mut stamp = 500;
    for batch in &case.batches {
        stamp += batch.gap_ms;
        stamps.push((stamp, batch.delay_ms, batch.points.clone()));
    }
    let ticks = (1..)
        .map(|tick| tick * TICK_MS + case.phase_ms)
        .take_while(|&at| at <= stamp + TICK_MS)
        .collect::<Vec<_>>();
    for (tick, &at) in ticks.iter().enumerate() {
        stamps.push((
            at,
            case.tick_heartbeat_delays_ms[tick % case.tick_heartbeat_delays_ms.len()],
            Vec::new(),
        ));
    }
    // A first counter at 0.5 s, before every tick, keeps aggregation's state non-empty.
    stamps.push((500, 0, vec![(0, 1)]));
    if contained {
        for (stamp, delay, _) in &mut stamps {
            let next_tick = ticks
                .iter()
                .copied()
                .find(|&at| at / 1_000 * 1_000 > *stamp)
                .unwrap_or(u64::MAX);
            *delay = (*delay).min(WINDOW_MS - 1 - *stamp % WINDOW_MS).min(next_tick - *stamp);
        }
    }
    stamps.sort_by_key(|(stamp, _, _)| *stamp);

    let mut events = Vec::new();
    let mut arrived = 0;
    for (stamp, delay, points) in stamps {
        arrived = arrived.max(stamp + delay);
        events.push(Event::Arrive {
            at: arrived,
            stamp,
            points,
        });
    }
    // The end: a heartbeat past every bucket that holds a point, arriving at once, and a tick at it.
    let end = (arrived.max(stamp) / WINDOW_MS + 2) * WINDOW_MS;
    events.push(Event::Arrive {
        at: end,
        stamp: end,
        points: Vec::new(),
    });
    events.extend(ticks.into_iter().map(|at| Event::Tick { at }));
    events.push(Event::Tick { at: end });
    // A stable sort, so that arrivals keep their order, and an arrival at a tick's time comes first.
    events.sort_by_key(|event| (event.at(), matches!(event, Event::Tick { .. })));
    events
}

fn time(relative_ms: u64) -> AdmissionTime {
    AdmissionTime::from_unix_nanos((B * 1_000 + relative_ms) * 1_000_000)
}

fn metric(context: usize, value: u8) -> Metric {
    let context_ref = Context::from_static_name(CONTEXTS[context]);
    if is_counter(context) {
        Metric::counter(context_ref, f64::from(value))
    } else {
        Metric::gauge(context_ref, f64::from(value))
    }
}

/// One point of aggregation's output: its context, kind, bucket start in seconds relative to `B`, and value.
type Point = (String, String, u64, String);

fn points_of(metric: &Metric) -> Vec<Point> {
    let (kind, points) = match metric.values() {
        MetricValues::Rate(points, interval) => (format!("rate/{}s", interval.as_secs()), points),
        MetricValues::Gauge(points) => ("gauge".to_string(), points),
        other => panic!("unexpected values {:?}", other),
    };
    points
        .into_iter()
        .map(|(ts, value)| {
            (
                metric.context().name().to_string(),
                kind.clone(),
                ts.map_or(0, |ts| ts.get() - B),
                value.to_string(),
            )
        })
        .collect()
}

/// Runs native aggregation over the timeline: each batch at its arrival time, each flush at its tick's time, both
/// floored to whole seconds.
fn native(events: &[Event]) -> Vec<Point> {
    let plan = plan();
    let node = plan.nodes.iter().find(|node| node.id.to_string() == "agg").unwrap();
    let op = OperatorRegistry::native()
        .configure(node.op.as_str(), &node.params)
        .unwrap();
    let mut core = AggregateCore::new(aggregate_params(op.params()).unwrap(), AggregateTelemetry::noop());
    let mut out = Vec::new();
    for event in events {
        match event {
            Event::Arrive { at, points, .. } => {
                core.admit(
                    B + at / 1_000,
                    points.iter().map(|&(context, value)| metric(context, value)),
                );
            }
            Event::Tick { at } => {
                core.flush_into(B + at / 1_000, false, |metric| out.extend(points_of(&metric)));
            }
        }
    }
    out
}

/// Runs the interpreter over the timeline, with aggregation as a kernel: each batch stamped with its admission time
/// and delivered at its arrival, each tick at its wall time.
fn interpreted(events: &[Event]) -> Vec<Point> {
    let registry = OperatorRegistry::native();
    let catalog = catalog();
    let source: crate::ir::NodeId = "src".parse().unwrap();
    let mut trace = Trace::new().install(plan());
    for event in events {
        trace = match event {
            Event::Arrive { stamp, points, .. } => trace.admit(
                source.clone(),
                time(*stamp),
                points.iter().map(|&(context, value)| metric(context, value)).collect(),
            ),
            Event::Tick { at } => trace.advance(time(*at)),
        };
    }
    let run = Interpreter::new(&registry, &catalog).run(&trace).unwrap();
    run.output(&OutputId::new("a").unwrap())
        .iter()
        .flat_map(|record| points_of(&record.metric))
        .collect()
}

/// What the comparison leaves out: the `(context, bucket)` pairs of (a), the contexts of (b), and whether (c) leaves
/// out every zero.
#[derive(Debug, Default)]
struct Exclusions {
    buckets: BTreeSet<(String, u64)>,
    contexts: BTreeSet<String>,
    zeros: bool,
}

impl Exclusions {
    fn of(events: &[Event]) -> Self {
        let mut exclusions = Self::default();
        let bucket = |ms: u64| ms / WINDOW_MS * (WINDOW_MS / 1_000);
        let ticks = events
            .iter()
            .enumerate()
            .filter_map(|(position, event)| match event {
                Event::Tick { at } => Some((position, *at)),
                Event::Arrive { .. } => None,
            })
            .collect::<Vec<_>>();
        for (position, event) in events.iter().enumerate() {
            let Event::Arrive { at, stamp, points } = event else {
                continue;
            };
            for &(context, _) in points {
                let name = CONTEXTS[context].to_string();
                // (a): the admission time's bucket and the arrival's differ.
                if bucket(*stamp) != bucket(*at) {
                    exclusions.buckets.insert((name.clone(), bucket(*stamp)));
                    exclusions.buckets.insert((name.clone(), bucket(*at)));
                }
                // (b): stamped before a tick's wall-clock second, and arrived after the tick.
                if ticks
                    .iter()
                    .any(|&(tick, tick_at)| tick < position && *stamp < tick_at / 1_000 * 1_000)
                {
                    exclusions.contexts.insert(name);
                }
            }
        }
        // (c): a tick that finds the watermark short of its second, and that no stamp at or past its second releases
        // before the next tick, which comes after its grace.
        for (index, &(tick, tick_at)) in ticks.iter().enumerate() {
            let second = tick_at / 1_000 * 1_000;
            let watermark = events[..tick]
                .iter()
                .filter_map(|event| match event {
                    Event::Arrive { stamp, .. } => Some(*stamp),
                    Event::Tick { .. } => None,
                })
                .max();
            if watermark.is_some_and(|watermark| watermark / 1_000 * 1_000 >= second) {
                continue;
            }
            let next = ticks.get(index + 1).map_or(events.len(), |&(next, _)| next);
            let released = events[tick..next]
                .iter()
                .any(|event| matches!(event, Event::Arrive { stamp, .. } if stamp / 1_000 * 1_000 >= second));
            let expires = ticks
                .get(index + 1)
                .is_some_and(|&(_, next_at)| next_at >= tick_at + GRACE_MS);
            if !released && expires {
                exclusions.zeros = true;
            }
        }
        exclusions
    }

    fn keeps(&self, point: &Point) -> bool {
        let (context, kind, bucket, value) = point;
        let excluded_zero = self.zeros && kind.starts_with("rate") && value == "0";
        !self.contexts.contains(context) && !self.buckets.contains(&(context.clone(), *bucket)) && !excluded_zero
    }
}

fn sorted(mut points: Vec<Point>) -> Vec<Point> {
    points.sort();
    points
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn property_test_bucket_assignment_differs_only_across_boundaries(
        case in arb_case(4_000, 6_000, 4_000),
    ) {
        let events = timeline(&case, false);
        let exclusions = Exclusions::of(&events);
        let native = sorted(native(&events).into_iter().filter(|point| exclusions.keeps(point)).collect());
        let interpreted = sorted(interpreted(&events).into_iter().filter(|point| exclusions.keeps(point)).collect());
        prop_assert_eq!(interpreted, native, "{:?}\n{:?}", exclusions, events);
    }

    #[test]
    fn property_test_bucket_assignment_is_native_without_straddling_batches_or_an_expired_grace(
        case in arb_case(4_000, 300, 1_000),
    ) {
        // Delays stay within each batch's bucket, below a third of a second, and short of the next tick, and the
        // heartbeat at each tick arrives within a second: nothing straddles a bucket or crosses a tick, and every tick
        // fires at its second within the grace.
        let events = timeline(&case, true);
        let exclusions = Exclusions::of(&events);
        prop_assert!(
            exclusions.buckets.is_empty() && exclusions.contexts.is_empty() && !exclusions.zeros,
            "{:?}\n{:?}",
            exclusions,
            events
        );
        let native = sorted(native(&events));
        let interpreted = sorted(interpreted(&events));
        prop_assert_eq!(interpreted, native, "{:?}", events);
    }
}

#[test]
fn a_batch_queued_across_a_bucket_boundary_lands_in_its_admission_times_bucket() {
    // Stamped 9.8, arriving 10.4: native aggregation puts it in [10, 20), the kernel in [0, 10). The comparison
    // leaves both buckets of its context out, and keeps the others.
    let case = Case {
        phase_ms: 0,
        batches: vec![Stamped {
            gap_ms: 9_300,
            delay_ms: 600,
            points: vec![(1, 2), (6, 5)],
        }],
        tick_heartbeat_delays_ms: vec![0],
    };
    let events = timeline(&case, false);
    let native = sorted(native(&events));
    let interpreted = sorted(interpreted(&events));
    let point = |context: &str, kind: &str, bucket: u64, value: &str| {
        (context.to_string(), kind.to_string(), bucket, value.to_string())
    };
    assert!(native.contains(&point("c1", "rate/10s", 10, "2")), "{:?}", native);
    assert!(
        interpreted.contains(&point("c1", "rate/10s", 0, "2")),
        "{:?}",
        interpreted
    );
    assert!(native.contains(&point("g0", "gauge", 10, "5")), "{:?}", native);
    assert!(interpreted.contains(&point("g0", "gauge", 0, "5")), "{:?}", interpreted);
    // The first flush, at 15, emits no zeros, so native aggregation's counter has nothing at [0, 10), where the
    // kernel's has its point.
    assert!(!native.contains(&point("c1", "rate/10s", 0, "0")), "{:?}", native);

    let exclusions = Exclusions::of(&events);
    assert!(exclusions.buckets.contains(&("c1".to_string(), 0)));
    assert!(exclusions.buckets.contains(&("c1".to_string(), 10)));
    let kept = |points: Vec<Point>| sorted(points.into_iter().filter(|point| exclusions.keeps(point)).collect());
    assert_eq!(kept(interpreted), kept(native));
}
