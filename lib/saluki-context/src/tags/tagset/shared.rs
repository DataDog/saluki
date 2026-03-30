use std::{fmt, ops::Deref as _, sync::Arc};

use serde::{ser::SerializeSeq as _, Serialize};
use smallvec::SmallVec;

use super::{frozen::FrozenTagSet, TagSet};
use crate::tags::Tag;

/// A shared, read-only set of tags.
///
/// # Structural sharing
///
/// In many cases, it is useful to extend a set of tags with additional tags, without needing to clone the additional
/// tags or re-allocate the underlying storage to fit the entire set of tags. `SharedTagSet` supports this by utilizing
/// "structural sharing", where `SharedTagSet` is internally represented by a set of smart pointers to `FrozenTagSet`.
///
/// This allows `SharedTagSet` to be cheaply extended with additional `SharedTagSet` instances, without needing to
/// allocate enough underlying storage to hold all of the individual tags. Extending a `SharedTagSet` will allocate a
/// small amount of memory (8 bytes) for each additional `SharedTagSet` that is chained after the first additional one:
/// this means that all new `SharedTagSet` instances can be extended once with no allocations whatsoever.
#[derive(Clone, Debug, Default)]
pub struct SharedTagSet(SmallVec<[Arc<FrozenTagSet>; 2]>);

impl SharedTagSet {
    /// Returns `true` if the tag set is empty.
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|ts| ts.is_empty())
    }

    /// Returns the number of tags in the set.
    pub fn len(&self) -> usize {
        self.0.iter().map(|ts| ts.len()).sum()
    }

    /// Returns `true` if the given tag is contained in the set.
    ///
    /// This matches the complete tag, rather than just the name.
    pub fn has_tag<T>(&self, tag: T) -> bool
    where
        T: AsRef<str>,
    {
        self.0.iter().any(|ts| ts.has_tag(tag.as_ref()))
    }

    /// Gets a single tag, by name, from the set.
    ///
    /// If multiple tags are present with the same name, the first tag with a matching name will be returned. If no tag
    /// in the set matches, `None` is returned.
    pub fn get_single_tag<T>(&self, tag_name: T) -> Option<&Tag>
    where
        T: AsRef<str>,
    {
        self.0.iter().find_map(|ts| ts.get_single_tag(tag_name.as_ref()))
    }

    /// Extends `self` with the tags from the `other`.
    ///
    /// If any of the individual `FrozenTagSet` instances in `other` are already present in `self`, they will not be added
    /// again. This method does not avoid duplicates across different `SharedTagSet` instances, so if the same tag is
    /// present in both `self` and `other`, it will be present when querying the resulting `SharedTagSet`.
    pub fn extend_from_shared(&mut self, other: &SharedTagSet) {
        // For each underlying `FrozenTagSet` in the other `SharedTagSet`, check if it is already present in this one, and if
        // not, add it.
        for tag_set in &other.0 {
            if !self.0.iter().any(|ts| Arc::ptr_eq(ts, tag_set)) {
                self.0.push(Arc::clone(tag_set));
            }
        }
    }

    /// Creates a `SharedTagSet` from a vector of tags.
    pub(super) fn from_tags(tags: Vec<Tag>) -> Self {
        if tags.is_empty() {
            return Self::default();
        }
        let mut inner = SmallVec::new();
        inner.push(Arc::new(FrozenTagSet::new(tags)));
        Self(inner)
    }

    /// Gets a tag by its flattened index across all chained tag sets.
    pub(super) fn get_by_flat_index(&self, index: usize) -> Option<&Tag> {
        let mut remaining = index;
        for ts in &self.0 {
            let len = ts.len();
            if remaining < len {
                return ts.into_iter().nth(remaining);
            }
            remaining -= len;
        }
        None
    }

    /// Creates a mutable `TagSet` from this `SharedTagSet`.
    ///
    /// This clones the `SharedTagSet` (cheap — just Arc pointer copies) and wraps it as the base
    /// of a new `TagSet` that can be mutated.
    pub fn to_mutable(&self) -> TagSet {
        TagSet::from(self.clone())
    }

    /// Returns the size of the tag set, in bytes.
    ///
    /// This includes the size of the vector holding any chained tagsets as well as each individual tag.
    ///
    /// Additionally, the value returned by this method does not compensate for externalities such as whether or not
    /// tags are inlined, interned, or heap allocated. This means that the value returned is essentially the
    /// worst-case usage, and should be used as a rough estimate.
    pub fn size_of(&self) -> usize {
        // Calculate the size of the SharedTagSet, which includes the size of the SmallVec and the size of each Arc.
        (self.0.len() * std::mem::size_of::<Arc<FrozenTagSet>>()) + self.0.iter().map(|ts| ts.size_of()).sum::<usize>()
    }
}

