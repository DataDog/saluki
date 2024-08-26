use std::{collections::HashSet, fmt, time::Duration};

use ddsketch_agent::DDSketch;
use smallvec::SmallVec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Metric kind.
pub enum MetricKind {
    /// A counter.
    Counter,

    /// A rate.
    Rate,

    /// A gauge.
    Gauge,

    /// A set.
    Set,

    /// A distribution.
    Distribution,
}

impl MetricKind {
    /// Returns the string representation of the metric kind.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Rate => "rate",
            Self::Gauge => "gauge",
            Self::Set => "set",
            Self::Distribution => "distribution",
        }
    }
}

/// A metric value.
#[derive(Clone, Debug)]
pub enum MetricValue {
    /// A counter.
    ///
    /// Counters generally represent a monotonically increasing value, such as the number of requests received.
    Counter {
        /// Counter value.
        value: f64,
    },

    /// A rate.
    ///
    /// Rates define the rate of change over a given interval, in seconds.
    ///
    /// For example, a rate with a value of 1.5 and an interval of 10 seconds would indicate that the value increases by
    /// 15 every 10 seconds.
    Rate {
        /// Normalized per-second value.
        value: f64,

        /// Interval over which the rate is calculated.
        interval: Duration,
    },

    /// A gauge.
    ///
    /// Gauges represent the latest value of a quantity, such as the current number of active connections. This value
    /// can go up or down, but gauges do not track the individual changes to the value, only the latest value.
    Gauge {
        /// Gauge value.
        value: f64,
    },

    /// A set.
    ///
    /// Sets represent a collection of unique values, such as the unique IP addresses that have connected to a service.
    Set {
        /// Unique values in the set.
        values: HashSet<String>,
    },

    /// A distribution.
    ///
    /// Distributions represent the distribution of a quantity, such as the response times for a service. By tracking
    /// each individual measurement, statistics can be derived over the sample set to provide insight, such as minimum
    /// and maximum value, how many of the values are above or below a specific threshold, and more.
    Distribution {
        /// The internal sketch representing the distribution.
        sketch: DDSketch,
    },
}

impl MetricValue {
    /// Returns the type of the metric value.
    pub fn kind(&self) -> MetricKind {
        match self {
            Self::Counter { .. } => MetricKind::Counter,
            Self::Rate { .. } => MetricKind::Rate,
            Self::Gauge { .. } => MetricKind::Gauge,
            Self::Set { .. } => MetricKind::Set,
            Self::Distribution { .. } => MetricKind::Distribution,
        }
    }

    /// Creates a counter from the given value.
    pub fn counter(value: f64) -> Self {
        Self::Counter { value }
    }

    /// Creates a gauge from the given value.
    pub fn gauge(value: f64) -> Self {
        Self::Gauge { value }
    }

