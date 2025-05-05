mod iter;

use std::{collections::HashSet, fmt, num::NonZeroU64, time::Duration};

use ddsketch_agent::DDSketch;
use ordered_float::OrderedFloat;
use smallvec::SmallVec;

mod sketch;
pub use self::sketch::SketchPoints;

mod histogram;
pub use self::histogram::{Histogram, HistogramPoints, HistogramSummary};

mod scalar;
pub use self::scalar::ScalarPoints;

mod set;
pub use self::set::SetPoints;
use super::SampleRate;

#[derive(Clone, Debug, Eq, PartialEq)]
struct TimestampedValue<T> {
    timestamp: Option<NonZeroU64>,
    value: T,
}

impl<T> From<T> for TimestampedValue<T> {
    fn from(value: T) -> Self {
        Self { timestamp: None, value }
    }
}

impl<T> From<(u64, T)> for TimestampedValue<T> {
    fn from((timestamp, value): (u64, T)) -> Self {
        Self {
            timestamp: NonZeroU64::new(timestamp),
            value,
        }
    }
}

impl From<(Option<NonZeroU64>, f64)> for TimestampedValue<OrderedFloat<f64>> {
    fn from((timestamp, value): (Option<NonZeroU64>, f64)) -> Self {
        Self {
            timestamp,
            value: OrderedFloat(value),
        }
    }
}

impl<T> From<(Option<NonZeroU64>, T)> for TimestampedValue<T> {
    fn from((timestamp, value): (Option<NonZeroU64>, T)) -> Self {
        Self { timestamp, value }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TimestampedValues<T, const N: usize> {
    values: SmallVec<[TimestampedValue<T>; N]>,
}

impl<T, const N: usize> TimestampedValues<T, N> {
    fn all_timestamped(&self) -> bool {
        self.values.iter().all(|value| value.timestamp.is_some())
    }

    fn any_timestamped(&self) -> bool {
        self.values.iter().any(|value| value.timestamp.is_some())
    }

    fn drain_timestamped(&mut self) -> Self {
        Self {
            values: self.values.drain_filter(|value| value.timestamp.is_some()).collect(),
        }
    }

    fn sort_by_timestamp(&mut self) {
        self.values.sort_by_key(|value| value.timestamp);
    }

    fn split_at_timestamp(&mut self, timestamp: u64) -> Option<Self> {
        if self.values.is_empty() {
            return None;
        }

        // Fast path: since all values are sorted, we know that if the first value's timestamp is set, and greater than
        // the split timestamp, nothing that comes after it could be split off either.
        if let Some(first) = self.values.first() {
            if let Some(first_ts) = first.timestamp {
                if first_ts.get() > timestamp {
                    return None;
                }
            }
        }

        let new_values = self
            .values
            .drain_filter(|value| value.timestamp.is_some_and(|ts| ts.get() <= timestamp));
        Some(Self::from(new_values))
    }

    fn set_timestamp(&mut self, timestamp: u64) {
        let timestamp = NonZeroU64::new(timestamp);
        for value in &mut self.values {
            value.timestamp = timestamp;
        }
    }

    fn collapse_non_timestamped<F>(&mut self, timestamp: u64, merge: F)
    where
        F: Fn(&mut T, &mut T),
    {
        self.values.dedup_by(|a, b| {
            if a.timestamp.is_none() && b.timestamp.is_none() {
                merge(&mut b.value, &mut a.value);
                true
            } else {
                false
            }
        });

        // Since all values are ordered by timestamp, with non-timestamped values ordered first, we know that if there
        // were any non-timestamped values that got collapsed, then the single remaining non-timestamped value will be
        // the first value in the set.
        if let Some(first) = self.values.first_mut() {
            first.timestamp = first.timestamp.or(NonZeroU64::new(timestamp));
        }
    }
}

impl<T, const N: usize> Default for TimestampedValues<T, N> {
    fn default() -> Self {
        Self {
            values: SmallVec::new(),
        }
    }
}

impl<T, const N: usize> From<TimestampedValue<T>> for TimestampedValues<T, N> {
    fn from(value: TimestampedValue<T>) -> Self {
        let mut values = SmallVec::new();
        values.push(value);

        Self { values }
    }
}

impl<I, IT, T, const N: usize> From<I> for TimestampedValues<T, N>
where
    I: IntoIterator<Item = IT>,
    IT: Into<TimestampedValue<T>>,
{
    fn from(value: I) -> Self {
        let mut values = SmallVec::new();
        values.extend(value.into_iter().map(Into::into));

        let mut values = Self { values };
        values.sort_by_timestamp();

        values
    }
}

/// The values of a metric.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetricValues {
    /// A counter.
    ///
    /// Counters generally represent a monotonically increasing value, such as the number of requests received.
    Counter(ScalarPoints),

    /// A rate.
    ///
    /// Rates define the rate of change over a given interval.
    ///
    /// For example, a rate with a value of 15 and an interval of 10 seconds would indicate that the value increases by
    /// 15 every 10 seconds, or 1.5 per second.
    Rate(ScalarPoints, Duration),

    /// A gauge.
    ///
    /// Gauges represent the latest value of a quantity, such as the current number of active connections. This value
    /// can go up or down, but gauges do not track the individual changes to the value, only the latest value.
    Gauge(ScalarPoints),

    /// A set.
    ///
    /// Sets represent a collection of unique values, such as the unique IP addresses that have connected to a service.
    Set(SetPoints),

    /// A histogram.
    ///
    /// Histograms represent the distribution of a quantity, such as the response times for a service, with forced
    /// client-side aggregation. Individual samples are stored locally, in full fidelity, and aggregate statistics
    /// can be queried against the sample set, but the individual samples cannot be accessed.
    Histogram(HistogramPoints),

    /// A distribution.
    ///
    /// Distributions represent the distribution of a quantity, such as the response times for a service, in such a way
    /// that server-side aggregation is possible. Individual samples are stored in a sketch, which supports being merged
    /// with other sketches server-side to facilitate global aggregation.
    ///
    /// Like histograms, sketches also provide the ability to be queried for aggregate statistics but the individual
    /// samples cannot be accessed.
    Distribution(SketchPoints),
}

impl MetricValues {
    /// Creates a set of counter values from the given value(s).
    pub fn counter<V>(values: V) -> Self
    where
        V: Into<ScalarPoints>,
    {
        Self::Counter(values.into())
    }

