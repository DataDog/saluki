use std::num::NonZeroU64;

use ordered_float::OrderedFloat;

use super::{iter::{PointsIter, PointsIterRef}, TimestampedValue, TimestampedValues};

/// A set of scalar points.
///
/// Used to represent the data points of "scalar" metric types, such as counters, gauges, and rates. Each data point is
/// attached to an optional timestamp.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScalarPoints(TimestampedValues<OrderedFloat<f64>, 4>);

impl ScalarPoints {
    pub(super) fn new() -> Self {
        Self(TimestampedValues::default())
    }

    pub(super) fn inner(&self) -> &TimestampedValues<OrderedFloat<f64>, 4> {
        &self.0
    }

    pub(super) fn inner_mut(&mut self) -> &mut TimestampedValues<OrderedFloat<f64>, 4> {
        &mut self.0
    }

    pub(super) fn add_point(&mut self, timestamp: Option<NonZeroU64>, value: f64) {
        self.0.values.push(TimestampedValue {
            timestamp,
            value: OrderedFloat(value),
        });
        self.0.sort_by_timestamp();
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

impl<T> FromIterator<T> for ScalarPoints
where
    T: Into<TimestampedValue<OrderedFloat<f64>>>,
{
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self(TimestampedValues::from(iter))
    }
}

impl IntoIterator for ScalarPoints {
    type Item = (Option<NonZeroU64>, f64);
    type IntoIter = PointsIter;

    fn into_iter(self) -> Self::IntoIter {
        PointsIter::scalar(self.0.values.into_iter())
    }
}

impl<'a> IntoIterator for &'a ScalarPoints {
    type Item = (Option<NonZeroU64>, f64);
    type IntoIter = PointsIterRef<'a>;

    fn into_iter(self) -> Self::IntoIter {
        PointsIterRef::scalar(self.0.values.iter())
    }
}