    /// Creates a set from the given values.
    pub fn set<I, T>(values: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<String>,
    {
        Self::Set {
            values: values.into_iter().map(Into::into).collect(),
        }
    }

    /// Creates a distribution from a single value.
    pub fn distribution_from_value(value: f64) -> Self {
        let mut sketch = DDSketch::default();
        sketch.insert(value);
        Self::Distribution { sketch }
    }

    /// Creates a distribution from multiple values.
    pub fn distribution_from_values(values: &[f64]) -> Self {
        let mut sketch = DDSketch::default();
        sketch.insert_many(values);
        Self::Distribution { sketch }
    }

    /// Creates a distribution from values in an iterator.
    pub fn distribution_from_iter<I, E>(iter: I) -> Result<Self, E>
    where
        I: Iterator<Item = Result<f64, E>>,
    {
        let mut sketch = DDSketch::default();
        for value in iter {
            sketch.insert(value?);
        }
        Ok(Self::Distribution { sketch })
    }

    /// Creates a rate from the total value and interval.
    ///
    /// The value will be divided by the interval, in seconds, to create a normalized per-second rate.
    pub fn rate_seconds(value: f64, interval: Duration) -> Self {
        let rate_value = value / interval.as_secs_f64();
        Self::Rate {
            value: rate_value,
            interval,
        }
    }

    /// Merges another metric value into this one.
    ///
    /// If both `self` and `other` are the same metric type, their values will be merged appropriately. If the metric
    /// types are different, or a specific precondition for the metric type is not met, the incoming value will override
    /// the existing value instead.
    ///
    /// For rates, the interval of both rates must match to be merged. For gauges, the incoming value will override the
    /// existing value.
    pub fn merge(&mut self, other: Self) {
        match (self, other) {
            (Self::Counter { value: a }, Self::Counter { value: b }) => *a += b,
            (Self::Rate { value: a, interval: i1 }, Self::Rate { value: b, interval: i2 }) if *i1 == i2 => *a += b,
            (Self::Gauge { value: a }, Self::Gauge { value: b }) => *a = b,
            (Self::Set { values: a }, Self::Set { values: b }) => {
                a.extend(b);
            }
            (Self::Distribution { sketch: sketch_a }, Self::Distribution { sketch: sketch_b }) => {
                sketch_a.merge(&sketch_b)
            }

            // Just override with whatever the incoming value is.
            (dest, src) => drop(std::mem::replace(dest, src)),
        }
    }
}

impl PartialEq for MetricValue {
    fn eq(&self, other: &Self) -> bool {
        use float_eq::FloatEq as _;

        match (self, other) {
            (Self::Counter { value: l_value }, Self::Counter { value: r_value }) => l_value.eq_ulps(r_value, &1),
            (
                Self::Rate {
                    value: l_value,
                    interval: l_interval,
                },
                Self::Rate {
                    value: r_value,
                    interval: r_interval,
                },
            ) => l_value.eq_ulps(r_value, &1) && l_interval == r_interval,
            (Self::Gauge { value: l_value }, Self::Gauge { value: r_value }) => l_value.eq_ulps(r_value, &1),
            (Self::Set { values: l_values }, Self::Set { values: r_values }) => l_values == r_values,
            (Self::Distribution { sketch: l_sketch }, Self::Distribution { sketch: r_sketch }) => l_sketch == r_sketch,
            _ => false,
        }
    }
}

impl Eq for MetricValue {}

impl fmt::Display for MetricValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Counter { value } => write!(f, "counter<{}>", value),
            Self::Rate { value, interval } => write!(f, "rate<{} over {:?}>", value, interval),
            Self::Gauge { value } => write!(f, "gauge<{}>", value),
            Self::Set { values } => write!(f, "set<{:?}>", values),
            Self::Distribution { sketch } => write!(f, "distribution<{:?}>", sketch),
        }
    }
}

/// A set of metric values.
///
/// `MetricValues` is a thin wrapper over a set of metric values, all of a homogenous type, that tracks the values as a
/// timestamp/value pair. This allows for holding multiple data points in a single metric.
///
/// ## Design
///
/// `MetricValues` is used to support two specific behaviors at the same time: holding values that may or may
/// not have a timestamp, and holding multiple values attached to a single metric. This is a common representation of
/// metrics, where a single metric may carry multiple values assigned to different buckets of time, such as the
/// OpenTelemetry metrics data model. However, values may not yet be associated with a timestamp, such as when they
/// arrive from applications that do not provide a timestamp.
///
/// Additionally, `MetricValues` only supports a homogenous set of values, which is either specified when creating an
/// empty `MetricValues` (using [`MetricValues::from_kind`]) or when creating it from a single value (using
/// [`MetricValues::from_value`].)
///
/// ## Timestamps
///
/// All metrics are associated with a timestamp value. The timestamp values themselves do not influence the behavior of
/// `MetricValues`, and are only relevant towards merging and sorting behavior, but generally, a timestamp of zero is
/// used to indicate no timestamp is (yet) associated with a value while non-zero timestamps are meant to indicate that
/// a timestamp is already associated with a value and should be used.
///
/// ## Ordering
///
///
///
/// ## Creating and adding values
///
/// As noted above, `MetricValues` only holds a single kind of metric value at any given time, which is established
/// creating `MetricValues` via [`MetricValues::from_kind`], [`MetricValues::from_value`],
/// [`MetricValues::from_values`], or [`MetricValues::from_values_fallible`]. Once a `MetricValues` instance is created,
/// further values can be added via [`MetricValues::merge_value`]. Even further, another `MetricValues` can be merged
/// into an existing set of values via [`MetricValues::merge_values`].
///
/// For most use cases, adding _new_ values to `MetricValues` should prefer [`MetricValues::push_value`], which adds the
/// value without any merging. This means that if a value already exists at the given timestamp, the new value will be
/// be added to the set as a separate entry. This allows downstream code to be able to observe the original values and
/// deal with any merging logic as needed.
///
/// ## Merging
///
/// In many cases, callers will either want to merge values with the same timestamp, whether merging single values or
/// merging another set of values. Both [`MetricValues::merge_value`] and [`MetricValues::merge_values`] support this
/// behavior by merging the incoming value(s) with any existing value(s) with the same timestamp. If no value exists at
/// the given timestamp, new entries will be added.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetricValues {
    kind: MetricKind,
    values: SmallVec<[(u64, MetricValue); 2]>,
}

