//! Sharing-optimized strings and string interning utilities.
//!
//! `stringtheory` provides two main components: a sharing-optimized string type, `MetaString`, and a string interning
//! implementation, `FixedSizeInterner`. These compponents are meant to work in concert, allowing for using a single
//! string type that can handle owned, shared, and interned strings, and providing a way to efficiently intern strings
//! strings when possible.
#![deny(warnings)]
#![deny(missing_docs)]

use std::{borrow::Borrow, fmt, hash, ops::Deref};

pub mod interning;
use self::interning::InternedString;

#[derive(Clone)]
enum Inner {
    /// An owned string.
    Owned(String),

    /// A static string reference.
    Static(&'static str),

    /// An interned string.
    Interned(InternedString),

    /// An inlined string.
    Inlined(InlinedString),
}

/// A string type that abstracts over various forms of string storage.
///
/// Normally, developers will work with either `String` (owned) or `&str` (borrowed) when dealing with strings. In some
/// cases, though, it can be useful to work with strings that use alternative storage, such as those that are atomically
/// shared (e.g. `Arc<str>`). While using those string types themselves isn't complex, using them and _also_ supporting
/// normal string types can be complex.
///
/// `MetaString` is an opinionated string type that abstracts over the normal string types like `String` and `&str`
/// while also supporting alternative storage, such as interned strings from `FixedSizeInterner`.
///
/// ## Supported types
///
/// `MetaString` supports the following "modes":
///
/// - owned (`String`)
/// - static (`&'static str`)
/// - interned (`InternedString`)
/// - inlined (up to 31 bytes)
///
/// ### Owned and borrowed strings
///
/// `MetaString` can be created from `String` and `&str` directly. For owned scenarios (`String`), the string value is
/// simply wrapped. For borrowed strings, they are copied into a newly-allocated storage buffer, after which the
/// resulting `MetaString` can be cheaply cloned and shared.
///
/// ### Static strings
///
/// `MetaString` can be created from `&'static str` directly. This is useful for string literals and other static
/// strings that are too large to be inlined.
///
/// ### Interned strings
///
/// `MetaString` can also be created from `InternedString`, which is a string that has been interned using
/// `FixedSizeInterner.` Interned strings are essentially a combination of the properties of `Arc<T>` -- owned values
/// that atomically track the reference count to a shared piece of data -- and a fixed-size buffer, where we allocate
/// one large buffer, and write many small strings into it, and provide references to those strings through
/// `InternedString`.
///
/// ### Inlined strings
///
/// Finally, `MetaString` can also be created by inlining small strings into `MetaString` itself, avoiding the need for
/// any backing allocation. "Small string optimization" is a common optimization for string types where small strings
/// can be stored directly in the string type itself by utilizing a "union"-style layout.
///
/// As `MetaString` is represented by multiple possible storage types, an internal enum is utilized to distinguish these
/// possible types. Enums are as large as the largest variant they contain, which means that an additional variant can
/// be added that is as large as the largest variant without increasing the size of the enum further. We do this, and we
/// simply store the string bytes directly in an inline array.
///
/// Essentially, we can store a string that is up to 31 bytes directly in `MetaString`.
///
/// ## Conversion methods
///
/// Implementations of `From<T>` exist for all of the aforementioned types to allow for easily converting to
/// `MetaString`. Once a caller has a `MetaString` value, they are generally expected to interact with the string in a
/// read-only way, as `MetaString` can be dereferenced directly to `&str`.
///
/// If a caller needs to be able to modify the string data, they can call `into_owned` to get an owned version of the
/// string, make their modifications to the owned version, and then convert that back to `MetaString`.
#[derive(Clone)]
pub struct MetaString {
    inner: Inner,
}

impl MetaString {
    /// Creates an empty `MetaString`.
    ///
    /// This does not allocate.
    pub const fn empty() -> Self {
        Self {
            inner: Inner::Owned(String::new()),
        }
    }

    /// Creates a new `MetaString` from the given static string.
    ///
    /// This does not allocate.
    pub fn from_static(s: &'static str) -> Self {
        Self {
            inner: Inner::Static(s),
        }
    }

    /// Attempts to create a new `MetaString` from the given string if it can be inlined.
    pub fn try_inline(s: &str) -> Option<Self> {
        InlinedString::new(s).map(|i| Self {
            inner: Inner::Inlined(i),
        })
    }

    /// Consumes `self` and returns an owned `String`.
    ///
    /// If the `MetaString` is already owned, this will simply return the inner `String` directly. Otherwise, this will
    /// allocate an owned version (`String`) of the string data.
    pub fn into_owned(self) -> String {
        match self.inner {
            Inner::Owned(s) => s,
            Inner::Static(s) => s.to_owned(),
            Inner::Interned(i) => (*i).to_owned(),
            Inner::Inlined(i) => i.deref().to_owned(),
        }
    }
}

impl Default for MetaString {
    fn default() -> Self {
        Self::empty()
    }
}

impl hash::Hash for MetaString {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.deref().hash(state)
    }
}

