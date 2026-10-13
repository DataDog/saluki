//! The cost of a shared `StateBudget` under churn: P threads re-admit one million contexts after a flush expired
//! them, each new context taking one unit of the global budget and one of its node's, as aggregation admits a new
//! context when it shares budgets (`saluki-metric-kernels`, `aggregate/state.rs`): node units first, bounded by the
//! node's limit, then global units, giving the node's units back if the global budget is full.
//!
//! Two protocols, for P in 1, 2, 4, 8, and 16 threads, each re-admitting `1M / P` contexts at once:
//!
//! - **per context**, as aggregation admits new contexts on one lane: one bounded acquisition of the node's unit and
//!   one of the global unit for every context;
//! - **reserved**, as a lane of a state owner on several lanes admits them: the contexts arrive in batches, and each
//!   batch takes its units through one `BudgetReservation`, in blocks of up to 64 units, returning the ones it did not
//!   use when it ends. A **cold** batch's blocks start at one unit, as the first batch of a churn does; a **warm**
//!   batch's start at as many units as the previous batch took, as every later batch of a churn does.
//!
//! The batch sizes are a state lane's routed buffers, `clamp(2,048 / P, 256, 1,024)` events (the plan crate's
//! `ROUTE_BUFFER_EVENTS`): 1,024 at P = 2, 512 at P = 4, and 256 at P = 8 and 16; every size runs at every P, and 64
//! shows blocks that never grow to their largest. The shapes:
//!
//! - **`acquire_only`**: back-to-back admissions, the worst case for contention on the two counters' cache lines;
//!   `reserved_acquire_only` and `reserved_warm_acquire_only` in batches of 1,024 contexts,
//!   `reserved_acquire_only_batch_{64,256,512}` and `reserved_warm_acquire_only_batch_{256,512}` in smaller ones;
//! - **`with_inserts`**: each admission followed by an insert into a table of the thread's own, standing in for the
//!   aggregation state's insert of the new context; `reserved_with_inserts` in cold batches of 1,024, and
//!   `reserved_warm_with_inserts_batch_256` in warm batches of 256;
//! - **`inserts_only`**: the inserts without the budget, the baseline.
//!
//! The stand-in insert costs a tenth of what aggregation's insert of a new context does, so `1 - inserts_only /
//! with_inserts` overstates the budget's share of a state lane's time: the share that decides is measured on
//! aggregation itself, by the `aggregate_churn` bench of `saluki-metric-kernels`, at each P's routed batch size. This
//! bench isolates the budget's own cost per context per lane, `time / (1M / P)`.
//!
//! Throughput is contexts per second over all threads; the time is from the moment every thread is released until the
//! last one finishes. The releases that empty the budget before the next iteration (aggregation releases one unit per
//! expired context, or a batch at once at a flush) are not timed.

use std::{
    collections::HashMap,
    hint::black_box,
    sync::Barrier,
    thread,
    time::{Duration, Instant},
};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use saluki_core::execution::{BudgetReservation, StateBudget};

/// The contexts re-admitted in all, per iteration.
const CONTEXTS: u64 = 1_000_000;

/// The threads that re-admit them.
const THREADS: [usize; 5] = [1, 2, 4, 8, 16];

/// The node's context limit: above what is admitted, so that nothing is refused.
const NODE_LIMIT: u64 = 2 * CONTEXTS;

/// How each context takes its units of the budgets.
#[derive(Clone, Copy)]
enum Protocol {
    /// It takes none: the baseline.
    Unbudgeted,

    /// One bounded acquisition of the node's unit and one of the global unit, for every context.
    PerContext,

    /// The contexts arrive in batches of `batch`, and each batch takes its units through one reservation, whose blocks
    /// start at one unit, or, if `warm`, at as many as the previous batch took.
    Reserved { batch: u64, warm: bool },
}

/// Admits one new context as aggregation does on one lane when it shares budgets: `true` if it holds a unit of each
/// budget.
#[inline]
fn admit(global: &StateBudget, node: &StateBudget) -> bool {
    if !node.try_acquire_below(1, NODE_LIMIT) {
        return false;
    }
    if !global.try_acquire(1) {
        node.release(1);
        return false;
    }
    true
}