impl MetricValues {
    /// Creates a new, empty `MetricValues` of the given metric kind.
    pub fn from_kind(kind: MetricKind) -> Self {
        Self {
            kind,
            values: SmallVec::new(),
        }
    }

    /// Creates a new set of metric values with the given value.
    ///
    /// The timestamp for the value is set to zero.
    pub fn from_value(value: MetricValue) -> Self {
        let kind = value.kind();
        let mut values = SmallVec::new();
        values.push((0, value));

        Self { kind, values }
    }

    /// Creates a new set of metric values with the given iterator of values.
    ///
    /// The timestamp for the values is set to zero, and values are merged following the normal merging logic of
    /// [`MetricValue::merge`].
    ///
    /// If the iterator is empty or contains values of different kinds, `None` is returned. Otherwise, `Some(_)`
    /// is returned containing the set of values.
    pub fn from_values<I>(values: I) -> Option<Self>
    where
        I: IntoIterator<Item = MetricValue>,
    {
        let mut maybe_values: Option<Self> = None;

        for value in values {
            if let Some(existing_values) = maybe_values.as_mut() {
                // Container is initialized, so proceed with the normal checks and push the value in.
                if existing_values.kind() != value.kind() {
                    return None;
                }

                if existing_values.merge_value(0, value).is_some() {
                    panic!("already assert metric kind matches");
                }
            } else {
                // Container is not initialized, so create it from the first value.
                maybe_values = Some(Self::from_value(value));
            }
        }

        maybe_values
    }

    /// Creates a new set of metric values with the given fallible iterator of values.
    ///
    /// The timestamp for the values is set to zero, and values are merged following the normal merging logic of
    /// [`MetricValue::merge`].
    ///
    /// If the iterator is empty or contains values of different kinds, `Ok(None)` is returned. Otherwise, `Ok(Some(_))`
    /// is returned containing the set of values.
    ///
    /// ## Errors
    ///
    /// If any of the values yielded from the iterator are an error, this function will short-circuit and return the
    /// error.
    pub fn from_values_fallible<I, E>(values: I) -> Result<Option<Self>, E>
    where
        I: IntoIterator<Item = Result<MetricValue, E>>,
    {
        let mut maybe_values: Option<Self> = None;

        for value in values {
            let value = value?;

            if let Some(existing_values) = maybe_values.as_mut() {
                // Container is initialized, so proceed with the normal checks and push the value in.
                if existing_values.kind() != value.kind() {
                    return Ok(None);
                }

                if existing_values.merge_value(0, value).is_some() {
                    panic!("already assert metric kind matches");
                }
            } else {
                // Container is not initialized, so create it from the first value.
                maybe_values = Some(Self::from_value(value));
            }
        }

        Ok(maybe_values)
    }

    /// Returns `true` if the set is empty.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Returns the number of values in the set.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns the kind of metric values held by the set.
    pub fn kind(&self) -> MetricKind {
        self.kind
    }

    /// Returns `true` if any values in the set have a timestamp.
    pub fn any_timestamped(&self) -> bool {
        self.values.iter().any(|(ts, _)| *ts != 0)
    }

    /// Returns `true` if all values in the set have a timestamp.
    pub fn all_timestamped(&self) -> bool {
        self.values.iter().all(|(ts, _)| *ts != 0)
    }

    fn sort_by_timestamp(&mut self) {
        self.values.sort_unstable_by_key(|(ts, _)| *ts);
    }

