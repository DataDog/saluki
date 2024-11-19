use std::{collections::HashSet, num::NonZeroU64};

use ordered_float::OrderedFloat;

use super::TimestampedValue;

enum PointsIterInner {
    Scalar(smallvec::IntoIter<[TimestampedValue<OrderedFloat<f64>>; 4]>),
    Set(smallvec::IntoIter<[TimestampedValue<HashSet<String>>; 1]>),
}

pub struct PointsIter {
    inner: PointsIterInner,
}

impl PointsIter {
    pub(super) fn scalar(values: smallvec::IntoIter<[TimestampedValue<OrderedFloat<f64>>; 4]>) -> Self {
        Self {
            inner: PointsIterInner::Scalar(values),
        }
    }

    pub(super) fn set(values: smallvec::IntoIter<[TimestampedValue<HashSet<String>>; 1]>) -> Self {
        Self {
            inner: PointsIterInner::Set(values),
        }
    }
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

impl<'a> PointsIterRef<'a> {
    pub(super) fn scalar(values: std::slice::Iter<'a, TimestampedValue<OrderedFloat<f64>>>) -> Self {
        Self {
            inner: PointsIterRefInner::Scalar(values),
        }
    }

    pub(super) fn set(values: std::slice::Iter<'a, TimestampedValue<HashSet<String>>>) -> Self {
        Self {
            inner: PointsIterRefInner::Set(values),
        }
    }
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
