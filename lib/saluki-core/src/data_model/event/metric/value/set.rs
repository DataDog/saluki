use std::{collections::HashSet, fmt, num::NonZeroU64};

use stringtheory::MetaString;

use super::{
    iter::{PointsIter, PointsIterRef},
    TimestampedValue, TimestampedValues,
};

/// The distinct members of one set value.
///
/// Most aggregated sets hold zero or one member, so those cases store no hash table. A table exists only once a
/// second distinct member arrives.
///
/// # Invariants
///
/// `Many` always holds two or more members. Only `insert` and `merge` create it, both with at least two members, and
/// no method removes members. Thus, `Empty` and `One` are the only forms of a set with fewer than two members.
///
/// # Design
///
/// `Many` boxes its `HashSet`, which `clippy::box_collection` warns about. The box is a size trade-off on 64-bit
/// targets:
///
/// - A `HashSet<MetaString>` is 48 bytes, a `MetaString` is 24 bytes, and a `Box` is 8 bytes.
/// - With the box, `SetMembers` is 32 bytes, and an aggregation `SetRow` is 56 bytes.
/// - Without the box, `SetMembers` would be 56 bytes, and a `SetRow` would be 80 bytes.
///
/// Every set row pays for the largest variant, but only sets with two or more members use a table. The box costs one
/// extra allocation and one pointer hop for those sets only.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) enum SetMembers {
    /// A set with no members.
    #[default]
    Empty,

    /// A set with one member, stored inline.
    One(MetaString),

    /// A set with two or more distinct members.
    #[expect(
        clippy::box_collection,
        reason = "The box keeps `SetMembers` at 32 bytes instead of 56 bytes. See the type docs."
    )]
    Many(Box<HashSet<MetaString>>),
}

impl SetMembers {
    pub(super) fn len(&self) -> usize {
        match self {
            Self::Empty => 0,
            Self::One(_) => 1,
            Self::Many(values) => values.len(),
        }
    }

    fn insert(&mut self, value: MetaString) {
        match self {
            Self::Empty => *self = Self::One(value),
            Self::One(existing) if *existing != value => {
                let first = std::mem::take(existing);
                *self = Self::Many(Box::new(HashSet::from([first, value])));
            }
            Self::One(_) => {}
            Self::Many(values) => {
                values.insert(value);
            }
        }
    }

    pub(super) fn merge(&mut self, other: Self) {
        match other {
            Self::Empty => {}
            Self::One(value) => self.insert(value),
            Self::Many(mut values) => match self {
                Self::Empty => *self = Self::Many(values),
                Self::One(value) => {
                    values.insert(std::mem::take(value));
                    *self = Self::Many(values);
                }
                Self::Many(existing) => existing.extend(*values),
            },
        }
    }

    fn iter(&self) -> impl Iterator<Item = &MetaString> {
        let (one, many) = match self {
            Self::Empty => (None, None),
            Self::One(value) => (Some(value), None),
            Self::Many(values) => (None, Some(values.iter())),
        };
        one.into_iter().chain(many.into_iter().flatten())
    }
}

impl<'a> FromIterator<&'a str> for SetMembers {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut members = Self::Empty;
        for value in iter {
            members.insert(MetaString::from(value));
        }
        members
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
pub struct SetPoints(TimestampedValues<SetMembers, 1>);

impl SetPoints {
    pub(super) fn inner(&self) -> &TimestampedValues<SetMembers, 1> {
        &self.0
    }

