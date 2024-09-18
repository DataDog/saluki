//! Interning utilities.
mod fixed_size;
use std::{fmt, ops::Deref};

pub use self::fixed_size::FixedSizeInterner;

pub(crate) struct InternerVtable {
    /// Name of the interner implementation that this string was interned with.
    pub interner_name: &'static str,

    /// Gets the raw parts of the interned string for reassembly.
    ///
    /// This is structured as such so that the caller can tie the appropriate lifetime to the string reference, as it
    /// cannot be done as part of the vtable signature itself.
    pub as_raw_parts: unsafe fn(*const ()) -> (*const u8, usize),

    /// Drops the interned string.
    pub drop: unsafe fn(*const ()),
}

/// An interned string.
///
/// This string type is read-only, and dereferences to `&str` for ergonomic usage. It is cheap to clone (16 bytes), but
/// generally will not be interacted with directly. Instead, most usages should be wrapped in `MetaString`.
#[derive(Clone)]
pub struct InternedString {
    state: *const (),
    vtable: &'static InternerVtable,
}

impl fmt::Debug for InternedString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InternedString")
            .field("state", &self.state)
            .field("vtable", &self.vtable.interner_name)
            .finish()
    }
}

impl Deref for InternedString {
    type Target = str;

    fn deref(&self) -> &str {
        // SAFETY: The virtual table is responsible for ensuring that the returned pointer is non-null and valid,
        // pointing to a slice of the given length, with valid UTF-8.
        unsafe {
            let (ptr, len) = (self.vtable.as_raw_parts)(self.state);
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(ptr, len))
        }
    }
}

impl PartialEq for InternedString {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::addr_eq(self.state, other.state)
    }
}

impl Drop for InternedString {
    fn drop(&mut self) {
        // SAFETY: The virtual table is responsible for ensuring that the pointer is non-null and valid.
        unsafe {
            (self.vtable.drop)(self.state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_of_interned_string() {
        // We're asserting that `InternedString` itself is 16 bytes: the thin pointer to the underlying interned
        // string's representation, and the vtable pointer.
        assert_eq!(std::mem::size_of::<InternedString>(), 16);
    }
}
