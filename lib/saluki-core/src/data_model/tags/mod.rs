//! Metric tags.

use std::{borrow::Borrow, fmt, hash, ops::Deref};

use serde::{Deserialize, Serialize};
use stringtheory::{CheapMetaString, MetaString};

mod raw;
pub use self::raw::RawTags;

mod tagset;
pub(crate) use self::tagset::TagSetIter;
pub use self::tagset::{SharedTagSet, TagSet};

/// A metric tag.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Tag(MetaString);

impl Tag {
    /// Creates a new, empty tag.
    pub const fn empty() -> Self {
        Self(MetaString::empty())
    }

    /// Creates a new tag from a static string.
    pub const fn from_static(s: &'static str) -> Self {
        Self(MetaString::from_static(s))
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
    /// For bare tags (for example, `production`), this is simply the tag value itself. For key/value-style tags (for example,
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
    /// For bare tags (for example, `production`), this always returns `None`. For key/value-style tags (for example, `service:web`),
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

/// A borrowed metric tag (`name` or `name:value`).
///
/// `TagStr` relates to [`Tag`] as `str` relates to `String`: `Tag` owns its string, and `TagStr` borrows one. A
/// packed context stores its tags as bytes and symbol IDs, not as `Tag` values. It lends each tag as a `&TagStr`, so
/// readers get the tag API without an allocation or a reference count update.
///
/// # Design
///
/// - `TagStr` is `#[repr(transparent)]` over `str`. Thus, [`TagStr::new`] casts a `&str` to a `&TagStr` at no cost.
/// - `Tag` implements `Deref<Target = TagStr>`, so every `TagStr` method works on a `Tag`.
/// - `Tag` implements `Borrow<TagStr>`. `Hash`, `Eq`, and `Ord` match on both types, so a `&TagStr` can look up a
///   `Tag` in a map or set.
/// - [`ToOwned`] turns a `&TagStr` into a `Tag`.
///
/// `TagStr` does not implement `Deref<Target = str>`. Otherwise, every `str` method would also apply to a `Tag`
/// through two derefs, and calls such as `tag.len()` could resolve to a different type than the reader expects. Use
/// [`TagStr::as_str`] or `AsRef<str>` to get the raw string.
#[derive(Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct TagStr(str);

impl TagStr {
    /// Borrows a tag string as a `TagStr`.
    pub const fn new(tag: &str) -> &Self {
        // SAFETY: TagStr is #[repr(transparent)] over str.
        unsafe { &*(tag as *const str as *const Self) }
    }

    /// Returns the complete tag.
    pub const fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the length of this tag in bytes.
    pub const fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns whether this tag is empty.
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the key, or the entire string for a bare tag.
    pub fn name(&self) -> &str {
        self.name_and_value().0
    }

    /// Returns the value of a key/value tag, if any.
    pub fn value(&self) -> Option<&str> {
        self.name_and_value().1
    }

    /// Splits the tag at its first `:` into the key and the optional value.
    pub fn name_and_value(&self) -> (&str, Option<&str>) {
        match memchr::memchr(b':', self.0.as_bytes()) {
            Some(idx) => (&self.0[..idx], Some(&self.0[idx + 1..])),
            None => (&self.0, None),
        }
    }

    /// Returns the value if this is a key/value tag whose key is `name`.
    ///
    /// Compares only the `name:` prefix, so the rest of the tag isn't scanned.
    pub fn value_if_named(&self, name: &str) -> Option<&str> {
        if name.as_bytes().contains(&b':') {
            return None;
        }
        self.0.strip_prefix(name)?.strip_prefix(':')
    }

    /// Returns whether the key of this tag, or the whole bare tag, is `name`.
    pub fn has_name(&self, name: &str) -> bool {
        if name.as_bytes().contains(&b':') {
            return false;
        }
        self.0
            .strip_prefix(name)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(':'))
    }

    /// Returns a view that remembers the position of the key/value separator.
    pub fn as_borrowed(&self) -> BorrowedTag<'_> {
        BorrowedTag::from(&self.0)
    }
}

