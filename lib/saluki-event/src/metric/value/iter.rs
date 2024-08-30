use std::{collections::HashSet, num::NonZeroU64};

use ddsketch_agent::DDSketch;
use ordered_float::OrderedFloat;

use super::{ScalarPoints, SetPoints, SketchPoints, TimestampedValue};

impl IntoIterator for ScalarPoints {
    type Item = (Option<NonZeroU64>, f64);
    type IntoIter = PointsIter;

    fn into_iter(self) -> Self::IntoIter {
        PointsIter {
            inner: PointsIterInner::Scalar(self.0.values.into_iter()),
        }
    }
}

impl<'a> IntoIterator for &'a ScalarPoints {
    type Item = (Option<NonZeroU64>, f64);
    type IntoIter = PointsIterRef<'a>;

    fn into_iter(self) -> Self::IntoIter {
        PointsIterRef {
            inner: PointsIterRefInner::Scalar(self.0.values.iter()),
        }
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

impl IntoIterator for SetPoints {
    type Item = (Option<NonZeroU64>, f64);
    type IntoIter = PointsIter;

    fn into_iter(self) -> Self::IntoIter {
        PointsIter {
            inner: PointsIterInner::Set(self.0.values.into_iter()),
        }
    }
}

impl<'a> IntoIterator for &'a SetPoints {
    type Item = (Option<NonZeroU64>, f64);
    type IntoIter = PointsIterRef<'a>;

    fn into_iter(self) -> Self::IntoIter {
        PointsIterRef {
            inner: PointsIterRefInner::Set(self.0.values.iter()),
        }
    }
}

enum PointsIterInner {
    Scalar(smallvec::IntoIter<[TimestampedValue<OrderedFloat<f64>>; 4]>),
    Set(smallvec::IntoIter<[TimestampedValue<HashSet<String>>; 1]>),
}

pub struct PointsIter {
    inner: PointsIterInner,
}

impl Iterator for PointsIter {
    type Item = (Option<NonZeroU64>, f64);

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            PointsIterInner::Scalar(iter) => iter.next().map(|value| (value.timestamp, value.value.into_inner())),
            PointsIterInner::Set(iter) => iter.next().map(|value| (value.timestamp, value.value.len() as f64)),
        }
    }
}

enum PointsIterRefInner<'a> {
    Scalar(std::slice::Iter<'a, TimestampedValue<OrderedFloat<f64>>>),
    Set(std::slice::Iter<'a, TimestampedValue<HashSet<String>>>),
}

pub struct PointsIterRef<'a> {
    inner: PointsIterRefInner<'a>,
}

impl<'a> Iterator for PointsIterRef<'a> {
    type Item = (Option<NonZeroU64>, f64);

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            PointsIterRefInner::Scalar(iter) => iter.next().map(|value| (value.timestamp, value.value.into_inner())),
            PointsIterRefInner::Set(iter) => iter.next().map(|value| (value.timestamp, value.value.len() as f64)),
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
