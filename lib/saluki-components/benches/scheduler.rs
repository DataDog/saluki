// Scheduler sensitivity benchmark targeting tokio's LIFO slot behavior.
//
// In tokio < 1.51, the LIFO slot was private per worker: when task A wakes task B,
// task B is placed in A's worker LIFO slot and runs next on that same thread.
// This is ideal for producer-consumer pipelines — no cross-thread cache thrash.
//
// In tokio 1.51 (PR #7431), the LIFO slot became stealable: other worker threads
// can take the task from the LIFO slot before the waking thread runs it. For a
// tight single-connection producer-consumer pipeline (like the OTLP ingest path),
// every message hop can now incur a cross-core cache miss.
//
// To expose this:
//   - Use a channel capacity of 1 so every send/receive causes a task wakeup.
//   - Do minimal per-message work so scheduling overhead dominates.
//   - Run N concurrent pipelines on N workers to maximize stealing opportunities.
//
// Run with:
//   cargo bench --bench scheduler -p saluki-components

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use futures::future::join_all;
use tokio::sync::mpsc;

// A single tight producer-consumer pair. channel_capacity=1 forces every send to
// block until the consumer receives, creating a wakeup on every single message.
async fn producer_consumer_pair(n_messages: usize, channel_capacity: usize) -> u64 {
    let (tx, mut rx) = mpsc::channel::<u64>(channel_capacity);

    let producer = tokio::spawn(async move {
        for i in 0..n_messages as u64 {
            tx.send(i).await.unwrap();
        }
    });

    let consumer = tokio::spawn(async move {
        let mut sum = 0u64;
        while let Some(v) = rx.recv().await {
            // Minimal work: just accumulate. Keeps per-message CPU cost low so
            // scheduling overhead is a large fraction of total time.
            sum = sum.wrapping_add(v);
        }
        sum
    });

    producer.await.unwrap();
    consumer.await.unwrap()
}

fn bench_lifo_scheduling(c: &mut Criterion) {
    // 4 workers matches the ADP production config and gives other threads a chance
    // to steal from the LIFO slot.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();

    let n_messages = 10_000usize;
    let mut group = c.benchmark_group("lifo_scheduling");
    group.throughput(Throughput::Elements(n_messages as u64));

    // Single pair, tight channel (capacity=1).
    // Baseline case: 1 producer + 1 consumer, maximum wakeup frequency.
    group.bench_function(BenchmarkId::new("single_pair", "cap1"), |b| {
        b.iter(|| rt.block_on(producer_consumer_pair(n_messages, 1)))
    });

    // Four concurrent pairs, tight channels.
    // 4 pipelines on 4 workers creates direct competition for LIFO slots —
    // each worker that finishes a consumer wakeup can steal another pair's task.
    // This is the load pattern closest to the OTLP ingest path under real traffic.
    group.bench_function(BenchmarkId::new("four_pairs", "cap1"), |b| {
        b.iter(|| rt.block_on(async { join_all((0..4).map(|_| producer_consumer_pair(n_messages / 4, 1))).await }))
    });

    // Same load but with a larger buffer (capacity=64).
    // Messages batch up, fewer wakeups per message, LIFO behavior matters less.
    // Should show a smaller difference between tokio versions — acts as a control.
    group.bench_function(BenchmarkId::new("four_pairs", "cap64"), |b| {
        b.iter(|| rt.block_on(async { join_all((0..4).map(|_| producer_consumer_pair(n_messages / 4, 64))).await }))
    });

    group.finish();
}

criterion_group!(benches, bench_lifo_scheduling);
criterion_main!(benches);