impl AsRef<str> for TagStr {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for TagStr {
    fn eq(&self, other: &str) -> bool {
        &self.0 == other
    }
}

impl PartialEq<&str> for TagStr {
    fn eq(&self, other: &&str) -> bool {
        &self.0 == *other
    }
}

impl PartialEq<Tag> for TagStr {
    fn eq(&self, other: &Tag) -> bool {
        &self.0 == other.as_str()
    }
}

impl PartialEq<TagStr> for Tag {
    fn eq(&self, other: &TagStr) -> bool {
        self.as_str() == &other.0
    }
}

impl fmt::Display for TagStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl CheapMetaString for TagStr {
    fn try_cheap_clone(&self) -> Option<MetaString> {
        MetaString::try_inline(&self.0)
    }
}

impl From<&TagStr> for MetaString {
    fn from(tag: &TagStr) -> Self {
        tag.as_str().into()
    }
}

impl ToOwned for TagStr {
    type Owned = Tag;

    fn to_owned(&self) -> Tag {
        Tag(self.into())
    }
}

impl Tag {
    /// Borrows this tag as a [`TagStr`].
    pub fn as_tag_str(&self) -> &TagStr {
        TagStr::new(&self.0)
    }
}

impl Deref for Tag {
    type Target = TagStr;

    fn deref(&self) -> &TagStr {
        self.as_tag_str()
    }
}

impl Borrow<TagStr> for Tag {
    fn borrow(&self) -> &TagStr {
        self.as_tag_str()
    }
}

impl AsRef<TagStr> for Tag {
    fn as_ref(&self) -> &TagStr {
        self.as_tag_str()
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
    /// For bare tags (for example, `production`), this is simply the tag value itself. For key/value-style tags (for example,
    /// `service:web`), this is the key part of the tag, or `service` based on the example.
    pub fn name(&self) -> &'a str {
        match self.separator {
            Some(idx) => &self.raw[..idx],
            None => self.raw,
        }
    }

    /// Gets the value of the tag.
    ///
    /// For bare tags (for example, `production`), this always returns `None`. For key/value-style tags (for example, `service:web`),
    /// this is the value part of the tag, or `web` based on the example.
    pub fn value(&self) -> Option<&'a str> {
        match self.separator {
            Some(idx) => Some(&self.raw[idx + 1..]),
            None => None,
        }
    }

    /// Gets the name and value of the tag.
    ///
    /// For bare tags (for example, `production`), this always returns `(Some(...), None)`.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn borrowed_tag_splits_name_and_value() {
        // (raw, expected name, expected value)
        let cases: &[(&str, &str, Option<&str>)] = &[
            // Key/value tag: split on the first colon.
            ("service:web", "service", Some("web")),
            // Bare tag: the whole string is the name, and there is no value.
            ("production", "production", None),
            // Empty value: the value is present but empty.
            ("key:", "key", Some("")),
            // Multiple colons: only the first colon splits; the rest stays in the value.
            ("a:b:c", "a", Some("b:c")),
            // Leading colon: empty name, value is the remainder.
            (":foo", "", Some("foo")),
            // Just a colon: empty name and empty value.
            (":", "", Some("")),
            // Empty string: empty name, no value.
            ("", "", None),
        ];

