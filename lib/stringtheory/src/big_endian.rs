use std::{borrow::Borrow, fmt, hash, ops::Deref, sync::Arc};

use serde::{Deserialize, Serialize};

use crate::interning::{InternedString, Interner};

enum Inner {
    Static(&'static str),
    Interned(InternedString),
    Shared(Arc<str>),
}

impl Inner {
    fn as_str(&self) -> &str {
        match self {
            Self::Static(s) => s,
            Self::Interned(s) => s.deref(),
            Self::Shared(s) => s.as_ref(),
        }
    }
}

impl Clone for Inner {
    fn clone(&self) -> Self {
        match self {
            Self::Static(s) => Self::Static(s),
            Self::Interned(s) => Self::Interned(s.clone()),
            Self::Shared(s) => Self::Shared(Arc::clone(s)),
        }
    }
}

/// A sharing-optimized string type.
///
/// On big-endian platforms, this type preserves the public `MetaString` API with shared string storage rather than the
/// tagged-pointer representation used on little-endian platforms.
pub struct MetaString {
    inner: Inner,
}

impl MetaString {
    /// Creates an empty `MetaString`.
    ///
    /// This doesn't allocate.
    pub const fn empty() -> Self {
        Self {
            inner: Inner::Static(""),
        }
    }

    /// Creates a new `MetaString` from the given static string.
    ///
    /// This doesn't allocate.
    pub const fn from_static(s: &'static str) -> Self {
        Self {
            inner: Inner::Static(s),
        }
    }

    /// Attempts to create a new `MetaString` from the given string if it can be inlined.
    pub fn try_inline(s: &str) -> Option<Self> {
        if s.len() <= 23 {
            Some(Self {
                inner: Inner::Shared(Arc::from(s)),
            })
        } else {
            None
        }
    }

    /// Creates a new `MetaString` from the given string, using the provided interner.
    ///
    /// The string is inlined if possible. If it can't be inlined, the interner is tried. If interning fails, an owned
    /// string is allocated.
    pub fn from_interner<I>(s: &str, interner: &I) -> Self
    where
        I: Interner,
    {
        if let Some(inlined) = Self::try_inline(s) {
            return inlined;
        }
        if let Some(interned) = interner.try_intern(s) {
            return Self::from(interned);
        }
        Self::from(s.to_owned())
    }

    /// Returns `true` if `self` has a length of zero bytes.
    pub fn is_empty(&self) -> bool {
        self.deref().is_empty()
    }

    /// Returns `true` if `self` can be cheaply cloned.
    pub const fn is_cheaply_cloneable(&self) -> bool {
        true
    }

    /// Consumes `self` and returns an owned `String`.
    pub fn into_owned(self) -> String {
        self.inner.as_str().to_owned()
    }
}

impl Default for MetaString {
    fn default() -> Self {
        Self::empty()
    }
}

impl Clone for MetaString {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
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

impl PartialEq<&str> for MetaString {
    fn eq(&self, other: &&str) -> bool {
        self.deref() == *other
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

impl Serialize for MetaString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.deref())
    }
}

impl<'de> Deserialize<'de> for MetaString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct MetaStringVisitor;

        impl<'de> serde::de::Visitor<'de> for MetaStringVisitor {
            type Value = MetaString;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a string")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(MetaString::from(v))
            }
        }

        deserializer.deserialize_str(MetaStringVisitor)
    }
}

impl From<String> for MetaString {
    fn from(s: String) -> Self {
        Self {
            inner: Inner::Shared(Arc::from(s.as_str())),
        }
    }
}

impl From<&str> for MetaString {
    fn from(s: &str) -> Self {
        Self::try_inline(s).unwrap_or_else(|| Self {
            inner: Inner::Shared(Arc::from(s)),
        })
    }
}

impl From<InternedString> for MetaString {
    fn from(s: InternedString) -> Self {
        Self {
            inner: Inner::Interned(s),
        }
    }
}

impl From<Arc<str>> for MetaString {
    fn from(s: Arc<str>) -> Self {
        Self {
            inner: Inner::Shared(s),
        }
    }
}

impl From<MetaString> for protobuf::Chars {
    fn from(value: MetaString) -> Self {
        value.into_owned().into()
    }
}

impl<'a> From<&'a MetaString> for protobuf::Chars {
    fn from(value: &'a MetaString) -> Self {
        value.deref().into()
    }
}

impl Deref for MetaString {
    type Target = str;

    fn deref(&self) -> &str {
        self.inner.as_str()
    }
}

impl AsRef<str> for MetaString {
    fn as_ref(&self) -> &str {
        self.deref()
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
