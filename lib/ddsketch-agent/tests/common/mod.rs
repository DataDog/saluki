#![allow(dead_code)]

use datadog_protos::metrics::Dogsketch;
use ddsketch_agent::DDSketch;
use rand::SeedableRng;
use rand_distr::{Distribution, Pareto};

pub fn insert_single_points(ns: &[f64]) {
    let mut sketch = DDSketch::default();
    for i in ns {
        sketch.insert(*i);
    }
}

pub fn insert_single_and_serialize(ns: &[f64]) {
    let mut sketch = DDSketch::default();
    for i in ns {
        sketch.insert(*i);
    }

    let mut dogsketch = Dogsketch::new();
    sketch.merge_to_dogsketch(&mut dogsketch);
}

pub fn insert_many_and_serialize(ns: &[f64]) {
    let mut sketch = DDSketch::default();
    sketch.insert_many(ns);

    let mut dogsketch = Dogsketch::new();
    sketch.merge_to_dogsketch(&mut dogsketch);
}

pub fn make_points(size: usize) -> Vec<f64> {
    // Generate a set of samples that roughly correspond to the latency of a
    // typical web service, in microseconds, with a gamma distribution: big hump
    // at the beginning with a long tail.  We limit this so the samples
    // represent latencies that bottom out at 15 milliseconds and tail off all
    // the way up to 10 seconds.
    let distribution = Pareto::new(1.0, 1.0).expect("pareto distribution should be valid");
    let seed = 0xC0FFEE;

    let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);
    distribution
        .sample_iter(&mut rng)
        // Scale by 10,000 to get microseconds.
        .map(|n| n * 10_000.0)
        .filter(|n| *n > 15_000.0 && *n < 10_000_000.0)
        .take(size)
        .collect::<Vec<_>>()
}