    /// Creates a set of counter values from a fallible iterator of values, based on the given sample rate.
    ///
    /// If `sample_rate` is `None`, no values will be modified. Otherwise, each value will be scaled proportionally to
    /// the quotient of `1 / sample_rate`.
    pub fn counter_sampled_fallible<I, E>(iter: I, sample_rate: Option<SampleRate>) -> Result<Self, E>
    where
        I: Iterator<Item = Result<f64, E>>,
    {
        let sample_rate = sample_rate.unwrap_or(SampleRate::unsampled());

        let mut points = ScalarPoints::new();
        for value in iter {
            let value = value?;
            points.add_point(None, value * sample_rate.raw_weight());
        }
        Ok(Self::Counter(points))
    }

    /// Creates a set of gauge values from the given value(s).
    pub fn gauge<V>(values: V) -> Self
    where
        V: Into<ScalarPoints>,
    {
        Self::Gauge(values.into())
    }

    /// Creates a set of gauge values from a fallible iterator of values.
    pub fn gauge_fallible<I, E>(iter: I) -> Result<Self, E>
    where
        I: Iterator<Item = Result<f64, E>>,
    {
        let mut points = ScalarPoints::new();
        for value in iter {
            points.add_point(None, value?);
        }
        Ok(Self::Gauge(points))
    }

    /// Creates a set from the given values.
    pub fn set<V>(values: V) -> Self
    where
        V: Into<SetPoints>,
    {
        Self::Set(values.into())
    }

    /// Creates a set of histogram values from the given value(s).
    pub fn histogram<V>(values: V) -> Self
    where
        V: Into<HistogramPoints>,
    {
        Self::Histogram(values.into())
    }

    /// Creates a set of histogram values from a fallible iterator of values, based on the given sample rate.
    ///
    /// If `sample_rate` is `None`, only the values present in the iterator will be used. Otherwise, each value will be
    /// inserted at a scaled count of `1 / sample_rate`.
    pub fn histogram_sampled_fallible<I, E>(iter: I, sample_rate: Option<SampleRate>) -> Result<Self, E>
    where
        I: Iterator<Item = Result<f64, E>>,
    {
        let sample_rate = sample_rate.unwrap_or(SampleRate::unsampled());

        let mut histogram = Histogram::default();
        for value in iter {
            let value = value?;
            histogram.insert(value, sample_rate);
        }
        Ok(Self::Histogram(histogram.into()))
    }

