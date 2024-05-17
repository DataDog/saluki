use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use datadog_protos::metrics::Dogsketch;
use ddsketch_agent::{BufferedDDSketch, DDSketch, Sketch};
use rand::SeedableRng;
use rand_distr::{Distribution, Pareto};

fn insert_single_and_serialize<T: Sketch + Default>(ns: &[f64]) {
    let mut sketch = T::default();
    for i in ns {
        sketch.insert(*i);
    }

    let mut dogsketch = Dogsketch::new();
    let _ = sketch.merge_to_dogsketch(&mut dogsketch);
}

fn insert_many_and_serialize<T: Sketch + Default>(ns: &[f64]) {
    let mut sketch = T::default();
    sketch.insert_many(&ns);

    let mut dogsketch = Dogsketch::new();
    let _ = sketch.merge_to_dogsketch(&mut dogsketch);
}

fn bench_sketch<T: Sketch + Default>(c: &mut Criterion) {
    let sizes = [1, 2, 5, 10, 100, 1_000, 10_000];

    // Generate a set of samples that roughly correspond to the latency of a
    // typical web service, in microseconds, with a gamma distribution: big hump
    // at the beginning with a long tail.  We limit this so the samples
    // represent latencies that bottom out at 15 milliseconds and tail off all
    // the way up to 10 seconds.
    let distribution = Pareto::new(1.0, 1.0).expect("pareto distribution should be valid");

    let seed = 0xC0FFEE;

    let mut group = c.benchmark_group(format!("{}/insert-single", std::any::type_name::<T>()));
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
            b.iter(|| insert_single_and_serialize::<T>(&vals));
        });
    }
    group.finish();

    let mut group = c.benchmark_group(format!("{}/insert-many", std::any::type_name::<T>()));
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
            b.iter(|| insert_many_and_serialize::<T>(&vals));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_sketch::<DDSketch>, bench_sketch::<BufferedDDSketch>);
criterion_main!(benches);
