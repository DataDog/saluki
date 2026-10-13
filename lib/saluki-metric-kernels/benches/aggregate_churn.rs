//! The share of a state lane's time per new context that aggregation's shared context budgets take under churn: the
//! measurement that decides whether per-batch reservations are enough (the stage 9 design's M4 on aggregation itself,
//! and O1: the budgets' operations within 5% of a state lane's time per new context).
//!
//! P lanes, for P in 1, 2, 4, 8, and 16, each an `AggregateCore` on a thread of its own, re-admit 2^20 new gauge
//! contexts in all after a flush expired them, `2^20 / P` each: the re-insertion burst after a flush. They arrive in
//! batches of a state lane's routed buffer at P, `clamp(2,048 / P, 256, 1,024)` events (the plan crate's
//! `ROUTE_BUFFER_EVENTS`), every event a new context. Three protocols:
//!
//! - **`exclusive`**: cores that admit by their own counts and touch no shared count, the baseline;
//! - **`per_context`**: cores that share one node's count and the global budget, each on one lane, with one
//!   acquisition of each per new context;
//! - **`reserved`**: cores that share them as one state owner's lanes, which take blocks through one reservation per
//!   batch on two lanes or more, and one unit per new context on one.
//!
//! The budgets' share per new context per lane is `per_context / exclusive - 1` and `reserved / exclusive - 1`, of
//! the times the report gives for one P. The time of one iteration is from the moment every lane's thread is released
//! until the last one finishes; the flush that empties the cores before the next iteration is not timed. Run it in
//! release mode on an otherwise idle machine:
//!
//! ```text
//! cargo bench -p saluki-metric-kernels --bench aggregate_churn
//! ```

use std::{
    num::NonZeroU64,
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use saluki_core::{
    data_model::{
        event::metric::{context::Context, Metric},
        tags::TagSet,
    },
    execution::StateBudget,
};
use saluki_metric_kernels::aggregate::{
    AggregateCore, AggregateParams, AggregateTelemetry, ContextBudget, HistogramConfiguration, LaneShared,
};

/// The contexts re-admitted in all, per iteration.
const CONTEXTS: usize = 1 << 20;

/// The lanes that re-admit them.
const LANES: [usize; 5] = [1, 2, 4, 8, 16];

/// The admission time of the first iteration's contexts, in Unix seconds.
const START: u64 = 1_700_000_000;

/// How one lane's core takes the units of the shared budgets for its new contexts.
#[derive(Clone, Copy)]
enum Churn {
    /// Exclusive cores, which admit by their own counts and touch no shared count: the baseline.
    Exclusive,
    /// Cores that share budgets, each on one lane: one acquisition of each budget per new context.
    PerContext,
    /// Cores that share budgets as the lanes of one state owner: in blocks, through one reservation per batch.
    Reserved,
}

impl Churn {
    fn name(self) -> &'static str {
        match self {
            Self::Exclusive => "exclusive",
            Self::PerContext => "per_context",
            Self::Reserved => "reserved",
        }
    }
}

/// The parameters of every core: 10-second buckets, room for every context, no counter expiry, and no histogram
/// statistics, since every context is a gauge.
fn params() -> AggregateParams {
    AggregateParams {
        window: NonZeroU64::new(10).expect("not zero"),
        context_limit: 2 * CONTEXTS,
        counter_expiry: None,
        flush_open_windows: false,
        histograms: HistogramConfiguration::try_new(&[], &[], false, String::new())
            .expect("no histogram statistics is a valid configuration"),
    }
}

/// Builds the cores of `lanes` lanes under `churn`, the sharing ones on budgets with room for every context.
fn cores(churn: Churn, lanes: usize) -> Vec<AggregateCore> {
    let budget = ContextBudget {
        global: StateBudget::new(2 * CONTEXTS as u64),
        node: StateBudget::unlimited(),
    };
    let shared = Arc::new(LaneShared::new(u16::try_from(lanes).expect("a few lanes")));
    (0..lanes)
        .map(|_| match churn {
            Churn::Exclusive => AggregateCore::new(params(), AggregateTelemetry::noop()),
            Churn::PerContext => AggregateCore::with_budget(params(), budget.clone(), AggregateTelemetry::noop()),
            Churn::Reserved => AggregateCore::with_budget(params(), budget.clone(), AggregateTelemetry::noop())
                .with_lanes(Arc::clone(&shared)),
        })
        .collect()
}

/// Returns each lane's batches of new gauge contexts: `CONTEXTS / lanes` per lane, in batches of the lane's routed
/// buffer.
fn batches(lanes: usize) -> Vec<Vec<Vec<Metric>>> {
    let batch = (2_048 / lanes).clamp(256, 1_024);
    (0..lanes)
        .map(|lane| {
            (0..CONTEXTS / lanes)
                .map(|index| {
                    let name = format!("lane{}.context{}", lane, index);
                    Metric::gauge(Context::from_parts(name, TagSet::default()), 1.0)
                })
                .collect::<Vec<_>>()
                .chunks(batch)
                .map(<[Metric]>::to_vec)
                .collect()
        })
        .collect()
}

/// Re-admits `batches[lane]` into `cores[lane]` for every lane at once, each lane on a thread of its own, at `at`, and
/// returns the time from releasing the threads to the last one's end; then flushes every context away, outside the
/// timing.
fn readmit(cores: &mut [AggregateCore], batches: &[Vec<Vec<Metric>>], at: u64) -> Duration {
    let batches = batches.to_vec();
    let start = Barrier::new(cores.len() + 1);
    let elapsed = thread::scope(|scope| {
        let lanes = cores
            .iter_mut()
            .zip(batches)
            .map(|(core, batches)| {
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    for batch in batches {
                        let outcome = core.admit(at, batch);
                        assert_eq!(outcome.dropped, 0, "the budgets have room for every context");
                    }
                    Instant::now()
                })
            })
            .collect::<Vec<_>>();
        start.wait();
        let released = Instant::now();
        lanes
            .into_iter()
            .map(|lane| lane.join().expect("a lane does not panic"))
            .max()
            .expect("there is a lane")
            .saturating_duration_since(released)
    });
    for core in cores.iter_mut() {
        core.flush_into(at + 50, false, |_| {});
        assert!(core.is_empty(), "the flush expired every context");
    }
    elapsed
}

fn bench_churn(c: &mut Criterion) {
    let mut group = c.benchmark_group("aggregate_churn");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(20));
    group.throughput(Throughput::Elements(CONTEXTS as u64));
    for lanes in LANES {
        let batches = batches(lanes);
        for churn in [Churn::Exclusive, Churn::PerContext, Churn::Reserved] {
            let mut cores = cores(churn, lanes);
            let mut round = 0u64;
            group.bench_with_input(BenchmarkId::new(churn.name(), lanes), &lanes, |b, _| {
                b.iter_custom(|iterations| {
                    (0..iterations)
                        .map(|_| {
                            round += 1;
                            readmit(&mut cores, &batches, START + 100 * round)
                        })
                        .sum()
                })
            });
        }
    }
    group.finish();
}

criterion_group!(benches, bench_churn);
criterion_main!(benches);
