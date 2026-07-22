use std::hint::black_box;
use std::mem::size_of;

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use saluki_components::transforms::{AggregateContextSnapshotBenchmarkHarness, AggregateContextSnapshotEntry};

const CONTEXT_COUNTS: [usize; 3] = [10_000, 100_000, 1_000_000];

fn benchmark_aggregate_context_snapshot(c: &mut Criterion) {
    let mut group = c.benchmark_group("aggregate_context_snapshot");

    for context_count in CONTEXT_COUNTS {
        let harness = AggregateContextSnapshotBenchmarkHarness::with_contexts(context_count);
        let retained_len = harness.retained_len();
        assert_eq!(retained_len, context_count);

        let preflight = harness.snapshot();
        assert_eq!(preflight.len(), retained_len);
        black_box(preflight);

        let structural_bytes = context_count * size_of::<AggregateContextSnapshotEntry>();
        let benchmark_id =
            BenchmarkId::from_parameter(format!("{context_count}_contexts_{structural_bytes}_structural_bytes"));
        group.throughput(Throughput::Elements(context_count as u64));
        group.bench_function(benchmark_id, |b| {
            b.iter_batched(
                || (),
                |()| {
                    let snapshot = harness.snapshot();
                    assert_eq!(snapshot.len(), retained_len);
                    black_box(snapshot)
                },
                BatchSize::PerIteration,
            );
        });
    }

    group.finish();
}

criterion_group!(benches, benchmark_aggregate_context_snapshot);
criterion_main!(benches);
