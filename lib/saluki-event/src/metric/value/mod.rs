mod iter;

use std::{collections::HashSet, num::NonZeroU64, time::Duration};

use ddsketch_agent::DDSketch;
use ordered_float::OrderedFloat;
use smallvec::SmallVec;

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
            .drain_filter(|value| value.timestamp.map_or(false, |ts| ts.get() <= timestamp));
        Some(Self::from(new_values))
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

/// A set of scalar points.
///
/// Used to represent the data points of "scalar" metric types, such as counters, gauges, and rates. Each data point is
/// attached to an optional timestamp.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScalarPoints(TimestampedValues<OrderedFloat<f64>, 4>);

impl ScalarPoints {
    fn new() -> Self {
        Self(TimestampedValues::default())
    }

    fn add_point(&mut self, timestamp: Option<NonZeroU64>, value: f64) {
        self.0.values.push(TimestampedValue {
            timestamp,
            value: OrderedFloat(value),
        });
        self.0.sort_by_timestamp();
    }

    fn drain_timestamped(&mut self) -> Self {
        Self(self.0.drain_timestamped())
    }

    fn split_at_timestamp(&mut self, timestamp: u64) -> Option<Self> {
        self.0.split_at_timestamp(timestamp).map(Self)
    }

    /// Merges another set of points into this one.
    ///
    /// If a point with the same timestamp exists in both sets, the values will be added together. Otherwise, the points
    /// will appended to the end of the set.
    pub fn merge(&mut self, other: Self) {
        let mut needs_sort = false;
        for other_value in other.0.values {
            if let Some(existing_value) = self
                .0
                .values
                .iter_mut()
                .find(|value| value.timestamp == other_value.timestamp)
            {
                existing_value.value += other_value.value;
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

impl From<f64> for ScalarPoints {
    fn from(value: f64) -> Self {
        Self(TimestampedValue::from(OrderedFloat(value)).into())
    }
}

impl<'a> From<&'a [f64]> for ScalarPoints {
    fn from(values: &'a [f64]) -> Self {
        Self(TimestampedValues::from(values.iter().map(|value| OrderedFloat(*value))))
    }
}

impl<const N: usize> From<[f64; N]> for ScalarPoints {
    fn from(values: [f64; N]) -> Self {
        Self(TimestampedValues::from(values.iter().map(|value| OrderedFloat(*value))))
    }
}

impl From<(u64, f64)> for ScalarPoints {
    fn from((ts, value): (u64, f64)) -> Self {
        Self(TimestampedValue::from((ts, OrderedFloat(value))).into())
    }
}

impl<'a> From<&'a [(u64, f64)]> for ScalarPoints {
    fn from(values: &'a [(u64, f64)]) -> Self {
        Self(TimestampedValues::from(
            values.iter().map(|(ts, value)| (*ts, OrderedFloat(*value))),
        ))
    }
}

impl<const N: usize> From<[(u64, f64); N]> for ScalarPoints {
    fn from(values: [(u64, f64); N]) -> Self {
        Self(TimestampedValues::from(
            values.iter().map(|(ts, value)| (*ts, OrderedFloat(*value))),
        ))
    }
}

/// A set of sketch points.
///
/// Used to represent the data points of sketch-based metrics, such as distributions. Each point is attached to an
/// optional timestamp.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SketchPoints(TimestampedValues<DDSketch, 1>);

impl SketchPoints {
    fn drain_timestamped(&mut self) -> Self {
        Self(self.0.drain_timestamped())
    }

    fn split_at_timestamp(&mut self, timestamp: u64) -> Option<Self> {
        self.0.split_at_timestamp(timestamp).map(Self)
    }

    /// Merges another set of points into this one.
    ///
    /// If a point with the same timestamp exists in both sets, the sketches will be merged together. Otherwise, the
    /// points will appended to the end of the set.
    pub fn merge(&mut self, other: Self) {
        let mut needs_sort = false;
        for other_value in other.0.values {
            if let Some(existing_value) = self
                .0
                .values
                .iter_mut()
                .find(|value| value.timestamp == other_value.timestamp)
            {
                existing_value.value.merge(&other_value.value);
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

impl From<f64> for SketchPoints {
    fn from(value: f64) -> Self {
        let mut sketch = DDSketch::default();
        sketch.insert(value);

        Self(TimestampedValue::from(sketch).into())
    }
}

impl<'a> From<&'a [f64]> for SketchPoints {
    fn from(values: &'a [f64]) -> Self {
        let mut sketch = DDSketch::default();
        sketch.insert_many(values);

        Self(TimestampedValue::from(sketch).into())
    }
}

impl<const N: usize> From<[f64; N]> for SketchPoints {
    fn from(values: [f64; N]) -> Self {
        let mut sketch = DDSketch::default();
        sketch.insert_many(&values[..]);

        Self(TimestampedValue::from(sketch).into())
    }
}

impl From<DDSketch> for SketchPoints {
    fn from(value: DDSketch) -> Self {
        Self(TimestampedValue::from(value).into())
    }
}

impl From<(u64, f64)> for SketchPoints {
    fn from((ts, value): (u64, f64)) -> Self {
        let mut sketch = DDSketch::default();
        sketch.insert(value);

        Self(TimestampedValue::from((ts, sketch)).into())
    }
}

impl<'a> From<(u64, &'a [f64])> for SketchPoints {
    fn from((ts, values): (u64, &'a [f64])) -> Self {
        let mut sketch = DDSketch::default();
        sketch.insert_many(values);

        Self(TimestampedValue::from((ts, sketch)).into())
    }
}

impl<'a> From<&'a [(u64, &'a [f64])]> for SketchPoints {
    fn from(values: &'a [(u64, &'a [f64])]) -> Self {
        Self(TimestampedValues::from(values.iter().map(|(ts, values)| {
            let mut sketch = DDSketch::default();
            sketch.insert_many(values);

            (*ts, sketch)
        })))
    }
}

/// A set of set points.
///
/// Used to represent the data points of sets. Each data point is attached to an optional timestamp.
///
/// Sets are an exception to the common scalar or sketch-based points, where actual string values are held instead.
/// These are generally meant to represent some unique set of values, whose count is then used as the actual output
/// metric.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetPoints(TimestampedValues<HashSet<String>, 1>);

impl SetPoints {
    fn drain_timestamped(&mut self) -> Self {
        Self(self.0.drain_timestamped())
    }

    fn split_at_timestamp(&mut self, timestamp: u64) -> Option<Self> {
        self.0.split_at_timestamp(timestamp).map(Self)
    }

    /// Merges another set of points into this one.
    ///
    /// If a point with the same timestamp exists in both sets, the sets will be merged together. Otherwise, the points
    /// will appended to the end of the set.
    pub fn merge(&mut self, other: Self) {
        let mut needs_sort = false;
        for other_value in other.0.values {
            if let Some(existing_value) = self
                .0
                .values
                .iter_mut()
                .find(|value| value.timestamp == other_value.timestamp)
            {
                existing_value.value.extend(other_value.value);
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

impl From<String> for SetPoints {
    fn from(value: String) -> Self {
        Self(TimestampedValue::from(HashSet::from([value])).into())
    }
}

impl<'a> From<&'a str> for SetPoints {
    fn from(value: &'a str) -> Self {
        Self(TimestampedValue::from(HashSet::from([value.to_string()])).into())
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

    /// A distribution.
    ///
    /// Distributions represent the distribution of a quantity, such as the response times for a service. By tracking
    /// each individual measurement, statistics can be derived over the sample set to provide insight, such as minimum
    /// and maximum value, how many of the values are above or below a specific threshold, and more.
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
    pub fn counter_sampled_fallible<I, E>(iter: I, sample_rate: Option<f64>) -> Result<Self, E>
    where
        I: Iterator<Item = Result<f64, E>>,
    {
        let sample_rate_scaled = sample_rate.map(|rate| 1.0 / rate);
        let mut points = ScalarPoints::new();
        for value in iter {
            let value = value?;
            points.add_point(None, sample_rate_scaled.map_or(value, |sr| value * sr));
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
    pub fn distribution_sampled_fallible<I, E>(iter: I, sample_rate: Option<f64>) -> Result<Self, E>
    where
        I: Iterator<Item = Result<f64, E>>,
    {
        let mut sketch = DDSketch::default();
        match sample_rate {
            Some(rate) => {
                let n = (1.0 / rate) as u32;

                for value in iter {
                    let value = value?;
                    sketch.insert_n(value, n);
                }
            }
            None => {
                for value in iter {
                    sketch.insert(value?);
                }
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
            Self::Counter(points) | Self::Rate(points, _) | Self::Gauge(points) => points.0.values.is_empty(),
            Self::Set(points) => points.0.values.is_empty(),
            Self::Distribution(points) => points.0.values.is_empty(),
        }
    }

    /// Returns the number of values in this metric.
    pub fn len(&self) -> usize {
        match self {
            Self::Counter(points) | Self::Rate(points, _) | Self::Gauge(points) => points.0.values.len(),
            Self::Set(points) => points.0.values.len(),
            Self::Distribution(points) => points.0.values.len(),
        }
    }

    /// Returns `true` if all values in this metric have a timestamp.
    pub fn all_timestamped(&self) -> bool {
        match self {
            Self::Counter(points) | Self::Rate(points, _) | Self::Gauge(points) => points.0.all_timestamped(),
            Self::Set(points) => points.0.all_timestamped(),
            Self::Distribution(points) => points.0.all_timestamped(),
        }
    }

    /// Returns `true` if any values in this metric have a timestamp.
    pub fn any_timestamped(&self) -> bool {
        match self {
            Self::Counter(points) | Self::Rate(points, _) | Self::Gauge(points) => points.0.any_timestamped(),
            Self::Set(points) => points.0.any_timestamped(),
            Self::Distribution(points) => points.0.any_timestamped(),
        }
    }

    /// Sets the timestamp for all values in this metric.
    ///
    /// This overrides all existing timestamps whether they are set or not. If `timestamp` is zero, all existing
    /// timestamps will be cleared.
    pub fn set_timestamp(&mut self, timestamp: u64) {
        match self {
            Self::Counter(points) | Self::Gauge(points) | Self::Rate(points, _) => {
                for value in &mut points.0.values {
                    value.timestamp = NonZeroU64::new(timestamp);
                }
            }
            Self::Set(points) => {
                for value in &mut points.0.values {
                    value.timestamp = NonZeroU64::new(timestamp);
                }
            }
            Self::Distribution(points) => {
                for value in &mut points.0.values {
                    value.timestamp = NonZeroU64::new(timestamp);
                }
            }
        }
    }

    /// Splits all timestamped values into a new `MetricValues`, leaving the remaining values in `self`.
    pub fn split_timestamped(&mut self) -> Self {
        match self {
            Self::Counter(points) => Self::Counter(points.drain_timestamped()),
            Self::Rate(points, interval) => Self::Rate(points.drain_timestamped(), *interval),
            Self::Gauge(points) => Self::Gauge(points.drain_timestamped()),
            Self::Set(points) => Self::Set(points.drain_timestamped()),
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
            Self::Distribution(points) => points.split_at_timestamp(timestamp).map(Self::Distribution),
        }
    }

    /// Collapses all non-timestamped values into a single value with the given timestamp.
    ///
    pub fn collapse_non_timestamped(&mut self, timestamp: u64) {
        match self {
            // Collapse by summing.
            Self::Counter(points) => points.0.collapse_non_timestamped(timestamp, collapse_scalar_merge),
            Self::Rate(points, _) => points.0.collapse_non_timestamped(timestamp, collapse_scalar_merge),
            // Collapse by keeping the last value.
            Self::Gauge(points) => points.0.collapse_non_timestamped(timestamp, collapse_scalar_latest),
            // Collapse by merging.
            Self::Set(points) => points.0.collapse_non_timestamped(timestamp, collapse_set),
            Self::Distribution(sketches) => sketches.0.collapse_non_timestamped(timestamp, collapse_sketch),
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
            (Self::Counter(a), Self::Counter(b)) => a.merge(b),
            (Self::Rate(a_points, a_interval), Self::Rate(b_points, b_interval)) => {
                if *a_interval != b_interval {
                    *a_points = b_points;
                    *a_interval = b_interval;
                } else {
                    a_points.merge(b_points);
                }
            }
            (Self::Gauge(a), Self::Gauge(b)) => *a = b,
            (Self::Set(a), Self::Set(b)) => a.merge(b),
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
            Self::Distribution(_) => "distribution",
        }
    }
}

fn collapse_scalar_merge(dest: &mut OrderedFloat<f64>, src: &mut OrderedFloat<f64>) {
    *dest += *src;
}

fn collapse_scalar_latest(dest: &mut OrderedFloat<f64>, src: &mut OrderedFloat<f64>) {
    *dest = *src;
}

fn collapse_set(dest: &mut HashSet<String>, src: &mut HashSet<String>) {
    dest.extend(src.drain());
}

fn collapse_sketch(dest: &mut DDSketch, src: &mut DDSketch) {
    dest.merge(src);
}