    /// Creates a set of distribution values from the given value(s).
    pub fn distribution<V>(values: V) -> Self
    where
        V: Into<SketchPoints>,
    {
        Self::Distribution(values.into())
    }

    /// Creates a set of distribution values from a fallible iterator of values, based on the given sample rate.
    ///
    /// If `sample_rate` is `None`, only the values present in the iterator will be used. Otherwise, each value will be
    /// inserted at a scaled count of `1 / sample_rate`.
    pub fn distribution_sampled_fallible<I, E>(iter: I, sample_rate: Option<SampleRate>) -> Result<Self, E>
    where
        I: Iterator<Item = Result<f64, E>>,
    {
        let sample_rate = sample_rate.unwrap_or(SampleRate::unsampled());
        let capped_sample_rate = u32::try_from(sample_rate.weight()).unwrap_or(u32::MAX);

        let mut sketch = DDSketch::default();
        for value in iter {
            let value = value?;
            if capped_sample_rate == 1 {
                sketch.insert(value);
            } else {
                sketch.insert_n(value, capped_sample_rate);
            }
        }
        Ok(Self::Distribution(sketch.into()))
    }

    /// Creates a set of rate values from the given value(s) and interval.
    pub fn rate<V>(values: V, interval: Duration) -> Self
    where
        V: Into<ScalarPoints>,
    {
        Self::Rate(values.into(), interval)
    }

