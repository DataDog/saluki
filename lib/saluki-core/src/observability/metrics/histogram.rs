//! Fixed-bucket histogram aggregation for Prometheus exposition.

use std::sync::LazyLock;

use crate::data_model::event::metric::Histogram;

const HISTOGRAM_BUCKET_COUNT: usize = 30;
static TIME_HISTOGRAM_BUCKETS: LazyLock<[(f64, &'static str); HISTOGRAM_BUCKET_COUNT]> =
    LazyLock::new(|| histogram_buckets::<HISTOGRAM_BUCKET_COUNT>(0.000000128, 4.0));
static NON_TIME_HISTOGRAM_BUCKETS: LazyLock<[(f64, &'static str); HISTOGRAM_BUCKET_COUNT]> =
    LazyLock::new(|| histogram_buckets::<HISTOGRAM_BUCKET_COUNT>(1.0, 2.0));

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_log_linear_bucket_schedule() {
        // Ported directly from the worked example in `histogram_buckets`'s own doc comment: with
        // base=2 and scale=4, the n-th pair is `(base*scale^n, midpoint(base*scale^n, base*scale^(n+1)))`,
        // producing the flattened sequence 2, 5, 8, 20, 32, 80, 128, 320, 512.
        let buckets = histogram_buckets::<9>(2.0, 4.0);
        let bounds: Vec<f64> = buckets.iter().map(|(bound, _)| *bound).collect();
        assert_eq!(bounds, vec![2.0, 5.0, 8.0, 20.0, 32.0, 80.0, 128.0, 320.0, 512.0]);

        // The string labels are the plain `Display` form of each upper bound.
        let labels: Vec<&str> = buckets.iter().map(|(_, label)| *label).collect();
        assert_eq!(labels, vec!["2", "5", "8", "20", "32", "80", "128", "320", "512"]);

        // The generic (non-time) schedule uses base=1, scale=2.
        let non_time = histogram_buckets::<6>(1.0, 2.0);
        let non_time_bounds: Vec<f64> = non_time.iter().map(|(bound, _)| *bound).collect();
        assert_eq!(non_time_bounds, vec![1.0, 1.5, 2.0, 3.0, 4.0, 6.0]);
    }

    #[test]
    fn metric_name_suffix_selects_the_bucket_schedule() {
        // A `_seconds` suffix selects the time-oriented schedule; everything else uses the generic
        // non-time schedule, which starts at an upper bound of 1.0.
        let time = AggregatedHistogram::new("request_latency_seconds");
        let non_time = AggregatedHistogram::new("request_count");

        let time_first = time.buckets().next().expect("time schedule has buckets").0;
        let non_time_first = non_time.buckets().next().expect("non-time schedule has buckets").0;

        assert_eq!(
            non_time_first, "1",
            "the non-time schedule starts at an upper bound of 1.0"
        );
        assert_ne!(
            time_first, non_time_first,
            "the `_seconds` suffix must select a different, time-oriented bucket schedule"
        );
    }

    #[test]
    fn adds_weighted_samples_into_cumulative_buckets() {
        // Using the non-time schedule (1, 1.5, 2, 3, 4, ...), a single value of 2.0 with weight 3 adds
        // to the sum/count by its weight and increments every cumulative "le" bucket whose upper bound
        // is >= 2.0.
        let mut histogram = AggregatedHistogram::new("queue_depth");
        histogram.add_sample(2.0, 3);

        assert_eq!(histogram.count(), 3);
        assert_eq!(histogram.sum(), 6.0);

        let buckets: Vec<(&str, u64)> = histogram.buckets().collect();
        assert_eq!(buckets[0], ("1", 0));
        assert_eq!(buckets[1], ("1.5", 0));
        assert_eq!(buckets[2], ("2", 3));
        assert_eq!(buckets[3], ("3", 3));
    }

    #[test]
    fn merge_sums_counts_sum_and_per_bucket_counts() {
        let mut left = AggregatedHistogram::new("queue_depth");
        left.add_sample(2.0, 1);

        let mut right = AggregatedHistogram::new("queue_depth");
        right.add_sample(4.0, 2);

        left.merge(&right);

        // Sum is weighted (2.0*1 + 4.0*2), count is total weight (1 + 2).
        assert_eq!(left.count(), 3);
        assert_eq!(left.sum(), 10.0);

        let buckets: Vec<(&str, u64)> = left.buckets().collect();
        // The `2` bucket only counts the value 2.0 (weight 1); the `4` bucket counts both samples.
        assert_eq!(buckets[2], ("2", 1));
        assert_eq!(buckets[4], ("4", 3));
    }
}