    fn merge_value_inner(&mut self, timestamp: u64, value: MetricValue) -> (Option<MetricValue>, bool) {
        if value.kind() != self.kind {
            return (Some(value), false);
        }

        let needs_sort = if let Some((_, existing_value)) = self.values.iter_mut().find(|(ts, _)| *ts == timestamp) {
            existing_value.merge(value);
            false
        } else {
            self.values.push((timestamp, value));
            true
        };

        (None, needs_sort)
    }

    /// Merges a metric value into this set of values with the given timestamp.
    ///
    /// If a value already exists at the given timestamp, the incoming value will be merged into the existing value
    /// following the normal merging logic of [`MetricValue::merge`]. Otherwise, a new entry will be added.
    ///
    /// ## Errors
    ///
    /// If the value's kind does not match the kind of this set, an error will be returned containing the original value.
    pub fn merge_value(&mut self, timestamp: u64, value: MetricValue) -> Option<MetricValue> {
        match self.merge_value_inner(timestamp, value) {
            (Some(value), _) => Some(value),
            (None, needs_sort) => {
                if needs_sort {
                    self.sort_by_timestamp();
                }

                None
            }
        }
    }

    /// Merges metric values into this set of values with the given timestamp.
    ///
    /// Each value in `values` is merged via [`merge_value`](Self::merge_value), and follows the merging logic described
    /// therein.
    ///
    /// ## Errors
    ///
    /// If any of the values have a kind that does not match the kind of this set, an error will be returned containing
    /// the original values.
    pub fn merge_values(&mut self, values: Self) -> Option<Self> {
        if self.kind != values.kind {
            return Some(values);
        }

        // Defer sorting until after we merge all values.
        let mut needs_sort_overall = false;
        for (timestamp, value) in values.values {
            match self.merge_value_inner(timestamp, value) {
                (Some(_), _) => panic!("values should be the same kind"),
                (None, needs_sort) => {
                    if needs_sort {
                        needs_sort_overall = true;
                    }
                }
            }
        }

        if needs_sort_overall {
            self.sort_by_timestamp();
        }

        None
    }

    /// Sets the timestamp for all values in the set to the given value.
    pub fn set_timestamp(&mut self, timestamp: u64) {
        for (existing_ts, _) in &mut self.values {
            *existing_ts = timestamp;
        }
    }

    /// Creates a new `MetricValues` containing all values from `self`.
    ///i
    /// This retains the existing capacity of the set.
    pub fn split_all(&mut self) -> Self {
        let values = self.values.drain(..).collect();

        Self {
            kind: self.kind,
            values,
        }
    }

    /// Consumes all values in the set.
    ///
    /// This retains the existing capacity of the set.
    pub fn take_values(&mut self) -> impl Iterator<Item = (u64, MetricValue)> + '_ {
        self.values.drain(..)
    }
}

impl fmt::Display for MetricValues {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[")?;

        let mut wrote_value = false;

        for (timestamp, value) in &self.values {
            if wrote_value {
                write!(f, ", ")?;
            }

            write!(f, "({}: {})", timestamp, value)?;
            wrote_value = true;
        }

        write!(f, "]")
    }
}

impl<'a> IntoIterator for &'a MetricValues {
    type Item = &'a (u64, MetricValue);
    type IntoIter = std::slice::Iter<'a, (u64, MetricValue)>;

    fn into_iter(self) -> Self::IntoIter {
        self.values.iter()
    }
}

impl<'a> IntoIterator for &'a mut MetricValues {
    type Item = &'a mut (u64, MetricValue);
    type IntoIter = std::slice::IterMut<'a, (u64, MetricValue)>;

