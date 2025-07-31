//! Interning utilities.
use std::ops::Deref;

pub(crate) mod fixed_size;
pub use self::fixed_size::FixedSizeInterner;

pub(crate) mod helpers;

pub(crate) mod map;
pub use self::map::GenericMapInterner;

/// A string interner.
pub trait Interner {
    /// Returns `true` if the interner contains no strings.
    fn is_empty(&self) -> bool;

    /// Returns the number of strings in the interner.
    fn len(&self) -> usize;

    /// Returns the total number of bytes in the interner.
    fn len_bytes(&self) -> usize;

    /// Returns the total number of bytes the interner can hold.
    fn capacity_bytes(&self) -> usize;

    /// Attempts to intern the given string.
    ///
    /// Returns `None` if the interner is full or the string cannot fit.
    fn try_intern(&self, s: &str) -> Option<InternedString>;
}

impl<T> Interner for &T
where
    T: Interner,
{
    fn is_empty(&self) -> bool {
        (**self).is_empty()
    }

    fn len(&self) -> usize {
        (**self).len()
    }

    fn len_bytes(&self) -> usize {
        (**self).len_bytes()
    }

    fn capacity_bytes(&self) -> usize {
        (**self).capacity_bytes()
    }

    fn try_intern(&self, s: &str) -> Option<InternedString> {
        (**self).try_intern(s)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum InternedStringState {
    GenericMap(self::map::StringState),
    FixedSize(self::fixed_size::StringState),
}

impl InternedStringState {
    #[inline]
    fn as_str(&self) -> &str {
        match self {
            Self::GenericMap(state) => state.as_str(),
            Self::FixedSize(state) => state.as_str(),
        }
    }
}

impl From<self::fixed_size::StringState> for InternedStringState {
    fn from(state: self::fixed_size::StringState) -> Self {
        Self::FixedSize(state)
    }
}

impl From<self::map::StringState> for InternedStringState {
    fn from(state: self::map::StringState) -> Self {
        Self::GenericMap(state)
    }
}

/// An interned string.
///
/// This string type is read-only, and dereferences to `&str` for ergonomic usage. It is cheap to clone (16 bytes), but
/// generally will not be interacted with directly. Instead, most usages should be wrapped in `MetaString`.
#[derive(Clone, Debug, PartialEq)]
pub struct InternedString {
    state: InternedStringState,
}

impl InternedString {
    pub(crate) fn into_state(self) -> InternedStringState {
        self.state
    }
}

impl<T> From<T> for InternedString
where
    T: Into<InternedStringState>,
{
    fn from(state: T) -> Self {
        Self { state: state.into() }
    }
}

impl Deref for InternedString {
    type Target = str;

    fn deref(&self) -> &str {
        self.state.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_of_interned_string() {
        // We're asserting that `InternedString` itself is 24 bytes: an enum over possible interner implementations,
        // each of which should be 16 bytes in size... making `InternedString` itself 24 bytes in size due to the additional
        // discriminant field.
        assert_eq!(std::mem::size_of::<InternedString>(), 24);
    }
}
