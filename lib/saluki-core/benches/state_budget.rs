//! The cost of a shared `StateBudget` under churn: P threads re-admit one million contexts after a flush expired
//! them, each new context taking one unit of the global budget and one of its node's, as aggregation admits a new
//! context when it shares budgets (`saluki-metric-kernels`, `aggregate/state.rs`): read the node's count against its
//! limit, acquire one global unit, then one node unit.
//!
//! Three shapes, for P in 1, 2, 4, 8, and 16 threads, each re-admitting `1M / P` contexts at once:
//!
//! - **`acquire_only`**: back-to-back admissions, the worst case for contention on the two counters' cache lines;
//! - **`with_inserts`**: each admission followed by an insert into a table of the thread's own, standing in for the
//!   aggregation state's insert of the new context;
//! - **`inserts_only`**: the inserts without the budget, the baseline: `1 - inserts_only / with_inserts` is the share
//!   of time the budget takes.
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
use saluki_core::execution::StateBudget;

/// The contexts re-admitted in all, per iteration.
const CONTEXTS: u64 = 1_000_000;

/// The threads that re-admit them.
const THREADS: [usize; 5] = [1, 2, 4, 8, 16];

/// The node's context limit: above what is admitted, so that nothing is refused.
const NODE_LIMIT: u64 = 2 * CONTEXTS;

/// Admits one new context as aggregation does when it shares budgets: `true` if it holds a unit of each budget.
#[inline]
fn admit(global: &StateBudget, node: &StateBudget) -> bool {
    if node.used() >= NODE_LIMIT || !global.try_acquire(1) {
        return false;
    }
    if !node.try_acquire(1) {
        global.release(1);
        return false;
    }
    true
}

/// Runs one re-admission of [`CONTEXTS`] contexts over `threads` threads and returns its time, from the release of
/// every thread to the end of the last one. With `budget`, each context is admitted through the budgets; with
/// `inserts`, it is then inserted into the thread's own table.
fn readmit(threads: usize, budget: bool, inserts: bool) -> Duration {
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
                    start.wait();
                    let first = thread as u64 * per_thread;
                    for key in first..first + per_thread {
                        if budget {
                            assert!(admit(global, node), "the budget has room for every context");
                        }
                        if inserts {
                            table.insert(black_box(key), key);
                        }
                    }
                    let finished = Instant::now();
                    // Expiry at the next flush: one release per thread, untimed.
                    if budget {
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
    let shapes = [
        ("acquire_only", true, false),
        ("with_inserts", true, true),
        ("inserts_only", false, true),
    ];
    for (shape, budget, inserts) in shapes {
        for threads in THREADS {
            group.bench_with_input(BenchmarkId::new(shape, threads), &threads, |b, &threads| {
                b.iter_custom(|iterations| (0..iterations).map(|_| readmit(threads, budget, inserts)).sum());
            });
        }
    }
    group.finish();
}

criterion_group!(benches, state_budget);
criterion_main!(benches);