impl PartialEq<str> for MetaString {
    fn eq(&self, other: &str) -> bool {
        self.deref() == other
    }
}

impl PartialEq<MetaString> for MetaString {
    fn eq(&self, other: &MetaString) -> bool {
        self.deref() == other.deref()
    }
}

impl Eq for MetaString {}

impl PartialOrd for MetaString {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MetaString {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.deref().cmp(other.deref())
    }
}

impl Borrow<str> for MetaString {
    fn borrow(&self) -> &str {
        self.deref()
    }
}

impl From<String> for MetaString {
    fn from(s: String) -> Self {
        MetaString { inner: Inner::Owned(s) }
    }
}

impl From<&str> for MetaString {
    fn from(s: &str) -> Self {
        match InlinedString::new(s) {
            Some(i) => Self {
                inner: Inner::Inlined(i),
            },
            None => Self {
                inner: Inner::Owned(s.to_owned()),
            },
        }
    }
}

impl From<InternedString> for MetaString {
    fn from(s: InternedString) -> Self {
        Self {
            inner: Inner::Interned(s),
        }
    }
}

impl From<MetaString> for protobuf::Chars {
    fn from(value: MetaString) -> Self {
        match value.inner {
            Inner::Owned(s) => s.into(),
            Inner::Static(s) => s.into(),
            Inner::Interned(i) => i.deref().into(),
            Inner::Inlined(i) => i.deref().into(),
        }
    }
}

impl<'a> From<&'a MetaString> for protobuf::Chars {
    fn from(value: &'a MetaString) -> Self {
        match &value.inner {
            Inner::Owned(s) => s.as_str().into(),
            Inner::Static(s) => protobuf::Chars::from_bytes(bytes::Bytes::from_static(s.as_bytes()))
                .expect("should never fail to convert existing string"),
            Inner::Interned(i) => i.deref().into(),
            Inner::Inlined(i) => i.deref().into(),
        }
    }
}

impl Deref for MetaString {
    type Target = str;

    fn deref(&self) -> &str {
        match &self.inner {
            Inner::Owned(s) => s,
            Inner::Static(s) => s,
            Inner::Interned(i) => i,
            Inner::Inlined(i) => i.deref(),
        }
    }
}

impl fmt::Debug for MetaString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.deref().fmt(f)
    }
}

impl fmt::Display for MetaString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.deref().fmt(f)
    }
}

/// A string that can be inlined up to 23 bytes.
#[derive(Clone)]
struct InlinedString {
    data: [u8; 24],
}

impl InlinedString {
    fn new(s: &str) -> Option<Self> {
        let s_len = s.len();
        if s_len > 23 {
            return None;
        }

        // SAFETY: We know it fits because we just checked that the string length is 23 or less.
        let s_len_b = s_len as u8;

        let mut data = [0; 24];
        data[0] = s_len_b;

        let s_buf = s.as_bytes();
        data[1..s_len + 1].copy_from_slice(s_buf);

        Some(Self { data })
    }
}

impl Deref for InlinedString {
    type Target = str;

    fn deref(&self) -> &str {
        let len = self.data[0] as usize;
        let s = &self.data[1..len + 1];

        // SAFETY: We know the data is valid UTF-8 because we only ever write valid UTF-8 data into the buffer.
        unsafe { std::str::from_utf8_unchecked(s) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_layout() {
        assert_eq!(std::mem::size_of::<MetaString>(), 32);
    }

    #[test]
    fn inlined_string() {
        // We expect this to hold because we need `InlinedString` to be one word less (usize, 8 bytes on 64-bit
        // platforms) than `MetaString` to ensure it isn't causing `MetaString` to be larger than it should be, but also
        // that it's maximizing the potential inlining capacity.
        assert_eq!(
            std::mem::size_of::<InlinedString>(),
            std::mem::size_of::<MetaString>() - std::mem::size_of::<usize>()
        );
    }

    #[test]
    fn try_inline() {
        // String is < 31 bytes (13 bytes), so it should be inline-able:
        let s = "hello, world!";
        assert_eq!(s.len(), 13);

        let ms = MetaString::try_inline(s).unwrap();
        assert_eq!(s, ms.deref());

        // String is < 31 bytes (39 bytes), so it shouldn't be inlined:
        let s = "hello, world!hello, world!hello, world!";
        assert_eq!(s.len(), 39);

        let ms = MetaString::try_inline(s);
        assert!(ms.is_none());
    }
}
