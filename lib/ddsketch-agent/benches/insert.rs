use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use ddsketch_agent::DDSketch;
use rand::SeedableRng;
use rand_distr::{Beta, Distribution};

fn insert_single(ns: &[f64]) {
    let mut sketch = DDSketch::default();
    for i in ns {
        sketch.insert(*i);
    }
}

fn insert_multi(ns: &[f64]) {
    let mut sketch = DDSketch::default();
    sketch.insert_many(&ns);
}

fn criterion_benchmark(c: &mut Criterion) {
    let sizes = [1, 2, 5, 10, 50, 100, 1_000, 10_000, 100_000];

    // https://www.wolframalpha.com/input?i=beta+distribution+with+a+%3D+1.6%2C+b+%3D+20
    // This distribution is scaled up by 1000x to approximate the response time
    // of a networked service in milliseconds.
    // Percentiles
    // 10th | 16.4519
    // 25th | 32.7133
    // 50th | 61.2044
    // 75th | 102.017
    // 90th | 149.339
    let distribution = Beta::new(1.2, 15.0).unwrap();

    let seed = 0xC0FFEE;

    let mut group = c.benchmark_group("insert-single");
    for size in sizes.iter() {
        group.throughput(Throughput::Elements(*size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, &size| {
            let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);
            let vals = distribution
                .sample_iter(&mut rng)
                .take(size)
                .map(|v| v * 1000.0)
                .collect::<Vec<_>>();
            b.iter(|| insert_single(&vals));
        });
    }
    group.finish();

    let mut group = c.benchmark_group("insert-many");
    for size in sizes.iter() {
        group.throughput(Throughput::Elements(*size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, &size| {
            let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);
            let vals = distribution
                .sample_iter(&mut rng)
                .take(size)
                .map(|v| v * 1000.0)
                .collect::<Vec<_>>();
            b.iter(|| insert_multi(&vals));
        });
    }
    group.finish();
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