impl From<TagSet> for SharedTagSet {
    fn from(tag_set: TagSet) -> Self {
        tag_set.into_shared()
    }
}

impl PartialEq<TagSet> for SharedTagSet {
    fn eq(&self, other: &TagSet) -> bool {
        // NOTE: We could try storing tags in sorted order internally, which would make this moot... but for now, we'll
        // avoid the sort (which lets us avoid an allocation) and just do the naive per-item comparison.

        if self.len() != other.len() {
            return false;
        }

        let self_tags = self.into_iter();
        let other_tags = other.into_iter();
        for (self_tag, other_tag) in self_tags.zip(other_tags) {
            if self_tag != other_tag {
                return false;
            }
        }

        true
    }
}

impl PartialEq<SharedTagSet> for SharedTagSet {
    fn eq(&self, other: &SharedTagSet) -> bool {
        // NOTE: We could try storing tags in sorted order internally, which would make this moot... but for now, we'll
        // avoid the sort (which lets us avoid an allocation) and just do the naive per-item comparison.

        if self.len() != other.len() {
            return false;
        }

        let self_tags = self.into_iter();
        let other_tags = other.into_iter();
        for (self_tag, other_tag) in self_tags.zip(other_tags) {
            if self_tag != other_tag {
                return false;
            }
        }

        true
    }
}

impl Eq for SharedTagSet {}

impl std::hash::Hash for SharedTagSet {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        for tag in self {
            tag.hash(state);
        }
    }
}

impl fmt::Display for SharedTagSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[")?;

        for (i, tag) in self.0.iter().flat_map(|ts| ts.deref().into_iter()).enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }

            write!(f, "{}", tag.as_str())?;
        }

        write!(f, "]")
    }
}

impl Serialize for SharedTagSet {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(self.len()))?;
        for tag in self.0.iter().flat_map(|ts| ts.deref().into_iter()) {
            seq.serialize_element(tag)?;
        }
        seq.end()
    }
}

impl<'a> IntoIterator for &'a SharedTagSet {
    type Item = &'a Tag;
    type IntoIter = SharedTagSetIterator<'a>;

    fn into_iter(self) -> Self::IntoIter {
        SharedTagSetIterator {
            inner: self.0.iter(),
            current: None,
        }
    }
}

impl FromIterator<Tag> for SharedTagSet {
    fn from_iter<T: IntoIterator<Item = Tag>>(iter: T) -> Self {
        TagSet::from_iter(iter).into_shared()
    }
}

/// Iterator over the tags in a `SharedTagSet`.
#[derive(Clone)]
pub struct SharedTagSetIterator<'a> {
    inner: std::slice::Iter<'a, Arc<FrozenTagSet>>,
    current: Option<std::slice::Iter<'a, Tag>>,
}

impl<'a> Iterator for SharedTagSetIterator<'a> {
    type Item = &'a Tag;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(iter) = &mut self.current {
                if let Some(tag) = iter.next() {
                    return Some(tag);
                }
            }

            if let Some(next_set) = self.inner.next() {
                self.current = Some(next_set.deref().into_iter());
            } else {
                return None;
            }
        }
    }
}
