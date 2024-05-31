use std::{borrow::Borrow, fmt, hash, ops::Deref};

pub mod interning;
use self::interning::InternedString;

#[derive(Clone)]
enum Inner {
    /// An owned string.
    Owned(String),

    /// A shared string.
    ///
    /// Validation that the bytes are UTF-8 happens during conversion to `MetaString`.
    Shared(bytes::Bytes),

    /// A shared string from `protobuf::Chars`.
    ///
    /// This is currently a hack, as `Chars` itself is simply a newtype around `Bytes`, but has no way to extract the
    /// inner `Bytes`. In the future, hopefully we can upstream support to do so, but for now, we'll do it this way.
    ProtoShared(protobuf::Chars),

    /// An interned string.
    Interned(InternedString),
}

/// A string type that abstracts over various forms of string storage.
///
/// Normally, developers will work with either `String` (owned) or `&str` (borrowed) when dealing with strings. In some
/// cases, though, it can be useful to work with strings that use alternative storage, such as those that are atomically
/// shared (e.g. `Arc<str>`). While using those string types themselves isn't complex, using them and _also_ supporting
/// normal string types can be complex.
///
/// `MetaString` is an opinionated string type that abstracts over the normal string types like `String` and `&str`
/// while also supporting alternative storage, such as string data backed by `bytes::Bytes`, or interned using
/// `FixedSizeInterner`.
///
/// ## Supported types
///
/// `MetaString` supports the following "modes":
///
/// - owned (`String`)
/// - shared (`bytes::Bytes`)
/// - `protobuf`-specific shared (`protobuf::Chars`)
/// - interned (`InternedString`)
///
/// ### Owned and borrowed strings
///
/// `MetaString` can be created from `String` and `&str` directly. For owned scenarios (`String`), the string value is
/// simply wrapped. For borrowed strings, they are copied into a newly-allocated storage buffer, after which the
/// resulting `MetaString` can be cheaply cloned and shared.
///
/// ### Shared strings
///
/// `MetaString` can be created from `bytes::Bytes` or `protobuf::Chars`, which are both shared byte buffers that use
/// atomics -- similar to `Arc<T>` -- to allow for cheaply cloning references to the same piece of data.
///
/// We handle the `protobuf`-specific `Chars` type as an optimization: while it is based on `bytes::Bytes`, it cannot be
/// trivially converted back-and-forth as the underlying bytes must be checked for UTF-8 validity. By handling
/// `protobuf::Chars` separately, we can avoid this revalidation of the string data.
///
/// ### Interned strings
///
/// Finally, `MetaString` can be created from `InternedString`, which is a string that has been interned using
/// `FixedSizeInterner.` Interned strings are essentially a combination of the properties of `Arc<T>` -- owned values
/// that atomically track the reference count to a shared piece of data -- and a fixed-size buffer, where we allocate
/// one large buffer, and write many small strings into it, and provide references to those strings through
/// `InternedString`.
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
    pub const fn empty() -> Self {
        MetaString {
            // NOTE: This does not allocate.
            inner: Inner::Shared(bytes::Bytes::new()),
        }
    }

    /// Consumes `self` and returns an owned `String`.
    ///
    /// If the `MetaString` is already owned, this will simply return the inner `String` directly. Otherwise, this will
    /// allocate an owned version (`String`) of the string data.
    pub fn into_owned(self) -> String {
        match self.inner {
            Inner::Owned(s) => s,
            Inner::Shared(b) => {
                // SAFETY: We always ensure that the bytes are valid UTF-8 when converting `Bytes` to `MetaString`.
                unsafe { String::from_utf8_unchecked(b.to_vec()) }
            }
            Inner::ProtoShared(b) => b.into(),
            Inner::Interned(i) => (*i).to_owned(),
        }
    }
}

impl Default for MetaString {
    fn default() -> Self {
        MetaString {
            inner: Inner::Owned(String::default()),
        }
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
    fn partial_cmp(&self, other: &MetaString) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MetaString {
    fn cmp(&self, other: &MetaString) -> std::cmp::Ordering {
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
        MetaString {
            inner: Inner::Shared(bytes::Bytes::copy_from_slice(s.as_bytes())),
        }
    }
}

impl TryFrom<bytes::Bytes> for MetaString {
    type Error = std::str::Utf8Error;

    fn try_from(value: bytes::Bytes) -> Result<Self, Self::Error> {
        let _ = std::str::from_utf8(&value)?;
        Ok(MetaString {
            inner: Inner::Shared(value),
        })
    }
}

impl From<InternedString> for MetaString {
    fn from(s: InternedString) -> Self {
        MetaString {
            inner: Inner::Interned(s),
        }
    }
}

impl From<protobuf::Chars> for MetaString {
    fn from(value: protobuf::Chars) -> Self {
        MetaString {
            inner: Inner::ProtoShared(value),
        }
    }
}

impl From<MetaString> for protobuf::Chars {
    fn from(value: MetaString) -> Self {
        match value.inner {
            Inner::Owned(s) => s.into(),
            Inner::Shared(b) => protobuf::Chars::from_bytes(b).expect("already validated as UTF-8"),
            Inner::ProtoShared(b) => b,
            Inner::Interned(i) => i.deref().into(),
        }
    }
}

impl<'a> From<&'a MetaString> for protobuf::Chars {
    fn from(value: &'a MetaString) -> Self {
        match &value.inner {
            Inner::Owned(s) => s.as_str().into(),
            Inner::Shared(b) => protobuf::Chars::from_bytes(b.clone()).expect("already validated as UTF-8"),
            Inner::ProtoShared(b) => b.clone(),
            Inner::Interned(i) => i.deref().into(),
        }
    }
}

impl Deref for MetaString {
    type Target = str;

    fn deref(&self) -> &str {
        match &self.inner {
            Inner::Owned(s) => s,
            // SAFETY: We always ensure that the bytes are valid UTF-8 when converting `Bytes` to `MetaString`.
            Inner::Shared(b) => unsafe { std::str::from_utf8_unchecked(b) },
            Inner::ProtoShared(b) => b,
            Inner::Interned(i) => i,
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
