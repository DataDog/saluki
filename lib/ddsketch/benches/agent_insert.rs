use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use datadog_protos::metrics::Dogsketch;
use ddsketch::DDSketch;
use rand::SeedableRng;
use rand_distr::{Distribution, Pareto};

fn insert_single_and_serialize(ns: &[f64]) {
    let mut sketch: DDSketch = DDSketch::default();
    for i in ns {
        sketch.insert(*i);
    }

    let mut dogsketch = Dogsketch::new();
    sketch.merge_to_dogsketch(&mut dogsketch);
}

fn insert_many_and_serialize(ns: &[f64]) {
    let mut sketch = DDSketch::default();
    sketch.insert_many(ns);

    let mut dogsketch = Dogsketch::new();
    sketch.merge_to_dogsketch(&mut dogsketch);
}

fn bench_sketch(c: &mut Criterion) {
    let sizes = [1, 10, 100, 1_000, 10_000];

    // Generate a set of samples that roughly correspond to the latency of a
    // typical web service, in microseconds, with a gamma distribution: big hump
    // at the beginning with a long tail.  We limit this so the samples
    // represent latencies that bottom out at 15 milliseconds and tail off all
    // the way up to 10 seconds.
    let distribution = Pareto::new(1.0, 1.0).expect("pareto distribution should be valid");

    let seed = 0xC0FFEE;

    let mut group = c.benchmark_group("DDSketch/insert-single");
    for size in sizes.iter() {
        group.throughput(Throughput::Elements(*size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, &size| {
            let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);
            let vals = distribution
                .sample_iter(&mut rng)
                // Scale by 10,000 to get microseconds.
                .map(|n| n * 10_000.0)
                .filter(|n| *n > 15_000.0 && *n < 10_000_000.0)
                .take(size)
                .collect::<Vec<_>>();
            b.iter(|| insert_single_and_serialize(&vals));
        });
    }
    group.finish();

    let mut group = c.benchmark_group("DDSketch/insert-many");
    for size in sizes.iter() {
        group.throughput(Throughput::Elements(*size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, &size| {
            let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);
            let vals = distribution
                .sample_iter(&mut rng)
                // Scale by 10,000 to get microseconds.
                .map(|n| n * 10_000.0)
                .filter(|n| *n > 15_000.0 && *n < 10_000_000.0)
                .take(size)
                .collect::<Vec<_>>();
            b.iter(|| insert_many_and_serialize(&vals));
        });
    }
    group.finish();
}

fn bench_insert_n(c: &mut Criterion) {
    let mut group = c.benchmark_group("DDSketch/insert-n");
    for count in [1u64, 10, 100, 1_000, 10_000] {
        group.throughput(Throughput::Elements(count));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter(|| {
                let mut sketch: DDSketch = DDSketch::default();
                sketch.insert_n(12345.0, count);
                sketch
            });
        });
    }
    group.finish();
}

fn bench_merge(c: &mut Criterion) {
    let distribution = Pareto::new(1.0, 1.0).expect("pareto distribution should be valid");
    let seed = 0xC0FFEE;

    let mut group = c.benchmark_group("DDSketch/merge");
    for size in [10usize, 100, 500, 1000] {
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);
            let vals: Vec<f64> = distribution
                .sample_iter(&mut rng)
                .map(|n| n * 10_000.0)
                .filter(|n| *n > 15_000.0 && *n < 10_000_000.0)
                .take(size)
                .collect();

            let mut s1: DDSketch = DDSketch::default();
            let mut s2: DDSketch = DDSketch::default();
            s1.insert_many(&vals[..size / 2]);
            s2.insert_many(&vals[size / 2..]);

            b.iter(|| {
                let mut target = s1.clone();
                target.merge(&s2);
                target
            });
        });
    }
    group.finish();
}

fn bench_quantile(c: &mut Criterion) {
    let distribution = Pareto::new(1.0, 1.0).expect("pareto distribution should be valid");
    let seed = 0xC0FFEE;

    let mut group = c.benchmark_group("DDSketch/quantile");
    for size in [10usize, 100, 500, 1000] {
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);
            let vals: Vec<f64> = distribution
                .sample_iter(&mut rng)
                .map(|n| n * 10_000.0)
                .filter(|n| *n > 15_000.0 && *n < 10_000_000.0)
                .take(size)
                .collect();

            let mut sketch: DDSketch = DDSketch::default();
            sketch.insert_many(&vals);

            b.iter(|| sketch.quantile(0.5));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_sketch, bench_insert_n, bench_merge, bench_quantile);
criterion_main!(benches);