    /// Returns `true` if this metric has no values.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Counter(points) | Self::Rate(points, _) | Self::Gauge(points) => points.is_empty(),
            Self::Set(points) => points.is_empty(),
            Self::Histogram(points) => points.is_empty(),
            Self::Distribution(points) => points.is_empty(),
        }
    }

    /// Returns the number of values in this metric.
    pub fn len(&self) -> usize {
        match self {
            Self::Counter(points) | Self::Rate(points, _) | Self::Gauge(points) => points.len(),
            Self::Set(points) => points.len(),
            Self::Histogram(points) => points.len(),
            Self::Distribution(points) => points.len(),
        }
    }

    /// Returns `true` if all values in this metric have a timestamp.
    pub fn all_timestamped(&self) -> bool {
        match self {
            Self::Counter(points) | Self::Rate(points, _) | Self::Gauge(points) => points.inner().all_timestamped(),
            Self::Set(points) => points.inner().all_timestamped(),
            Self::Histogram(points) => points.inner().all_timestamped(),
            Self::Distribution(points) => points.inner().all_timestamped(),
        }
    }

    /// Returns `true` if any values in this metric have a timestamp.
    pub fn any_timestamped(&self) -> bool {
        match self {
            Self::Counter(points) | Self::Rate(points, _) | Self::Gauge(points) => points.inner().any_timestamped(),
            Self::Set(points) => points.inner().any_timestamped(),
            Self::Histogram(points) => points.inner().any_timestamped(),
            Self::Distribution(points) => points.inner().any_timestamped(),
        }
    }

    /// Sets the timestamp for all values in this metric.
    ///
    /// This overrides all existing timestamps whether they are set or not. If `timestamp` is zero, all existing
    /// timestamps will be cleared.
    pub fn set_timestamp(&mut self, timestamp: u64) {
        match self {
            Self::Counter(points) | Self::Gauge(points) | Self::Rate(points, _) => {
                points.inner_mut().set_timestamp(timestamp)
            }
            Self::Set(points) => points.inner_mut().set_timestamp(timestamp),
            Self::Histogram(points) => points.inner_mut().set_timestamp(timestamp),
            Self::Distribution(points) => points.inner_mut().set_timestamp(timestamp),
        }
    }

    /// Splits all timestamped values into a new `MetricValues`, leaving the remaining values in `self`.
    pub fn split_timestamped(&mut self) -> Self {
        match self {
            Self::Counter(points) => Self::Counter(points.drain_timestamped()),
            Self::Rate(points, interval) => Self::Rate(points.drain_timestamped(), *interval),
            Self::Gauge(points) => Self::Gauge(points.drain_timestamped()),
            Self::Set(points) => Self::Set(points.drain_timestamped()),
            Self::Histogram(points) => Self::Histogram(points.drain_timestamped()),
            Self::Distribution(points) => Self::Distribution(points.drain_timestamped()),
        }
    }

    /// Splits all values with a timestamp less than or equal to `timestamp` into a new `MetricValues`, leaving the
    /// remaining values in `self`.
    pub fn split_at_timestamp(&mut self, timestamp: u64) -> Option<Self> {
        match self {
            Self::Counter(points) => points.split_at_timestamp(timestamp).map(Self::Counter),
            Self::Rate(points, interval) => points
                .split_at_timestamp(timestamp)
                .map(|points| Self::Rate(points, *interval)),
            Self::Gauge(points) => points.split_at_timestamp(timestamp).map(Self::Gauge),
            Self::Set(points) => points.split_at_timestamp(timestamp).map(Self::Set),
            Self::Histogram(points) => points.split_at_timestamp(timestamp).map(Self::Histogram),
            Self::Distribution(points) => points.split_at_timestamp(timestamp).map(Self::Distribution),
        }
    }

    /// Collapses all non-timestamped values into a single value with the given timestamp.
    ///
    pub fn collapse_non_timestamped(&mut self, timestamp: u64) {
        match self {
            // Collapse by summing.
            Self::Counter(points) => points.inner_mut().collapse_non_timestamped(timestamp, merge_scalar_sum),
            Self::Rate(points, _) => points.inner_mut().collapse_non_timestamped(timestamp, merge_scalar_sum),
            // Collapse by keeping the last value.
            Self::Gauge(points) => points
                .inner_mut()
                .collapse_non_timestamped(timestamp, merge_scalar_latest),
            // Collapse by merging.
            Self::Set(points) => points.inner_mut().collapse_non_timestamped(timestamp, merge_set),
            Self::Histogram(points) => points.inner_mut().collapse_non_timestamped(timestamp, merge_histogram),
            Self::Distribution(sketches) => sketches.inner_mut().collapse_non_timestamped(timestamp, merge_sketch),
        }
    }

    /// Merges another set of metric values into this one.
    ///
    /// If both `self` and `other` are the same metric type, their values will be merged appropriately. If the metric
    /// types are different, or a specific precondition for the metric type is not met, the incoming values will override
    /// the existing values instead.
    ///
    /// For rates, the interval of both rates must match to be merged. For gauges, the incoming value will override the
    /// existing value.
    pub fn merge(&mut self, other: Self) {
        match (self, other) {
            (Self::Counter(a), Self::Counter(b)) => a.merge(b, merge_scalar_sum),
            (Self::Rate(a_points, a_interval), Self::Rate(b_points, b_interval)) => {
                if *a_interval != b_interval {
                    *a_points = b_points;
                    *a_interval = b_interval;
                } else {
                    a_points.merge(b_points, merge_scalar_sum);
                }
            }
            (Self::Gauge(a), Self::Gauge(b)) => a.merge(b, merge_scalar_latest),
            (Self::Set(a), Self::Set(b)) => a.merge(b),
            (Self::Histogram(a), Self::Histogram(b)) => a.merge(b),
            (Self::Distribution(a), Self::Distribution(b)) => a.merge(b),

            // Just override with whatever the incoming value is.
            (dest, src) => drop(std::mem::replace(dest, src)),
        }
    }

    /// Returns the metric value type as a string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Counter(_) => "counter",
            Self::Rate(_, _) => "rate",
            Self::Gauge(_) => "gauge",
            Self::Set(_) => "set",
            Self::Histogram(_) => "histogram",
            Self::Distribution(_) => "distribution",
        }
    }

    /// Returns `true` if this metric is a serie.
    pub fn is_serie(&self) -> bool {
        matches!(
            self,
            Self::Counter(_) | Self::Rate(_, _) | Self::Gauge(_) | Self::Set(_) | Self::Histogram(_)
        )
    }

    /// Returns `true` if this metric is a sketch.
    pub fn is_sketch(&self) -> bool {
        matches!(self, Self::Distribution(_))
    }
}

impl fmt::Display for MetricValues {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Counter(points) => write!(f, "{}", points),
            Self::Rate(points, interval) => write!(f, "{} over {:?}", points, interval),
            Self::Gauge(points) => write!(f, "{}", points),
            Self::Set(points) => write!(f, "{}", points),
            Self::Histogram(points) => write!(f, "{}", points),
            Self::Distribution(points) => write!(f, "{}", points),
        }
    }
}

fn merge_scalar_sum(dest: &mut OrderedFloat<f64>, src: &mut OrderedFloat<f64>) {
    *dest += *src;
}

fn merge_scalar_latest(dest: &mut OrderedFloat<f64>, src: &mut OrderedFloat<f64>) {
    *dest = *src;
}