    fn into_iter(self) -> Self::IntoIter {
        self.values.iter_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! counter_values {
        ($($ts:expr => $value:expr),+) => {
            MetricValues {
                kind: MetricKind::Counter,
                values: vec![$(
                    ($ts, MetricValue::counter($value)),
                )+].into()
            }
        };
    }

    macro_rules! gauge_values {
        ($($ts:expr => $value:expr),+) => {
            MetricValues {
                kind: MetricKind::Gauge,
                values: vec![$(
                    ($ts, MetricValue::gauge($value)),
                )+].into()
            }
        };
    }

    #[test]
    fn merge_counter_value() {
        let mut a = MetricValue::counter(1.0);
        let b = MetricValue::counter(2.0);

        a.merge(b);
        assert_eq!(a, MetricValue::counter(3.0));
    }

    #[test]
    fn merge_rate_value_same_interval() {
        let mut a = MetricValue::rate_seconds(1.0, Duration::from_secs(1));
        let b = MetricValue::rate_seconds(2.0, Duration::from_secs(1));

        a.merge(b);
        assert_eq!(a, MetricValue::rate_seconds(3.0, Duration::from_secs(1)));
    }

    #[test]
    fn merge_rate_value_different_interval() {
        let mut a = MetricValue::rate_seconds(1.0, Duration::from_secs(1));
        let b = MetricValue::rate_seconds(2.0, Duration::from_secs(2));

        a.merge(b);
        assert_eq!(a, MetricValue::rate_seconds(2.0, Duration::from_secs(2)));
    }

    #[test]
    fn merge_gauge_value() {
        let mut a = MetricValue::gauge(1.0);
        let b = MetricValue::gauge(2.0);

        a.merge(b);
        assert_eq!(a, MetricValue::gauge(2.0));
    }

    #[test]
    fn merge_set_value() {
        let mut a = MetricValue::set(["a", "b"]);
        let b = MetricValue::set(["b", "c"]);

        a.merge(b);
        assert_eq!(a, MetricValue::set(["a", "b", "c"]));
    }

    #[test]
    fn merge_distribution_value() {
        let mut a = MetricValue::distribution_from_value(1.0);
        let b = MetricValue::distribution_from_value(2.0);

        a.merge(b);
        assert_eq!(a, MetricValue::distribution_from_values(&[1.0, 2.0]));
    }

    #[test]
    fn merge_different_kind_value() {
        let mut a = MetricValue::counter(1.0);
        let b = MetricValue::gauge(2.0);

        a.merge(b);
        assert_eq!(a, MetricValue::gauge(2.0));
    }

    #[test]
    fn is_empty_len() {
        let values = MetricValues::from_kind(MetricKind::Counter);
        assert!(values.is_empty());
        assert_eq!(values.len(), 0);

        let values = counter_values!(0 => 1.0);
        assert!(!values.is_empty());
        assert_eq!(values.len(), 1);
    }

    #[test]
    fn from_value_kind() {
        // What matters here is that when we create `MetricValues` from a single value, the kind is set to the kind of
        // the value.
        let value = MetricValue::counter(1.0);
        let values = MetricValues::from_value(value.clone());

        assert_eq!(value.kind(), values.kind());
    }

    #[test]
    fn from_value_default_timestamp() {
        // What matters here is that when we create `MetricValues` from a single value, the timestamp is set to zero.
        let values = MetricValues::from_value(MetricValue::counter(1.0));

        assert_eq!(values, counter_values!(0 => 1.0));
    }

    #[test]
    fn from_values_empty() {
        let values = MetricValues::from_values(std::iter::empty());
        assert_eq!(values, None);
    }

    #[test]
    fn from_values_single() {
        let values = MetricValues::from_values(std::iter::once(MetricValue::counter(1.0)));
        assert_eq!(values, Some(counter_values!(0 => 1.0)));
    }

    #[test]
    fn from_values_multiple() {
        let values = MetricValues::from_values(vec![MetricValue::counter(1.0), MetricValue::counter(2.0)]);

        assert_eq!(values, Some(counter_values!(0 => 3.0)));
    }

    #[test]
    fn from_values_different_kinds() {
        let values = MetricValues::from_values(vec![MetricValue::counter(1.0), MetricValue::gauge(2.0)]);

        assert_eq!(values, None);
    }

    #[test]
    fn from_values_fallible_empty() {
        let values = MetricValues::from_values_fallible(std::iter::empty::<Result<MetricValue, ()>>());
        assert_eq!(values, Ok(None));
    }

    #[test]
    fn from_values_fallible_single() {
        let values = MetricValues::from_values_fallible(std::iter::once(Ok::<_, ()>(MetricValue::counter(1.0))));
        assert_eq!(values, Ok(Some(counter_values!(0 => 1.0))));
    }

    #[test]
    fn from_values_fallible_multiple() {
        let values = MetricValues::from_values_fallible(vec![
            Ok::<_, ()>(MetricValue::counter(1.0)),
            Ok::<_, ()>(MetricValue::counter(2.0)),
        ]);

        assert_eq!(values, Ok(Some(counter_values!(0 => 3.0))));
    }

    #[test]
    fn from_values_fallible_different_kinds() {
        let values = MetricValues::from_values_fallible(vec![
            Ok::<_, ()>(MetricValue::counter(1.0)),
            Ok::<_, ()>(MetricValue::gauge(2.0)),
        ]);

        assert_eq!(values, Ok(None));
    }

    #[test]
    fn from_values_fallible_error_item() {
        let values = MetricValues::from_values_fallible(vec![Ok::<_, ()>(MetricValue::counter(1.0)), Err(())]);

        assert_eq!(values, Err(()));
    }

    #[test]
    fn merge_values_single_same_kind() {
        let mut values = counter_values!(0 => 1.0);

        assert_eq!(values.merge_value(0, MetricValue::counter(2.0)), None);
        assert_eq!(values, counter_values!(0 => 3.0));

        assert_eq!(values.merge_value(10, MetricValue::counter(4.0)), None);
        assert_eq!(values, counter_values!(0 => 3.0, 10 => 4.0));

        assert_eq!(values.merge_value(0, MetricValue::counter(5.0)), None);
        assert_eq!(values, counter_values!(0 => 8.0, 10 => 4.0));

        assert_eq!(values.merge_value(10, MetricValue::counter(6.0)), None);
        assert_eq!(values, counter_values!(0 => 8.0, 10 => 10.0));
    }

    #[test]
    fn merge_values_single_different_kind() {
        let mut values = counter_values!(0 => 1.0);

        let gauge_value = MetricValue::gauge(2.0);
        let original_gauge_value = gauge_value.clone();

        assert_eq!(values.merge_value(0, gauge_value), Some(original_gauge_value));
        assert_eq!(values, counter_values!(0 => 1.0));
    }

    #[test]
    fn merge_values_multiple_same_kind() {
        let mut values_a = counter_values!(0 => 1.0, 10 => 2.0);
        let values_b = counter_values!(0 => 3.0, 10 => 4.0);

        assert_eq!(values_a.merge_values(values_b), None);
        assert_eq!(values_a, counter_values!(0 => 4.0, 10 => 6.0));
    }

    #[test]
    fn merge_values_multiple_different_kind() {
        let mut values_a = counter_values!(0 => 1.0, 10 => 2.0);
        let values_b = gauge_values!(0 => 3.0, 10 => 4.0);
        let original_values_b = values_b.clone();

        assert_eq!(values_a.merge_values(values_b), Some(original_values_b));
        assert_eq!(values_a, counter_values!(0 => 1.0, 10 => 2.0));
    }

    #[test]
    fn timestamp_checks() {
        let no_timestamp_values1 = counter_values!(0 => 1.0);
        assert!(!no_timestamp_values1.any_timestamped());
        assert!(!no_timestamp_values1.all_timestamped());

        let no_timestamp_values2 = counter_values!(0 => 1.0, 0 => 2.0);
        assert!(!no_timestamp_values2.any_timestamped());
        assert!(!no_timestamp_values2.all_timestamped());

        let mixed_timestamp_values = counter_values!(0 => 1.0, 10 => 2.0);
        assert!(mixed_timestamp_values.any_timestamped());
        assert!(!mixed_timestamp_values.all_timestamped());

        let all_timestamp_values = counter_values!(5 => 1.0, 10 => 2.0);
        assert!(all_timestamp_values.any_timestamped());
        assert!(all_timestamp_values.all_timestamped());
    }

    #[test]
    fn set_timestamp() {
        let mut values = counter_values!(0 => 1.0, 10 => 2.0);
        values.set_timestamp(5);

        assert_eq!(values, counter_values!(5 => 1.0, 5 => 2.0));
    }

    #[test]
    fn split_all() {
        let mut values = counter_values!(0 => 1.0, 10 => 2.0);
        let split_values = values.split_all();

        assert!(values.is_empty());
        assert_eq!(split_values, counter_values!(0 => 1.0, 10 => 2.0));
    }
}