    pub(super) fn inner_mut(&mut self) -> &mut TimestampedValues<SetMembers, 1> {
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
                existing_value.value.merge(other_value.value);
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
    /// Creates a set with one member.
    ///
    /// `MetaString::from(String)` keeps the heap allocation of the `String`, even for a short string. This method
    /// first tries `MetaString::try_inline`. Thus, a member of up to 23 bytes on 64-bit little-endian targets lives
    /// inside the `MetaString`, and the set keeps no heap buffer for it.
    fn from(value: String) -> Self {
        let value = MetaString::try_inline(&value).unwrap_or_else(|| MetaString::from(value));
        Self(TimestampedValue::from(SetMembers::One(value)).into())
    }
}

impl<'a> From<&'a str> for SetPoints {
    fn from(value: &'a str) -> Self {
        Self(TimestampedValue::from(SetMembers::One(MetaString::from(value))).into())
    }
}

impl<'a> From<(u64, &'a str)> for SetPoints {
    fn from((ts, value): (u64, &'a str)) -> Self {
        Self(TimestampedValue::from((ts, SetMembers::One(MetaString::from(value)))).into())
    }
}

impl<'a, const N: usize> From<[&'a str; N]> for SetPoints {
    fn from(values: [&'a str; N]) -> Self {
        Self(TimestampedValue::from(values.into_iter().collect::<SetMembers>()).into())
    }
}

impl<'a, const N: usize> From<(u64, [&'a str; N])> for SetPoints {
    fn from((ts, values): (u64, [&'a str; N])) -> Self {
        Self(TimestampedValue::from((ts, values.into_iter().collect::<SetMembers>())).into())
    }
}

impl<'a, const N: usize> From<[(u64, &'a str); N]> for SetPoints {
    fn from(values: [(u64, &'a str); N]) -> Self {
        Self(
            values
                .iter()
                .map(|(ts, value)| TimestampedValue::from((*ts, SetMembers::One(MetaString::from(*value)))))
                .into(),
        )
    }
}

impl<'a, const N: usize> From<[(u64, &'a [&'a str]); N]> for SetPoints {
    fn from(values: [(u64, &'a [&'a str]); N]) -> Self {
        Self(
            values
                .iter()
                .map(|(ts, values)| TimestampedValue::from((*ts, values.iter().copied().collect::<SetMembers>())))
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

#[cfg(test)]
mod tests {
    use super::{SetMembers, SetPoints};

    #[test]
    fn singleton_stays_inline() {
        let member = if cfg!(target_endian = "little") {
            "12345678901234567890123"
        } else {
            "1234567890123456"
        };
        let mut points = SetPoints::from(member);
        points.merge(SetPoints::from(member.to_owned()));
        let SetMembers::One(value) = &points.inner().values[0].value else {
            panic!("expected one member without a backing set");
        };
        assert_eq!(value, member);
        assert!(value.is_cheaply_cloneable());
        assert_eq!(points.len(), 1);
        assert_eq!((&points).into_iter().collect::<Vec<_>>(), [(None, 1.0)]);
        assert_eq!(points.clone().into_iter().collect::<Vec<_>>(), [(None, 1.0)]);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn boxed_many_variant_keeps_set_members_small() {
        use std::{collections::HashSet, mem::size_of};

        use stringtheory::MetaString;

        assert_eq!(size_of::<HashSet<MetaString>>(), 48);
        assert_eq!(size_of::<SetMembers>(), 32);
    }

    #[test]
    fn promotes_only_for_distinct_members() {
        let mut points = SetPoints::from("first");
        points.merge(SetPoints::from("second"));
        assert!(matches!(points.inner().values[0].value, SetMembers::Many(_)));
        points.merge(SetPoints::from(["first", "third"]));
        assert_eq!(points, SetPoints::from(["third", "second", "first"]));
        assert_eq!(points.into_iter().collect::<Vec<_>>(), [(None, 3.0)]);
    }

    #[test]
    fn merges_each_timestamp_independently() {
        let mut points = SetPoints::from([(10, "a"), (20, "b")]);
        points.merge(SetPoints::from([(10, &["a", "b"][..]), (30, &[][..])]));
        assert_eq!(points.len(), 3);
        assert_eq!(
            points
                .into_iter()
                .map(|(ts, count)| (ts.unwrap().get(), count))
                .collect::<Vec<_>>(),
            [(10, 2.0), (20, 1.0), (30, 0.0)]
        );
    }
}
