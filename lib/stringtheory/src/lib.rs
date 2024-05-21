use std::{borrow::Borrow, fmt, ops::Deref};

use bytes::Bytes;

pub mod interning;
use self::interning::fixed_size::InternedString;

#[derive(Clone)]
enum Inner {
    /// An owned string.
    Owned(String),

    /// A shared string.
    ///
    /// Validation that the bytes are UTF-8 happens during conversion to `MetaString`.
    Shared(Bytes),

    /// A shared string from `protobuf::Chars`.
    ///
    /// This is currently a hack, as `Chars` itself is simply a newtype around `Bytes`, but has no way to extract the
    /// inner `Bytes`. In the future, hopefully we can upstream support to do so, but for now, we'll do it this way.
    ProtoShared(protobuf::Chars),

    /// An interned string.
    Interned(InternedString),
}

#[derive(Clone)]
pub struct MetaString {
    inner: Inner,
}

impl MetaString {
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
        self.deref().partial_cmp(other.deref())
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
            inner: Inner::Owned(s.to_owned()),
        }
    }
}

impl TryFrom<Bytes> for MetaString {
    type Error = std::str::Utf8Error;

    fn try_from(value: Bytes) -> Result<Self, Self::Error> {
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

impl Deref for MetaString {
    type Target = str;

    fn deref(&self) -> &str {
        match &self.inner {
            Inner::Owned(s) => s,
            // SAFETY: We always ensure that the bytes are valid UTF-8 when converting `Bytes` to `MetaString`.
            Inner::Shared(b) => unsafe { std::str::from_utf8_unchecked(b) },
            Inner::ProtoShared(b) => &b,
            Inner::Interned(i) => &i,
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
