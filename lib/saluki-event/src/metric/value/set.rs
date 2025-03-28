use std::{collections::HashSet, fmt, num::NonZeroU64};

use super::{
    iter::{PointsIter, PointsIterRef},
    TimestampedValue, TimestampedValues,
};

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
    pub(super) fn inner(&self) -> &TimestampedValues<HashSet<String>, 1> {
        &self.0
    }

    pub(super) fn inner_mut(&mut self) -> &mut TimestampedValues<HashSet<String>, 1> {
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

impl<'a> From<(u64, &'a str)> for SetPoints {
    fn from((ts, value): (u64, &'a str)) -> Self {
        Self(TimestampedValue::from((ts, HashSet::from([value.to_string()]))).into())
    }
}

impl<'a, const N: usize> From<[&'a str; N]> for SetPoints {
    fn from(values: [&'a str; N]) -> Self {
        Self(TimestampedValue::from(HashSet::from_iter(values.into_iter().map(|s| s.to_string()))).into())
    }
}

impl<'a, const N: usize> From<(u64, [&'a str; N])> for SetPoints {
    fn from((ts, values): (u64, [&'a str; N])) -> Self {
        Self(TimestampedValue::from((ts, values.into_iter().map(|s| s.to_string()).collect())).into())
    }
}

impl<'a, const N: usize> From<[(u64, &'a str); N]> for SetPoints {
    fn from(values: [(u64, &'a str); N]) -> Self {
        Self(
            values
                .iter()
                .map(|(ts, value)| TimestampedValue::from((*ts, HashSet::from([value.to_string()]))))
                .into(),
        )
    }
}

impl<'a, const N: usize> From<[(u64, &'a [&'a str]); N]> for SetPoints {
    fn from(values: [(u64, &'a [&'a str]); N]) -> Self {
        Self(
            values
                .iter()
                .map(|(ts, values)| TimestampedValue::from((*ts, values.iter().map(|s| s.to_string()).collect())))
                .into(),
        )
    }
}

impl IntoIterator for SetPoints {
    type Item = (Option<NonZeroU64>, f64);
    type IntoIter = PointsIter;

    fn into_iter(self) -> Self::IntoIter {
        PointsIter::set(self.0.values.into_iter())
    }
}

impl<'a> IntoIterator for &'a SetPoints {
    type Item = (Option<NonZeroU64>, f64);
    type IntoIter = PointsIterRef<'a>;

    fn into_iter(self) -> Self::IntoIter {
        PointsIterRef::set(self.0.values.iter())
    }
}

impl fmt::Display for SetPoints {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[")?;
        for (i, point) in self.0.values.iter().enumerate() {
            if i > 0 {
                write!(f, ",")?;
            }

            let ts = point.timestamp.map(|ts| ts.get()).unwrap_or_default();
            write!(f, "({}, [", ts)?;
            for (j, value) in point.value.iter().enumerate() {
                if j > 0 {
                    write!(f, ",")?;
                }
                write!(f, "{}", value)?;
            }
            write!(f, "])")?;
        }
        write!(f, "]")
    }
}
