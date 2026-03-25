use std::fmt;

use crate::tags::Tag;

/// Flat, immutable tag storage.
///
/// This is a private type used as the backing storage inside `SharedTagSet`. It holds a simple
/// `Vec<Tag>` and provides read-only access. External callers work with `TagSet` (mutable) and
/// `SharedTagSet` (shared/frozen) instead.
#[derive(Clone, Debug, Default)]
pub(crate) struct FrozenTagSet(Vec<Tag>);

impl FrozenTagSet {
    /// Creates a new `FrozenTagSet` from the given vector of tags.
    pub(crate) fn new(tags: Vec<Tag>) -> Self {
        Self(tags)
    }

    /// Returns `true` if the tag set is empty.
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the number of tags in the set.
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` if the given tag is contained in the set.
    ///
    /// This matches the complete tag, rather than just the name.
    pub(crate) fn has_tag(&self, tag: &str) -> bool {
        self.0.iter().any(|existing| existing.as_str() == tag)
    }

    /// Gets a single tag, by name, from the set.
    ///
    /// If multiple tags are present with the same name, the first tag with a matching name will be
    /// returned. If no tag in the set matches, `None` is returned.
    pub(crate) fn get_single_tag(&self, tag_name: &str) -> Option<&Tag> {
        self.0.iter().find(|tag| tag.name() == tag_name)
    }

    /// Returns the size of the tag set, in bytes.
    pub(crate) fn size_of(&self) -> usize {
        (self.len() * std::mem::size_of::<Tag>()) + self.0.iter().map(|tag| tag.len()).sum::<usize>()
    }
}

impl<'a> IntoIterator for &'a FrozenTagSet {
    type Item = &'a Tag;
    type IntoIter = std::slice::Iter<'a, Tag>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl FromIterator<Tag> for FrozenTagSet {
    fn from_iter<I: IntoIterator<Item = Tag>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl From<Tag> for FrozenTagSet {
    fn from(tag: Tag) -> Self {
        Self(vec![tag])
    }
}

impl fmt::Display for FrozenTagSet {
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