/// Runs one re-admission of [`CONTEXTS`] contexts over `threads` threads and returns its time, from the release of
/// every thread to the end of the last one. Each context takes its units as `protocol` says, and, with `inserts`, is
/// then inserted into the thread's own table.
fn readmit(threads: usize, protocol: Protocol, inserts: bool) -> Duration {
    let global = StateBudget::new(2 * CONTEXTS);
    let node = StateBudget::unlimited();
    let per_thread = CONTEXTS / threads as u64;
    let start = Barrier::new(threads + 1);
    let elapsed = thread::scope(|scope| {
        let workers = (0..threads)
            .map(|thread| {
                let (global, node, start) = (&global, &node, &start);
                scope.spawn(move || {
                    let mut table = HashMap::with_capacity(if inserts { per_thread as usize } else { 0 });
                    let mut insert = |key: u64| {
                        if inserts {
                            table.insert(black_box(key), key);
                        }
                    };
                    start.wait();
                    let first = thread as u64 * per_thread;
                    let keys = first..first + per_thread;
                    match protocol {
                        Protocol::Unbudgeted => keys.for_each(insert),
                        Protocol::PerContext => {
                            for key in keys {
                                assert!(admit(global, node), "the budget has room for every context");
                                insert(key);
                            }
                        }
                        Protocol::Reserved { batch, warm } => {
                            let (mut key, mut previous) = (keys.start, 0);
                            while key < keys.end {
                                let end = keys.end.min(key + batch);
                                let first_block = if warm { previous } else { 1 };
                                let mut reservation =
                                    BudgetReservation::new(node, NODE_LIMIT, global).with_first_block(first_block);
                                for key in key..end {
                                    assert!(reservation.take(), "the budget has room for every context");
                                    insert(key);
                                }
                                previous = reservation.taken();
                                drop(reservation);
                                key = end;
                            }
                        }
                    }
                    let finished = Instant::now();
                    // Expiry at the next flush: one release per thread, untimed.
                    if !matches!(protocol, Protocol::Unbudgeted) {
                        global.release(per_thread);
                        node.release(per_thread);
                    }
                    black_box(table);
                    finished
                })
            })
            .collect::<Vec<_>>();
        start.wait();
        let released = Instant::now();
        workers
            .into_iter()
            .map(|worker| worker.join().expect("a worker does not panic"))
            .max()
            .expect("there is a worker")
            .saturating_duration_since(released)
    });
    debug_assert_eq!((global.used(), node.used()), (0, 0));
    elapsed
}

fn state_budget(c: &mut Criterion) {
    let mut group = c.benchmark_group("state_budget");
    group.sample_size(10);
    group.throughput(Throughput::Elements(CONTEXTS));
    let cold = |batch| Protocol::Reserved { batch, warm: false };
    let warm = |batch| Protocol::Reserved { batch, warm: true };
    let shapes = [
        ("acquire_only", Protocol::PerContext, false),
        ("with_inserts", Protocol::PerContext, true),
        ("inserts_only", Protocol::Unbudgeted, true),
        ("reserved_acquire_only", cold(1_024), false),
        ("reserved_acquire_only_batch_64", cold(64), false),
        ("reserved_acquire_only_batch_256", cold(256), false),
        ("reserved_acquire_only_batch_512", cold(512), false),
        ("reserved_warm_acquire_only", warm(1_024), false),
        ("reserved_warm_acquire_only_batch_256", warm(256), false),
        ("reserved_warm_acquire_only_batch_512", warm(512), false),
        ("reserved_with_inserts", cold(1_024), true),
        ("reserved_warm_with_inserts_batch_256", warm(256), true),
    ];
    for (shape, protocol, inserts) in shapes {
        for threads in THREADS {
            group.bench_with_input(BenchmarkId::new(shape, threads), &threads, |b, &threads| {
                b.iter_custom(|iterations| (0..iterations).map(|_| readmit(threads, protocol, inserts)).sum());
            });
        }
    }
    group.finish();
}

criterion_group!(benches, state_budget);
criterion_main!(benches);
