use std::num::NonZeroU64;

use ordered_float::OrderedFloat;
use smallvec::SmallVec;

use super::{TimestampedValue, TimestampedValues};
use crate::metric::SampleRate;

#[derive(Clone, Debug, Eq, PartialEq)]
struct WeightedSample {
    value: OrderedFloat<f64>,
    weight: u64,
}

/// A basic histogram.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Histogram {
    sum: OrderedFloat<f64>,
    samples: SmallVec<[WeightedSample; 3]>,
}

impl Histogram {
    /// Insert a sample into the histogram.
    pub fn insert(&mut self, value: f64, sample_rate: SampleRate) {
        self.sum += value * sample_rate.raw_weight();
        self.samples.push(WeightedSample {
            value: OrderedFloat(value),
            weight: sample_rate.weight(),
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

        let mut count = 0;
        let mut sum = 0.0;
        for sample in &self.samples {
            count += sample.weight;
            sum += sample.value.0 * sample.weight as f64;
        }

        HistogramSummary {
            histogram: self,
            count,
            sum,
        }
    }

    /// Merges another histogram into this one.
    pub fn merge(&mut self, other: &mut Histogram) {
        self.sum += other.sum;
        self.samples.extend(other.samples.drain(..));
    }
}

/// Summary view over a [`Histogram`].
pub struct HistogramSummary<'a> {
    histogram: &'a Histogram,
    count: u64,
    sum: f64,
}

impl<'a> HistogramSummary<'a> {
    /// Returns the number of samples in the histogram.
    ///
    /// This is adjusted by the weight of each sample, based on the sample rate given during insertion.
    pub fn count(&self) -> u64 {
        self.count
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
        self.sum / self.count as f64
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

        let scaled_quantile = (quantile * 1000.0) as u64 / 10;
        let target = (scaled_quantile * self.count - 1) / 100;

        let mut weight = 0;
        for sample in &self.histogram.samples {
            weight += sample.weight;
            if weight > target {
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

impl<const N: usize> From<[f64; N]> for HistogramPoints {
    fn from(values: [f64; N]) -> Self {
        let mut histogram = Histogram::default();
        for value in values {
            histogram.insert(value, SampleRate::unsampled());
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

impl<'a> IntoIterator for &'a mut HistogramPoints {
    type Item = (Option<NonZeroU64>, &'a mut Histogram);
    type IntoIter = HistogramIterRefMut<'a>;

    fn into_iter(self) -> Self::IntoIter {
        HistogramIterRefMut {
            inner: self.0.values.iter_mut(),
        }
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

pub struct HistogramIterRefMut<'a> {
    inner: std::slice::IterMut<'a, TimestampedValue<Histogram>>,
}

impl<'a> Iterator for HistogramIterRefMut<'a> {
    type Item = (Option<NonZeroU64>, &'a mut Histogram);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|value| (value.timestamp, &mut value.value))
    }
}
