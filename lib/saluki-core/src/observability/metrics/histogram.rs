//! Fixed-bucket histogram aggregation for Prometheus exposition.

use std::sync::LazyLock;

use crate::data_model::event::metric::Histogram;

const TIME_HISTOGRAM_BUCKET_COUNT: usize = 30;
static TIME_HISTOGRAM_BUCKETS: LazyLock<[(f64, &'static str); TIME_HISTOGRAM_BUCKET_COUNT]> =
    LazyLock::new(|| histogram_buckets::<TIME_HISTOGRAM_BUCKET_COUNT>(0.000000128, 4.0));

const NON_TIME_HISTOGRAM_BUCKET_COUNT: usize = 30;
static NON_TIME_HISTOGRAM_BUCKETS: LazyLock<[(f64, &'static str); NON_TIME_HISTOGRAM_BUCKET_COUNT]> =
    LazyLock::new(|| histogram_buckets::<NON_TIME_HISTOGRAM_BUCKET_COUNT>(1.0, 2.0));

/// An aggregated histogram with fixed buckets, suitable for Prometheus exposition.
///
/// Bucket layout follows a log-linear schedule. A metric name ending in `_seconds` selects the
/// time-oriented bucket schedule; everything else uses the non-time schedule.
#[derive(Clone, Debug)]
pub struct AggregatedHistogram {
    sum: f64,
    count: u64,
    buckets: Vec<(f64, &'static str, u64)>,
}

impl AggregatedHistogram {
    /// Creates a new `AggregatedHistogram` for the given metric name.
    ///
    /// The metric name determines the bucket schedule: names ending in `_seconds` use a
    /// time-oriented schedule, everything else uses a generic non-time schedule.
    pub fn new(metric_name: &str) -> Self {
        let base_buckets = if metric_name.ends_with("_seconds") {
            &TIME_HISTOGRAM_BUCKETS[..]
        } else {
            &NON_TIME_HISTOGRAM_BUCKETS[..]
        };

        let buckets = base_buckets
            .iter()
            .map(|(upper_bound, upper_bound_str)| (*upper_bound, *upper_bound_str, 0))
            .collect();

        Self {
            sum: 0.0,
            count: 0,
            buckets,
        }
    }

    /// Merges another aggregated histogram into this one.
    ///
    /// The two histograms must have been constructed with the same bucket schedule (in practice,
    /// this means they were constructed for the same metric name).
    pub fn merge(&mut self, other: &AggregatedHistogram) {
        self.sum += other.sum;
        self.count += other.count;
        for (dst, src) in self.buckets.iter_mut().zip(other.buckets.iter()) {
            dst.2 += src.2;
        }
    }

    /// Folds the samples from `histogram` into this aggregated histogram.
    pub fn merge_histogram(&mut self, histogram: &Histogram) {
        for sample in histogram.samples() {
            self.add_sample(sample.value.into_inner(), sample.weight.0 as u64);
        }
    }

    fn add_sample(&mut self, value: f64, weight: u64) {
        self.sum += value * weight as f64;
        self.count += weight;

        for (upper_bound, _, count) in &mut self.buckets {
            if value <= *upper_bound {
                *count += weight;
            }
        }
    }

    /// Returns the running sum of all observed samples.
    pub fn sum(&self) -> f64 {
        self.sum
    }

    /// Returns the running count of all observed samples.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Returns an iterator over the buckets as `(upper_bound_str, cumulative_count)` pairs, in
    /// ascending order by upper bound.
    pub fn buckets(&self) -> impl Iterator<Item = (&'static str, u64)> + '_ {
        self.buckets
            .iter()
            .map(|(_, upper_bound_str, count)| (*upper_bound_str, *count))
    }
}

/// Generates a set of `N` log-linear histogram buckets.
///
/// The n-th pair of buckets is `(i, j)`, where `i = base * scale^n` and `j` is the midpoint between
/// `i` and `base * scale^(n+1)`. For example, with `base=2` and `scale=4`, the sequence is `2, 5, 8,
/// 20, 32, 80, 128, 320, 512, ...`.
fn histogram_buckets<const N: usize>(base: f64, scale: f64) -> [(f64, &'static str); N] {
    let mut buckets = [(0.0, ""); N];

    let log_linear_buckets = std::iter::repeat(base).enumerate().flat_map(|(i, base)| {
        let pow = scale.powf(i as f64);
        let value = base * pow;

        let next_pow = scale.powf((i + 1) as f64);
        let next_value = base * next_pow;
        let midpoint = (value + next_value) / 2.0;

        [value, midpoint]
    });

    for (i, current_le) in log_linear_buckets.enumerate().take(N) {
        let (bucket_le, bucket_le_str) = &mut buckets[i];
        let current_le_str = format!("{}", current_le);

        *bucket_le = current_le;
        *bucket_le_str = current_le_str.leak();
    }

    buckets
}