fn merge_set(dest: &mut HashSet<String>, src: &mut HashSet<String>) {
    dest.extend(src.drain());
}

fn merge_histogram(dest: &mut Histogram, src: &mut Histogram) {
    dest.merge(src);
}

fn merge_sketch(dest: &mut DDSketch, src: &mut DDSketch) {
    dest.merge(src);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{HistogramPoints, MetricValues, SetPoints, SketchPoints};
    use crate::metric::ScalarPoints;

    #[test]
    fn merge_counters() {
        let cases = [
            // Both A and B have single point with an identical timestamp, so the points should be merged.
            (
                ScalarPoints::from((1, 1.0)),
                ScalarPoints::from((1, 2.0)),
                ScalarPoints::from((1, 3.0)),
            ),
            // A has a single point with a timestamp, B has a single point without a timestamp, so both points should be kept.
            (
                ScalarPoints::from((1, 1.0)),
                ScalarPoints::from(2.0),
                ScalarPoints::from([(0, 2.0), (1, 1.0)]),
            ),
            // Both A and B have single point without a timestamp, so the points should be merged.
            (
                ScalarPoints::from(5.0),
                ScalarPoints::from(6.0),
                ScalarPoints::from(11.0),
            ),
        ];

        for (a, b, expected) in cases {
            let mut merged = MetricValues::Counter(a.clone());
            merged.merge(MetricValues::Counter(b.clone()));

            assert_eq!(
                merged,
                MetricValues::Counter(expected.clone()),
                "merged {} with {}, expected {} but got {}",
                a,
                b,
                expected,
                merged
            );
        }
    }

    #[test]
    fn merge_gauges() {
        let cases = [
            // Both A and B have single point with an identical timestamp, so B's point value should override A's point value.
            (
                ScalarPoints::from((1, 1.0)),
                ScalarPoints::from((1, 2.0)),
                ScalarPoints::from((1, 2.0)),
            ),
            // A has a single point with a timestamp, B has a single point without a timestamp, so both points should be kept.
            (
                ScalarPoints::from((1, 1.0)),
                ScalarPoints::from(2.0),
                ScalarPoints::from([(0, 2.0), (1, 1.0)]),
            ),
            // Both A and B have single point without a timestamp, so B's point value should override A's point value.
            (
                ScalarPoints::from(5.0),
                ScalarPoints::from(6.0),
                ScalarPoints::from(6.0),
            ),
        ];

        for (a, b, expected) in cases {
            let mut merged = MetricValues::Gauge(a.clone());
            merged.merge(MetricValues::Gauge(b.clone()));

            assert_eq!(
                merged,
                MetricValues::Gauge(expected.clone()),
                "merged {} with {}, expected {} but got {}",
                a,
                b,
                expected,
                merged
            );
        }
    }

    #[test]
    fn merge_rates() {
        const FIVE_SECS: Duration = Duration::from_secs(5);
        const TEN_SECS: Duration = Duration::from_secs(10);

        let cases = [
            // Both A and B have single point with an identical timestamp, and identical intervals, so the points should be merged.
            (
                ScalarPoints::from((1, 1.0)),
                FIVE_SECS,
                ScalarPoints::from((1, 2.0)),
                FIVE_SECS,
                ScalarPoints::from((1, 3.0)),
                FIVE_SECS,
            ),
            // A has a single point with a timestamp, B has a single point without a timestamp, and identical intervals, so both points should be kept.
            (
                ScalarPoints::from((1, 1.0)),
                FIVE_SECS,
                ScalarPoints::from(2.0),
                FIVE_SECS,
                ScalarPoints::from([(0, 2.0), (1, 1.0)]),
                FIVE_SECS,
            ),
            // Both A and B have single point without a timestamp, and identical intervals, so the points should be merged.
            (
                ScalarPoints::from(5.0),
                FIVE_SECS,
                ScalarPoints::from(6.0),
                FIVE_SECS,
                ScalarPoints::from(11.0),
                FIVE_SECS,
            ),
            // We do three permutations here -- identical timestamped point, differing timestamped point,
            // non-timestamped point -- but always with differing intervals, which should lead to B overriding A
            // entirely.
            (
                ScalarPoints::from((1, 1.0)),
                FIVE_SECS,
                ScalarPoints::from((1, 2.0)),
                TEN_SECS,
                ScalarPoints::from((1, 2.0)),
                TEN_SECS,
            ),
            (
                ScalarPoints::from((1, 3.0)),
                FIVE_SECS,
                ScalarPoints::from(4.0),
                TEN_SECS,
                ScalarPoints::from((0, 4.0)),
                TEN_SECS,
            ),
            (
                ScalarPoints::from(7.0),
                TEN_SECS,
                ScalarPoints::from(9.0),
                FIVE_SECS,
                ScalarPoints::from(9.0),
                FIVE_SECS,
            ),
        ];

        for (a, a_interval, b, b_interval, expected, expected_interval) in cases {
            let mut merged = MetricValues::Rate(a.clone(), a_interval);
            merged.merge(MetricValues::Rate(b.clone(), b_interval));

            assert_eq!(
                merged,
                MetricValues::Rate(expected.clone(), expected_interval),
                "merged {}/{:?} with {}/{:?}, expected {} but got {}",
                a,
                a_interval,
                b,
                b_interval,
                expected,
                merged
            );
        }
    }

    #[test]
    fn merge_sets() {
        let cases = [
            // Both A and B have single point with an identical timestamp, so the values should be merged.
            (
                SetPoints::from((1, "foo")),
                SetPoints::from((1, "bar")),
                SetPoints::from((1, ["foo", "bar"])),
            ),
            // A has a single point with a timestamp, B has a single point without a timestamp, so both points should be
            // kept.
            (
                SetPoints::from((1, "foo")),
                SetPoints::from("bar"),
                SetPoints::from([(0, "bar"), (1, "foo")]),
            ),
            // Both A and B have single point without a timestamp, so the values should be merged.
            (
                SetPoints::from("foo"),
                SetPoints::from("bar"),
                SetPoints::from(["foo", "bar"]),
            ),
        ];

        for (a, b, expected) in cases {
            let mut merged = MetricValues::Set(a.clone());
            merged.merge(MetricValues::Set(b.clone()));

            assert_eq!(
                merged,
                MetricValues::Set(expected.clone()),
                "merged {} with {}, expected {} but got {}",
                a,
                b,
                expected,
                merged
            );
        }
    }

    #[test]
    fn merge_histograms() {
        let cases = [
            // Both A and B have single point with an identical timestamp, so the samples should be merged.
            (
                HistogramPoints::from((1, 1.0)),
                HistogramPoints::from((1, 2.0)),
                HistogramPoints::from((1, [1.0, 2.0])),
            ),
            // A has a single point with a timestamp, B has a single point without a timestamp, so both points should be kept.
            (
                HistogramPoints::from((1, 1.0)),
                HistogramPoints::from(2.0),
                HistogramPoints::from([(0, 2.0), (1, 1.0)]),
            ),
            // Both A and B have single point without a timestamp, so the samples should be merged.
            (
                HistogramPoints::from(5.0),
                HistogramPoints::from(6.0),
                HistogramPoints::from([5.0, 6.0]),
            ),
        ];

        for (a, b, expected) in cases {
            let mut merged = MetricValues::Histogram(a.clone());
            merged.merge(MetricValues::Histogram(b.clone()));

            assert_eq!(
                merged,
                MetricValues::Histogram(expected.clone()),
                "merged {} with {}, expected {} but got {}",
                a,
                b,
                expected,
                merged
            );
        }
    }

    #[test]
    fn merge_distributions() {
        let cases = [
            // Both A and B have single point with an identical timestamp, so the sketches should be merged.
            (
                SketchPoints::from((1, 1.0)),
                SketchPoints::from((1, 2.0)),
                SketchPoints::from((1, [1.0, 2.0])),
            ),
            // A has a single point with a timestamp, B has a single point without a timestamp, so both points should be kept.
            (
                SketchPoints::from((1, 1.0)),
                SketchPoints::from(2.0),
                SketchPoints::from([(0, 2.0), (1, 1.0)]),
            ),
            // Both A and B have single point without a timestamp, so the sketches should be merged.
            (
                SketchPoints::from(5.0),
                SketchPoints::from(6.0),
                SketchPoints::from([5.0, 6.0]),
            ),
        ];

        for (a, b, expected) in cases {
            let mut merged = MetricValues::Distribution(a.clone());
            merged.merge(MetricValues::Distribution(b.clone()));

            assert_eq!(
                merged,
                MetricValues::Distribution(expected.clone()),
                "merged {} with {}, expected {} but got {}",
                a,
                b,
                expected,
                merged
            );
        }
    }
}
