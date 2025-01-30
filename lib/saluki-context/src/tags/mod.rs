//! Metric tags.

use std::{fmt, hash, ops::Deref as _, sync::Arc};

use serde::Serialize;
use stringtheory::MetaString;

mod raw;
pub use self::raw::RawTags;

/// A value containing tags that can be visited.
pub trait Tagged {
    /// Visits the tags in this value.
    fn visit_tags<F>(&self, visitor: F)
    where
        F: FnMut(&Tag);
}

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

    /// Returns a reference to the entire underlying tag string.
    pub fn as_str(&self) -> &str {
        &self.0
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

    /// Returns a borrowed version of the tag.
    pub fn as_borrowed(&self) -> BorrowedTag<'_> {
        BorrowedTag::from(self.0.deref())
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

impl Serialize for Tag {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
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

impl AsRef<str> for Tag {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A borrowed metric tag.
#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct BorrowedTag<'a> {
    raw: &'a str,
    separator: Option<usize>,
}

impl<'a> BorrowedTag<'a> {
    /// Returns `true` if the tag is empty.
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// Returns the length of the tag, in bytes.
    pub fn len(&self) -> usize {
        self.raw.len()
    }

    /// Gets the name of the tag.
    ///
    /// For bare tags (e.g. `production`), this is simply the tag value itself. For key/value-style tags (e.g.
    /// `service:web`), this is the key part of the tag, or `service` based on the example.
    pub fn name(&self) -> &'a str {
        match self.separator {
            Some(idx) => &self.raw[..idx],
            None => self.raw,
        }
    }

    /// Gets the value of the tag.
    ///
    /// For bare tags (e.g. `production`), this always returns `None`. For key/value-style tags (e.g. `service:web`),
    /// this is the value part of the tag, or `web` based on the example.
    pub fn value(&self) -> Option<&'a str> {
        match self.separator {
            Some(idx) => Some(&self.raw[idx + 1..]),
            None => None,
        }
    }

    /// Gets the name and value of the tag.
    ///
    /// For bare tags (e.g. `production`), this always returns `(Some(...), None)`.
    pub fn name_and_value(&self) -> (&'a str, Option<&'a str>) {
        match self.separator {
            Some(idx) => (&self.raw[..idx], Some(&self.raw[idx + 1..])),
            None => (self.raw, None),
        }
    }

    /// Consumes this borrowed tag and returns an owned version of the raw tag.
    pub fn into_string(self) -> MetaString {
        self.raw.into()
    }
}

impl<'a> From<&'a str> for BorrowedTag<'a> {
    fn from(s: &'a str) -> Self {
        let separator = memchr::memchr(b':', s.as_bytes());
        Self { raw: s, separator }
    }
}

impl<'a> AsRef<str> for BorrowedTag<'a> {
    fn as_ref(&self) -> &str {
        self.raw
    }
}

impl<'a> hash::Hash for BorrowedTag<'a> {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}

/// A set of tags.
#[derive(Clone, Debug, Default, Serialize)]
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

    /// Inserts a tag into the set.
    ///
    /// If the tag is already present in the set, this does nothing.
    fn insert_tag_borrowed(&mut self, tag: &Tag) {
        if !self.0.iter().any(|existing| existing == tag) {
            self.0.push(tag.clone());
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

    /// Merges the tags from another shared set into this set.
    ///
    /// If a tag from `other` is already present in this set, it will not be added.
    pub fn merge_missing_shared(&mut self, other: &SharedTagSet) {
        for tag in other {
            self.insert_tag_borrowed(tag);
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

impl Tagged for TagSet {
    fn visit_tags<F>(&self, mut visitor: F)
    where
        F: FnMut(&Tag),
    {
        for tag in &self.0 {
            visitor(tag);
        }
    }
}

impl fmt::Display for TagSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[")?;

        for (i, tag) in self.0.iter().enumerate() {
            if i > 0 {
                write!(f, ",")?;
            }

            write!(f, "{}", tag.as_str())?;
        }

        write!(f, "]")
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

impl fmt::Display for SharedTagSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[")?;

        for (i, tag) in self.0.deref().into_iter().enumerate() {
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
        self.0.deref().serialize(serializer)
    }
}

impl Tagged for SharedTagSet {
    fn visit_tags<F>(&self, visitor: F)
    where
        F: FnMut(&Tag),
    {
        self.0.visit_tags(visitor);
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