        for (raw, name, value) in cases {
            let borrowed = BorrowedTag::from(*raw);
            assert_eq!(borrowed.name(), *name, "name for {raw:?}");
            assert_eq!(borrowed.value(), *value, "value for {raw:?}");
            assert_eq!(borrowed.name_and_value(), (*name, *value), "name_and_value for {raw:?}");

            // `Tag` re-implements the same split independently (via `split_once`), so the two must
            // agree on every input.
            let owned = Tag::from(*raw);
            assert_eq!(owned.name(), *name, "Tag::name for {raw:?}");
            assert_eq!(owned.value(), *value, "Tag::value for {raw:?}");
        }
    }

    #[test]
    fn borrowed_tag_is_empty_and_len() {
        let empty = BorrowedTag::from("");
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);

        let tag = BorrowedTag::from("service:web");
        assert!(!tag.is_empty());
        assert_eq!(tag.len(), "service:web".len());
    }

    #[test]
    fn tag_str_splits_name_and_value() {
        // (raw, expected name, expected value)
        let cases: &[(&str, &str, Option<&str>)] = &[
            ("service:web", "service", Some("web")),
            ("production", "production", None),
            ("key:", "key", Some("")),
            ("a:b:c", "a", Some("b:c")),
            (":foo", "", Some("foo")),
            (":", "", Some("")),
            ("", "", None),
        ];

        for (raw, name, value) in cases {
            let tag = TagStr::new(raw);
            assert_eq!(tag.name_and_value(), (*name, *value), "name_and_value for {raw:?}");
            assert_eq!(tag.name(), *name, "name for {raw:?}");
            assert_eq!(tag.value(), *value, "value for {raw:?}");
            assert!(tag.has_name(name), "has_name for {raw:?}");
            assert_eq!(tag.value_if_named(name), *value, "value_if_named for {raw:?}");
            assert_eq!(tag.as_borrowed().name_and_value(), (*name, *value));
        }
    }

    #[test]
    fn tag_str_matches_names_exactly() {
        let tag = TagStr::new("device:sda1");
        assert_eq!(tag.value_if_named("device"), Some("sda1"));
        assert_eq!(tag.value_if_named("dev"), None);
        assert_eq!(tag.value_if_named("device:sda1"), None);
        assert!(tag.has_name("device"));
        assert!(!tag.has_name("dev"));
        assert!(!tag.has_name("devices"));
        assert!(!tag.has_name("device:sda1"));

        let bare = TagStr::new("device");
        assert_eq!(bare.value_if_named("device"), None);
        assert!(bare.has_name("device"));

        let nested = TagStr::new("a:b:c");
        assert_eq!(nested.value_if_named("a"), Some("b:c"));
        assert_eq!(nested.value_if_named("a:b"), None);
        assert!(!nested.has_name("a:b"));

        assert!(TagStr::new("").has_name(""));
        assert_eq!(TagStr::new("").value_if_named(""), None);
        assert!(TagStr::new(":foo").has_name(""));
        assert_eq!(TagStr::new(":foo").value_if_named(""), Some("foo"));
        assert!(!TagStr::new("foo").has_name(""));
        assert_eq!(TagStr::new("key:").value_if_named("key"), Some(""));
    }

    #[test]
    fn tag_derefs_and_borrows_as_tag_str() {
        use std::collections::HashSet;

        let tag = Tag::from("service:web");
        let tag_str: &TagStr = &tag;
        assert_eq!(tag_str, TagStr::new("service:web"));
        assert_eq!(*tag_str, tag);
        assert_eq!(tag, *tag_str);
        assert_eq!(tag_str, "service:web");
        assert_eq!(tag_str.to_owned(), tag);
        assert_eq!(MetaString::from(tag_str), "service:web");
        assert_eq!(tag_str.to_string(), "service:web");

        let set: HashSet<Tag> = [Tag::from("env:prod"), tag.clone()].into_iter().collect();
        assert!(set.contains(TagStr::new("service:web")));
        assert!(set.contains(TagStr::new("env:prod")));
        assert!(!set.contains(TagStr::new("service")));
    }

    #[test]
    fn tag_str_deduplicates_borrowed_tags() {
        use saluki_common::iter::ReusableDeduplicator;

        let owned = [Tag::from("a:1"), Tag::from("b:2")];
        let tags = [
            TagStr::new("a:1"),
            owned[1].as_tag_str(),
            TagStr::new("a:1"),
            owned[0].as_tag_str(),
            TagStr::new("c"),
        ];
        let mut deduplicator = ReusableDeduplicator::<TagStr>::new();
        let unique: Vec<&str> = deduplicator
            .deduplicated(tags.into_iter())
            .map(TagStr::as_str)
            .collect();
        assert_eq!(unique, ["a:1", "b:2", "c"]);
    }

    #[test]
    fn tag_display_renders_full_tag_string() {
        assert_eq!(Tag::from("service:web").to_string(), "service:web");
        assert_eq!(Tag::from("bare").to_string(), "bare");
    }
}
