use std::num::NonZeroU64;

use ddsketch_agent::DDSketch;

use super::{TimestampedValue, TimestampedValues};

/// A set of sketch points.
///
/// Used to represent the data points of sketch-based metrics, such as distributions. Each point is attached to an
/// optional timestamp.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SketchPoints(TimestampedValues<DDSketch, 1>);

impl SketchPoints {
    pub(super) fn inner(&self) -> &TimestampedValues<DDSketch, 1> {
        &self.0
    }

    pub(super) fn inner_mut(&mut self) -> &mut TimestampedValues<DDSketch, 1> {
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

impl<'a, const N: usize> From<&'a [f64; N]> for SketchPoints {
    fn from(values: &'a [f64; N]) -> Self {
        let mut sketch = DDSketch::default();
        sketch.insert_many(values);

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

impl<'a, const N: usize> From<[(u64, &'a [f64]); N]> for SketchPoints {
    fn from(values: [(u64, &'a [f64]); N]) -> Self {
        Self(TimestampedValues::from(values.iter().map(|(ts, values)| {
            let mut sketch = DDSketch::default();
            sketch.insert_many(values);

            (*ts, sketch)
        })))
    }
}

impl IntoIterator for SketchPoints {
    type Item = (Option<NonZeroU64>, DDSketch);
    type IntoIter = SketchesIter;

    fn into_iter(self) -> Self::IntoIter {
        SketchesIter {
            inner: self.0.values.into_iter(),
        }
    }
}

impl<'a> IntoIterator for &'a SketchPoints {
    type Item = (Option<NonZeroU64>, &'a DDSketch);
    type IntoIter = SketchesIterRef<'a>;

    fn into_iter(self) -> Self::IntoIter {
        SketchesIterRef {
            inner: self.0.values.iter(),
        }
    }
}

pub struct SketchesIter {
    inner: smallvec::IntoIter<[TimestampedValue<DDSketch>; 1]>,
}

impl Iterator for SketchesIter {
    type Item = (Option<NonZeroU64>, DDSketch);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|value| (value.timestamp, value.value))
    }
}

pub struct SketchesIterRef<'a> {
    inner: std::slice::Iter<'a, TimestampedValue<DDSketch>>,
}

impl<'a> Iterator for SketchesIterRef<'a> {
    type Item = (Option<NonZeroU64>, &'a DDSketch);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|value| (value.timestamp, &value.value))
    }
}
