use std::{fmt, hash, ops::Deref as _};

use stringtheory::MetaString;

/// A metric tag.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Tag(MetaString);

impl Tag {
    /// Creates a new, empty tag.
    pub const fn empty() -> Self {
        Self(MetaString::empty())
    }

    /// Returns `true` if the tag is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the length of the tag, in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Gets the name of the tag.
    ///
    /// For bare tags (e.g. `production`), this is simply the tag value itself. For key/value-style tags (e.g.
    /// `service:web`), this is the key part of the tag, or `service` based on the example.
    pub fn name(&self) -> &str {
        let s = self.0.deref();
        match s.split_once(':') {
            Some((name, _)) => name,
            None => s,
        }
    }

    /// Gets the value of the tag.
    ///
    /// For bare tags (e.g. `production`), this always returns `None`. For key/value-style tags (e.g. `service:web`),
    /// this is the value part of the tag, or `web` based on the example.
    pub fn value(&self) -> Option<&str> {
        self.0.deref().split_once(':').map(|(_, value)| value)
    }

    /// Consumes the tag and returns the inner `MetaString`.
    pub fn into_inner(self) -> MetaString {
        self.0
    }
}

impl PartialEq<str> for Tag {
    fn eq(&self, other: &str) -> bool {
        self.0.deref() == other
    }
}

impl hash::Hash for Tag {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl<T> From<T> for Tag
where
    T: Into<MetaString>,
{
    fn from(s: T) -> Self {
        Self(s.into())
    }
}

/// A borrowed metric tag.
#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct BorrowedTag<'a>(&'a str);

impl<'a> BorrowedTag<'a> {
    /// Returns `true` if the tag is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the length of the tag, in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Gets the name of the tag.
    ///
    /// For bare tags (e.g. `production`), this is simply the tag value itself. For key/value-style tags (e.g.
    /// `service:web`), this is the key part of the tag, or `service` based on the example.
    pub fn name(&self) -> &'a str {
        match self.0.split_once(':') {
            Some((name, _)) => name,
            None => self.0,
        }
    }

    /// Gets the value of the tag.
    ///
    /// For bare tags (e.g. `production`), this always returns `None`. For key/value-style tags (e.g. `service:web`),
    /// this is the value part of the tag, or `web` based on the example.
    pub fn value(&self) -> Option<&'a str> {
        self.0.split_once(':').map(|(_, value)| value)
    }

    /// Gets the name and value of the tag.
    ///
    /// For bare tags (e.g. `production`), this always returns `(Some(...), None)`.
    pub fn name_and_value(&self) -> (&'a str, Option<&'a str>) {
        match self.0.split_once(':') {
            Some((name, value)) => (name, Some(value)),
            None => (self.0, None),
        }
    }
}

impl<'a> From<&'a str> for BorrowedTag<'a> {
    fn from(s: &'a str) -> Self {
        Self(s)
    }
}

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

    /// Returns a sorted version of the tag set.
    pub fn as_sorted(&self) -> Self {
        let mut tags = self.0.clone();
        tags.sort_unstable();
        Self(tags)
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
