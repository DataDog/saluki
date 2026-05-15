use std::{fmt, num::NonZeroU64};

use ordered_float::OrderedFloat;
use smallvec::SmallVec;

use super::{SampleRate, TimestampedValue, TimestampedValues};

/// A weighted sample.
///
/// A weighted sample contains a value and a weight. The weight corresponds to the sample rate of the value, where the
/// value can be considered to be responsible for `weight` samples overall. For example, if a metric was emitted 10
/// times with the same value, you could instead emit it once with a sample rate of 10%, which would be equivalent to a
/// weight of 10 (1/0.1).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WeightedSample {
    /// The sample value.
    pub value: OrderedFloat<f64>,

    /// The sample weight.
    pub weight: OrderedFloat<f64>,
}

/// A basic histogram.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Histogram {
    samples: SmallVec<[WeightedSample; 3]>,
    /// Weight shared by every sample so far; `0.0` means no samples have been inserted yet.
    shared_weight: OrderedFloat<f64>,
    /// Set to `true` on the first insertion whose weight differs from [`shared_weight`].
    weights_vary: bool,
}

impl Histogram {
    /// Insert a sample into the histogram.
    pub fn insert(&mut self, value: f64, sample_rate: SampleRate) {
        let weight = OrderedFloat(sample_rate.raw_weight());
        if self.shared_weight == OrderedFloat(0.0) {
            self.shared_weight = weight;
        } else if weight != self.shared_weight {
            self.weights_vary = true;
        }
        self.samples.push(WeightedSample {
            value: OrderedFloat(value),
            weight,
        });
    }

    /// Returns a summary view over the histogram.
    ///
    /// The summary view contains basic aggregates (sum, count, min, max, etc) and allows querying the histogram for
    /// various quantiles.
    pub fn summary_view(&mut self) -> HistogramSummary<'_> {
        // Sort the samples by their value, lowest to highest.
        //
        // This is required as we depend on it as an invariant in `HistogramSummary` to quickly answer aggregates like
        // minimum and maximum, as well as quantile queries.
        self.samples.sort_unstable_by_key(|sample| sample.value);

        // Compute count and sum in a single pass using compensated (Neumaier) summation.
        // Four accumulators: (count_s, count_c) for the weight total and (sum_s, sum_c) for the
        // value total, keeping one correction term per accumulator.
        let (count, sum) = if self.weights_vary {
            // Varying weights: accumulate value * weight for sum, weight for count.
            let (cs, cc, ss, sc) =
                self.samples
                    .iter()
                    .fold((0.0_f64, 0.0_f64, 0.0_f64, 0.0_f64), |(cs, cc, ss, sc), sample| {
                        let (cs, cc) = neumaier_add(cs, cc, sample.weight.0);
                        let (ss, sc) = neumaier_add(ss, sc, sample.value.0 * sample.weight.0);
                        (cs, cc, ss, sc)
                    });
            (cs + cc, ss + sc)
        } else {
            // Uniform weights: accumulate raw values for sum (scaled once at the end), weight for count.
            let (cs, cc, ss, sc) =
                self.samples
                    .iter()
                    .fold((0.0_f64, 0.0_f64, 0.0_f64, 0.0_f64), |(cs, cc, ss, sc), sample| {
                        let (cs, cc) = neumaier_add(cs, cc, sample.weight.0);
                        let (ss, sc) = neumaier_add(ss, sc, sample.value.0);
                        (cs, cc, ss, sc)
                    });
            (cs + cc, (ss + sc) * self.shared_weight.0)
        };

        HistogramSummary {
            histogram: self,
            count,
            sum,
        }
    }

    /// Merges another histogram into this one.
    pub fn merge(&mut self, other: &mut Histogram) {
        if !self.weights_vary {
            if other.weights_vary {
                self.weights_vary = true;
            } else if self.shared_weight == OrderedFloat(0.0) {
                self.shared_weight = other.shared_weight;
            } else if other.shared_weight != OrderedFloat(0.0) && self.shared_weight != other.shared_weight {
                self.weights_vary = true;
            }
        }
        self.samples.extend(other.samples.drain(..));
    }

    /// Returns the raw weighted samples of the histogram.
    pub fn samples(&self) -> &[WeightedSample] {
        &self.samples
    }
}

