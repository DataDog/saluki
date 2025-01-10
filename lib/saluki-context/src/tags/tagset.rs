use std::{ops::Deref as _, sync::Arc};

use super::Tag;

/// A set of tags.
#[derive(Clone, Debug, Default)]
pub struct TagSet(Vec<Tag>);

impl TagSet {
    /// Creates a new, empty tag set with the given capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self(Vec::with_capacity(capacity))
    }

    /// Returns `true` if the tag set is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the number of tags in the set.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Inserts a tag into the set.
    ///
    /// If the tag is already present in the set, this does nothing.
    pub fn insert_tag<T>(&mut self, tag: T)
    where
        T: Into<Tag>,
    {
        let tag = tag.into();
        if !self.0.iter().any(|existing| existing == &tag) {
            self.0.push(tag);
        }
    }

    /// Removes a tag, by name, from the set.
    pub fn remove_tags<T>(&mut self, tag_name: T) -> Option<Vec<Tag>>
    where
        T: AsRef<str>,
    {
        let tag_name = tag_name.as_ref();

        let mut tags = Vec::new();
        let mut idx = 0;
        while idx < self.0.len() {
            if tag_has_name(&self.0[idx], tag_name) {
                tags.push(self.0.swap_remove(idx));
            } else {
                idx += 1;
            }
        }

        if tags.is_empty() {
            None
        } else {
            Some(tags)
        }
    }

    /// Returns `true` if the given tag is contained in the set.
    ///
    /// This matches the complete tag, rather than just the name.
    pub fn has_tag<T>(&self, tag: T) -> bool
    where
        T: AsRef<str>,
    {
        let tag = tag.as_ref();
        self.0.iter().any(|existing| existing.0.as_ref() == tag)
    }

    /// Gets a single tag, by name, from the set.
    ///
    /// If multiple tags are present with the same name, the first tag with a matching name will be returned. If no tag
    /// in the set matches, `None` is returned.
    pub fn get_single_tag<T>(&self, tag_name: T) -> Option<&Tag>
    where
        T: AsRef<str>,
    {
        let tag_name = tag_name.as_ref();
        self.0.iter().find(|tag| tag_has_name(tag, tag_name))
    }

    /// Retains only the tags specified by the predicate.
    ///
    /// In other words, remove all tags `t` for which `f(&t)` returns `false`. This method operates in place, visiting
    /// each element exactly once in the original order, and preserves the order of the retained tags.
    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&Tag) -> bool,
    {
        self.0.retain(|tag| f(tag));
    }

    /// Merges the tags from another set into this set.
    ///
    /// If a tag from `other` is already present in this set, it will not be added.
    pub fn merge_missing(&mut self, other: Self) {
        for tag in other.0 {
            self.insert_tag(tag);
        }
    }

    /// Consumes this `TagSet` and returns a shared, read-only version of it.
    pub fn into_shared(self) -> SharedTagSet {
        SharedTagSet(Arc::new(self))
    }
}

impl PartialEq<TagSet> for TagSet {
    fn eq(&self, other: &TagSet) -> bool {
        // NOTE: We could try storing tags in sorted order internally, which would make this moot... but for now, we'll
        // avoid the sort (which lets us avoid an allocation) and just do the naive per-item comparison.
        if self.0.len() != other.0.len() {
            return false;
        }

        for other_tag in &other.0 {
            if !self.0.iter().any(|tag| tag == other_tag) {
                return false;
            }
        }

        true
    }
}

impl IntoIterator for TagSet {
    type Item = Tag;
    type IntoIter = std::vec::IntoIter<Tag>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a TagSet {
    type Item = &'a Tag;
    type IntoIter = std::slice::Iter<'a, Tag>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl FromIterator<Tag> for TagSet {
    fn from_iter<I: IntoIterator<Item = Tag>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl Extend<Tag> for TagSet {
    fn extend<T: IntoIterator<Item = Tag>>(&mut self, iter: T) {
        self.0.extend(iter)
    }
}

impl From<Tag> for TagSet {
    fn from(tag: Tag) -> Self {
        Self(vec![tag])
    }
}

/// A shared, read-only set of tags.
#[derive(Clone, Debug)]
pub struct SharedTagSet(Arc<TagSet>);

impl SharedTagSet {
    /// Returns `true` if the tag set is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the number of tags in the set.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` if the given tag is contained in the set.
    ///
    /// This matches the complete tag, rather than just the name.
    pub fn has_tag<T>(&self, tag: T) -> bool
    where
        T: AsRef<str>,
    {
        self.0.has_tag(tag)
    }

    /// Gets a single tag, by name, from the set.
    ///
    /// If multiple tags are present with the same name, the first tag with a matching name will be returned. If no tag
    /// in the set matches, `None` is returned.
    pub fn get_single_tag<T>(&self, tag_name: T) -> Option<&Tag>
    where
        T: AsRef<str>,
    {
        self.0.get_single_tag(tag_name)
    }
}

impl PartialEq<TagSet> for SharedTagSet {
    fn eq(&self, other: &TagSet) -> bool {
        // NOTE: We could try storing tags in sorted order internally, which would make this moot... but for now, we'll
        // avoid the sort (which lets us avoid an allocation) and just do the naive per-item comparison.
        if self.0.len() != other.len() {
            return false;
        }

        for other_tag in other {
            if !self.0.deref().into_iter().any(|tag| tag == other_tag) {
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
        if self.0.len() != other.0.len() {
            return false;
        }

        for other_tag in other.0.deref() {
            if !self.0.deref().into_iter().any(|tag| tag == other_tag) {
                return false;
            }
        }

        true
    }
}

impl<'a> IntoIterator for &'a SharedTagSet {
    type Item = &'a Tag;
    type IntoIter = std::slice::Iter<'a, Tag>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.deref().into_iter()
    }
}

fn tag_has_name(tag: &Tag, tag_name: &str) -> bool {
    // Try matching it as a key-value pair (e.g., `env:production`) first, and then just try matching it as a bare tag
    // (e.g., `production`).
    let tag_str = tag.0.deref();
    tag_str
        .split_once(':')
        .map_or_else(|| tag_str == tag_name, |(name, _)| name == tag_name)
}
