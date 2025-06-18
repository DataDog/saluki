//! Metric tags.

use std::{fmt, hash, ops::Deref as _};

use serde::Serialize;
use stringtheory::{CheapMetaString, MetaString};

mod raw;
pub use self::raw::{RawTags, RawTagsFilter, RawTagsFilterPredicate};

mod tagset;
pub use self::tagset::{SharedTagSet, TagSet};

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

impl CheapMetaString for Tag {
    fn try_cheap_clone(&self) -> Option<MetaString> {
        if self.0.is_cheaply_cloneable() {
            Some(self.0.clone())
        } else {
            None
        }
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

impl AsRef<str> for BorrowedTag<'_> {
    fn as_ref(&self) -> &str {
        self.raw
    }
}

impl hash::Hash for BorrowedTag<'_> {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}