/// Summary view over a [`Histogram`].
pub struct HistogramSummary<'a> {
    histogram: &'a Histogram,
    count: f64,
    sum: f64,
}

impl HistogramSummary<'_> {
    /// Returns the number of samples in the histogram.
    ///
    /// This is adjusted by the weight of each sample, based on the sample rate given during insertion.
    ///
    /// The underlying weight accumulation uses compensated (Neumaier) summation over float weights,
    /// and the result is rounded to the nearest integer. For standard sample rates whose reciprocals
    /// are exact integers (e.g. `0.1`, `0.25`, `0.5`, `1.0`) rounding has no effect; for
    /// non-integer-reciprocal rates (e.g. `0.21` → weight ≈ 4.762) it gives a closer approximation
    /// than truncation.
    pub fn count(&self) -> u64 {
        self.count.round() as u64
    }

    /// Returns the sum of all samples in the histogram.
    ///
    /// This is adjusted by the weight of each sample, based on the sample rate given during insertion.
    pub fn sum(&self) -> f64 {
        self.sum
    }

    /// Returns the minimum value in the histogram.
    ///
    /// If the histogram is empty, `None` will be returned.
    pub fn min(&self) -> Option<f64> {
        // NOTE: Samples are sorted before `HistogramSummary` is created, so we know that the first value is the minimum.
        self.histogram.samples.first().map(|sample| sample.value.0)
    }

    /// Returns the maximum value in the histogram.
    ///
    /// If the histogram is empty, `None` will be returned.
    pub fn max(&self) -> Option<f64> {
        // NOTE: Samples are sorted before `HistogramSummary` is created, so we know that the last value is the maximum.
        self.histogram.samples.last().map(|sample| sample.value.0)
    }

    /// Returns the average value in the histogram.
    pub fn avg(&self) -> f64 {
        self.sum / self.count
    }

    /// Returns the median value in the histogram.
    ///
    /// If the histogram is empty, `None` will be returned.
    pub fn median(&self) -> Option<f64> {
        self.quantile(0.5)
    }

    /// Returns the given quantile in the histogram.
    ///
    /// If the histogram is empty, or if `quantile` is less than 0.0 or greater than 1.0, `None` will be returned.
    pub fn quantile(&self, quantile: f64) -> Option<f64> {
        if !(0.0..=1.0).contains(&quantile) {
            return None;
        }

        // target is the cumulative weight threshold: walk samples until the running weight exceeds it.
        let target = quantile * self.count - 0.01;

        let mut ws = 0.0_f64;
        let mut wc = 0.0_f64;
        for sample in &self.histogram.samples {
            (ws, wc) = neumaier_add(ws, wc, sample.weight.0);
            if ws + wc > target {
                return Some(sample.value.0);
            }
        }

        None
    }
}

/// A set of histogram points.
///
/// Used to represent the data points of histograms. Each data point is attached to an optional timestamp.
///
/// Histograms are conceptually similar to sketches, but hold all raw samples directly and thus have a slightly worse
/// memory efficiency as the number of samples grows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistogramPoints(TimestampedValues<Histogram, 1>);

impl HistogramPoints {
    pub(super) fn inner(&self) -> &TimestampedValues<Histogram, 1> {
        &self.0
    }

    pub(super) fn inner_mut(&mut self) -> &mut TimestampedValues<Histogram, 1> {
        &mut self.0
    }

    pub(super) fn drain_timestamped(&mut self) -> Self {
        Self(self.0.drain_timestamped())
    }

    pub(super) fn split_at_timestamp(&mut self, timestamp: u64) -> Option<Self> {
        self.0.split_at_timestamp(timestamp).map(Self)
    }

    /// Returns `true` if this set is empty.
    pub fn is_empty(&self) -> bool {
        self.0.values.is_empty()
    }

    /// Returns the number of points in this set.
    pub fn len(&self) -> usize {
        self.0.values.len()
    }

    /// Merges another set of points into this one.
    ///
    /// If a point with the same timestamp exists in both sets, the histograms will be merged together. Otherwise, the
    /// points will appended to the end of the set.
    pub fn merge(&mut self, other: Self) {
        let mut needs_sort = false;
        for mut other_value in other.0.values {
            if let Some(existing_value) = self
                .0
                .values
                .iter_mut()
                .find(|value| value.timestamp == other_value.timestamp)
            {
                existing_value.value.merge(&mut other_value.value);
            } else {
                self.0.values.push(other_value);
                needs_sort = true;
            }
        }

        if needs_sort {
            self.0.sort_by_timestamp();
        }
    }
}

