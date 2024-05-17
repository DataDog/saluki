use datadog_protos::metrics::Dogsketch;
use ddsketch_agent::Sketch;
use dhat::HeapStats;
use rand::SeedableRng;
use rand_distr::{Distribution, Pareto};

pub fn insert_single_and_serialize<T: Sketch + Default>(ns: &[f64]) {
    let mut sketch = T::default();
    for i in ns {
        sketch.insert(*i);
    }

    let mut dogsketch = Dogsketch::new();
    let _ = sketch.merge_to_dogsketch(&mut dogsketch);
}

pub fn insert_many_and_serialize<T: Sketch + Default>(ns: &[f64]) {
    let mut sketch = T::default();
    sketch.insert_many(&ns);

    let mut dogsketch = Dogsketch::new();
    let _ = sketch.merge_to_dogsketch(&mut dogsketch);
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

#[non_exhaustive]
pub struct MathableHeapStats {
    pub total_blocks: u64,
    pub total_bytes: u64,
    pub max_blocks: usize,
    pub max_bytes: usize,
    pub curr_blocks: usize,
    pub curr_bytes: usize,
}

impl From<HeapStats> for MathableHeapStats {
    fn from(stats: HeapStats) -> Self {
        Self {
            total_blocks: stats.total_blocks,
            total_bytes: stats.total_bytes,
            max_blocks: stats.max_blocks,
            max_bytes: stats.max_bytes,
            curr_blocks: stats.curr_blocks,
            curr_bytes: stats.curr_bytes,
        }
    }
}

impl std::ops::Sub for MathableHeapStats {
    type Output = MathableHeapStats;

    fn sub(self, rhs: MathableHeapStats) -> Self::Output {
        MathableHeapStats {
            total_blocks: self.total_blocks - rhs.total_blocks,
            total_bytes: self.total_bytes - rhs.total_bytes,
            max_blocks: self.max_blocks - rhs.max_blocks,
            max_bytes: self.max_bytes - rhs.max_bytes,
            curr_blocks: self.curr_blocks - rhs.curr_blocks,
            curr_bytes: self.curr_bytes - rhs.curr_bytes,
        }
    }
}