impl From<Histogram> for HistogramPoints {
    fn from(value: Histogram) -> Self {
        Self(TimestampedValue::from(value).into())
    }
}

impl From<f64> for HistogramPoints {
    fn from(value: f64) -> Self {
        let mut histogram = Histogram::default();
        histogram.insert(value, SampleRate::unsampled());

        Self(TimestampedValue::from(histogram).into())
    }
}

impl From<(u64, f64)> for HistogramPoints {
    fn from((ts, value): (u64, f64)) -> Self {
        let mut histogram = Histogram::default();
        histogram.insert(value, SampleRate::unsampled());

        Self(TimestampedValue::from((ts, histogram)).into())
    }
}

impl<const N: usize> From<[f64; N]> for HistogramPoints {
    fn from(values: [f64; N]) -> Self {
        let mut histogram = Histogram::default();
        for value in values {
            histogram.insert(value, SampleRate::unsampled());
        }

        Self(TimestampedValue::from(histogram).into())
    }
}

impl<const N: usize> From<(u64, [f64; N])> for HistogramPoints {
    fn from((ts, values): (u64, [f64; N])) -> Self {
        let mut histogram = Histogram::default();
        for value in values {
            histogram.insert(value, SampleRate::unsampled());
        }

        Self(TimestampedValue::from((ts, histogram)).into())
    }
}

impl<const N: usize> From<[(u64, f64); N]> for HistogramPoints {
    fn from(values: [(u64, f64); N]) -> Self {
        Self(
            values
                .into_iter()
                .map(|(ts, value)| {
                    let mut histogram = Histogram::default();
                    histogram.insert(value, SampleRate::unsampled());

                    (ts, histogram)
                })
                .into(),
        )
    }
}

impl<'a> From<&'a [f64]> for HistogramPoints {
    fn from(values: &'a [f64]) -> Self {
        let mut histogram = Histogram::default();
        for value in values {
            histogram.insert(*value, SampleRate::unsampled());
        }

        Self(TimestampedValue::from(histogram).into())
    }
}

impl<'a, const N: usize> From<&'a [f64; N]> for HistogramPoints {
    fn from(values: &'a [f64; N]) -> Self {
        let mut histogram = Histogram::default();
        for value in values {
            histogram.insert(*value, SampleRate::unsampled());
        }

        Self(TimestampedValue::from(histogram).into())
    }
}

impl IntoIterator for HistogramPoints {
    type Item = (Option<NonZeroU64>, Histogram);
    type IntoIter = HistogramIter;

    fn into_iter(self) -> Self::IntoIter {
        HistogramIter {
            inner: self.0.values.into_iter(),
        }
    }
}

impl<'a> IntoIterator for &'a HistogramPoints {
    type Item = (Option<NonZeroU64>, &'a Histogram);
    type IntoIter = HistogramIterRef<'a>;

    fn into_iter(self) -> Self::IntoIter {
        HistogramIterRef {
            inner: self.0.values.iter(),
        }
    }
}

impl<'a> IntoIterator for &'a mut HistogramPoints {
    type Item = (Option<NonZeroU64>, &'a mut Histogram);
    type IntoIter = HistogramIterRefMut<'a>;

    fn into_iter(self) -> Self::IntoIter {
        HistogramIterRefMut {
            inner: self.0.values.iter_mut(),
        }
    }
}

impl fmt::Display for HistogramPoints {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[")?;
        for (i, point) in self.0.values.iter().enumerate() {
            if i > 0 {
                write!(f, ",")?;
            }

            let ts = point.timestamp.map(|ts| ts.get()).unwrap_or_default();
            write!(f, "({}, [", ts)?;
            for (j, sample) in point.value.samples().iter().enumerate() {
                if j > 0 {
                    write!(f, ",")?;
                }
                write!(f, "{{{} * {}}}", sample.value, sample.weight)?;
            }
            write!(f, "])")?;
        }
        write!(f, "]")
    }
}

pub struct HistogramIter {
    inner: smallvec::IntoIter<[TimestampedValue<Histogram>; 1]>,
}

impl Iterator for HistogramIter {
    type Item = (Option<NonZeroU64>, Histogram);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|value| (value.timestamp, value.value))
    }
}

pub struct HistogramIterRef<'a> {
    inner: std::slice::Iter<'a, TimestampedValue<Histogram>>,
}

impl<'a> Iterator for HistogramIterRef<'a> {
    type Item = (Option<NonZeroU64>, &'a Histogram);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|value| (value.timestamp, &value.value))
    }
}

pub struct HistogramIterRefMut<'a> {
    inner: std::slice::IterMut<'a, TimestampedValue<Histogram>>,
}

impl<'a> Iterator for HistogramIterRefMut<'a> {
    type Item = (Option<NonZeroU64>, &'a mut Histogram);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|value| (value.timestamp, &mut value.value))
    }
}

/// Performs a single Neumaier (compensated) addition step.
///
/// Returns the updated running sum `t` and the compensation term `c` that captures
/// the rounding error lost when adding `x` to `s`.
fn neumaier_add(s: f64, c: f64, x: f64) -> (f64, f64) {
    let t = s + x;
    let c = if s.abs() >= x.abs() {
        c + ((s - t) + x)
    } else {
        c + ((x - t) + s)
    };
    (t, c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn histogram_from_values(values: &[(f64, u64)]) -> Histogram {
        let mut h = Histogram::default();
        for &(value, weight) in values {
            let weight = OrderedFloat(weight as f64);
            if h.shared_weight == OrderedFloat(0.0) {
                h.shared_weight = weight;
            } else if weight != h.shared_weight {
                h.weights_vary = true;
            }
            h.samples.push(WeightedSample {
                value: OrderedFloat(value),
                weight,
            });
        }
        h
    }

    #[test]
    fn compensated_sum_catastrophic_cancellation() {
        // Naive summation: (1 + 1e100) + (1 - 1e100) = 0 due to float cancellation.
        // Compensated summation must return 2.0.
        let mut h = histogram_from_values(&[(1.0, 1), (1e100, 1), (1.0, 1), (-1e100, 1)]);
        let view = h.summary_view();
        assert_eq!(view.sum(), 2.0, "compensated sum should be 2.0, not 0.0");
    }

    #[test]
    fn compensated_sum_empty() {
        let mut h = Histogram::default();
        let view = h.summary_view();
        assert_eq!(view.sum(), 0.0);
        assert_eq!(view.count(), 0);
    }

    #[test]
    fn compensated_sum_uniform_weights_positives() {
        let mut h = histogram_from_values(&[(1.0, 2), (2.0, 2), (3.0, 2)]);
        let view = h.summary_view();
        // sum = (1+2+3)*2 = 12, count = 6
        assert_eq!(view.sum(), 12.0);
        assert_eq!(view.count(), 6);
    }

    #[test]
    fn compensated_sum_uniform_weights_negatives() {
        let mut h = histogram_from_values(&[(-3.0, 1), (-2.0, 1), (-1.0, 1)]);
        let view = h.summary_view();
        assert_eq!(view.sum(), -6.0);
        assert_eq!(view.count(), 3);
    }

    #[test]
    fn compensated_sum_varying_weights() {
        // Different weights trigger the fallback Neumaier path.
        let mut h = histogram_from_values(&[(1.0, 1), (2.0, 2), (3.0, 4)]);
        let view = h.summary_view();
        // sum = 1*1 + 2*2 + 3*4 = 1 + 4 + 12 = 17, count = 7
        assert_eq!(view.sum(), 17.0);
        assert_eq!(view.count(), 7);
    }

    #[test]
    fn compensated_sum_all_zeros() {
        let mut h = histogram_from_values(&[(0.0, 1), (0.0, 1), (0.0, 1)]);
        let view = h.summary_view();
        assert_eq!(view.sum(), 0.0);
        assert_eq!(view.count(), 3);
    }

    #[test]
    fn compensated_sum_single_value() {
        let mut h = histogram_from_values(&[(42.0, 5)]);
        let view = h.summary_view();
        assert_eq!(view.sum(), 210.0);
        assert_eq!(view.count(), 5);
    }
}
