//! Sharing-optimized strings and string interning utilities.
//!
//! `stringtheory` provides two main components: a sharing-optimized string type, `MetaString`, and a string interning
//! implementation, `FixedSizeInterner`. These components are meant to work in concert, allowing for using a single
//! string type that can handle owned, shared, and interned strings, and providing a way to efficiently intern strings
//! strings when possible.
#![deny(warnings)]
#![deny(missing_docs)]

use std::{borrow::Borrow, fmt, hash, mem::ManuallyDrop, ops::Deref, slice::from_raw_parts, str::from_utf8_unchecked};

pub mod interning;
use serde::Serialize;

use self::interning::InternedString;

const OWNED_PTR_TAG: usize = 0xFF00_0000_0000_0000;
const INLINED_STR_DATA_BUF_LEN: usize = 24;
const INLINED_STR_MAX_LEN: usize = INLINED_STR_DATA_BUF_LEN - 1;
const INLINED_STR_LEN_BYTE_IDX: usize = INLINED_STR_MAX_LEN;

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
enum Zero {
    Zero = 0,
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
#[allow(clippy::enum_clike_unportable_variant)]
enum Unused {
    // NOTE: We use the clippy allow above because otherwise, Clippy falsely warns that our `usize::MAX` is too big for
    // this usize-sized enum on 32-bit platforms... which is obviously wrong on its face.
    //
    // https://github.com/rust-lang/rust-clippy/issues/8043
    Unused = usize::MAX,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EmptyUnion {
    cap: Zero, // Field one.
    len: Zero, // Field two.
    ptr: Zero, // Field three.
}

impl EmptyUnion {
    fn new() -> Self {
        Self {
            cap: Zero::Zero,
            len: Zero::Zero,
            ptr: Zero::Zero,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct OwnedUnion {
    cap: usize,   // Field one.
    len: usize,   // Field two.
    ptr: *mut u8, // Field three.
}

impl OwnedUnion {
    fn as_str(&self) -> &str {
        // Mask our pointer to remove the top byte discriminant value.
        let masked_ptr = (self.ptr as usize & !OWNED_PTR_TAG) as *const u8;

        // SAFETY: We know our pointer is valid, and non-null, since it's derived from a valid `String`.
        // SAFETY: We know our data is valid UTF-8 since it's from a valid `String`.
        unsafe { from_utf8_unchecked(from_raw_parts(masked_ptr, self.len)) }
    }

    fn into_owned(self) -> String {
        // Mask our pointer to remove the top byte discriminant value.
        let masked_ptr = (self.ptr as usize & !OWNED_PTR_TAG) as *mut u8;

        // SAFETY: We know our pointer is valid, and non-null, since it's derived from a valid `String`.
        // SAFETY: We know our data is valid UTF-8 since it's from a valid `String`.
        unsafe { String::from_raw_parts(masked_ptr, self.len, self.cap) }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct StaticUnion {
    _padding1: Unused, // Field one.
    len: usize,        // Field two.
    ptr: *mut u8,      // Field three.
}

impl StaticUnion {
    fn as_str(&self) -> &str {
        // SAFETY: We know our pointer is valid, and non-null, since it's derived from a valid static string reference.
        // SAFETY: We know our data is valid UTF-8 since it's from a valid static string reference.
        unsafe { from_utf8_unchecked(from_raw_parts(self.ptr, self.len)) }
    }
}

#[repr(C)]
struct InternedUnion {
    _padding1: Unused,     // Field one.
    _padding2: Unused,     // Field two.
    state: InternedString, // Field three.
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InlinedUnion {
    // Data is arranged as 23 bytes of string data, and 1 byte for the string length.
    data: [u8; INLINED_STR_DATA_BUF_LEN], // Fields one, two, and three.
}

impl InlinedUnion {
    fn as_str(&self) -> &str {
        let len = self.data[INLINED_STR_LEN_BYTE_IDX] as usize;

        // SAFETY: We know our data is valid UTF-8 since we only ever derive inlined strings from valid strings.
        unsafe { from_utf8_unchecked(&self.data[0..len]) }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DiscriminantUnion {
    maybe_cap: usize,
    maybe_len: usize,
    maybe_ptr: usize,
}

enum UnionType {
    Empty,
    Owned,
    Static,
    Interned,
    Inlined,
}

impl DiscriminantUnion {
    fn get_union_type(&self) -> UnionType {
        if self.maybe_ptr == 0 {
            // When `maybe_ptr` is zero, it implies a null pointer for any pointer-based variants, and a zero length for the
            // inlined variant, which means this has to be representing an empty string.
            UnionType::Empty
        } else {
            // Check the capacity field first to determine if it's an owned string or not.
            if self.maybe_cap == usize::MAX {
                // We can't have an owned string if its capacity is `usize::MAX`, so now look at `maybe_len`, which
                // tells us if we're dealing with a static or interned string.
                if self.maybe_len == usize::MAX {
                    // Similarly, we can't have any sort of allocated string with a length of `usize::MAX`, so this has
                    // to be an interned string.
                    UnionType::Interned
                } else {
                    // We have a valid length, so this has to be a static string.
                    UnionType::Static
                }
            } else {
                // We know this is either an owned string or an inlined string. Check the lowest byte of `maybe_ptr` to
                // see if it's greater than the maximum length for an inlined string.
                if self.maybe_ptr & OWNED_PTR_TAG == OWNED_PTR_TAG {
                    UnionType::Owned
                } else {
                    UnionType::Inlined
                }
            }
        }
    }
}

/// The core data structure for holding all different string variants.
///
/// This union have four data fields -- one for each possible string variant -- and a discriminant field, used to
/// determine which string variant is actually present. The discriminant field interrogates the bits in each machine
/// word field (all variants are three machine words) to determine which bit patterns are valid or not for a given
/// variant, allowing the string variant to be authoritatively determined.
///
/// ## Invariants
///
/// This code depends on a number of invariants in order to work correctly:
///
/// 1. Only used on 64-bit platforms.
/// 2. The pointers for `String` (pointer to the byte allocation) and `&'static str` (pointer to the byte slice) cannot
///    ever be null when the strings are non-empty.
/// 3. Any valid pointer we acquire to the data for any string variant will not have its top byte set (bits 57-64) in
///    its original form, and is safe to be masked out.
/// 4. Allocations can never be larger than `isize::MAX`, meaning that any length/capacity field for a string cannot
///    ever be larger than `isize::MAX`, implying the 63rd bit (top-most bit) for length/capacity should always be 0.
/// 5. A valid UTF-8 string can never have a byte that is all ones (0xFF).
/// 6. An inlined string can only hold up to 23 bytes of data, meaning that the length field for that string can never
///    have a value greater than 23.
union Inner {
    empty: EmptyUnion,
    owned: OwnedUnion,
    _static: StaticUnion,
    interned: ManuallyDrop<InternedUnion>,
    inlined: InlinedUnion,
    discriminant: DiscriminantUnion,
}

impl Inner {
    const fn empty() -> Self {
        Self {
            empty: EmptyUnion {
                cap: Zero::Zero,
                len: Zero::Zero,
                ptr: Zero::Zero,
            },
        }
    }

    fn owned(value: String) -> Self {
        match value.capacity() {
            // The string we got is empty.
            0 => Self::empty(),
            cap => {
                let mut value = value.into_bytes();
                let len = value.len();

                // We take the string data pointer and we set the top byte to a known value to indicate that this is an
                // owned string, which we look for when discriminating the string during dereferencing.
                let ptr = (value.as_mut_ptr() as usize | OWNED_PTR_TAG) as *mut _;

                // We're taking ownership of the underlying string allocation so we can't let it drop.
                std::mem::forget(value);

                Self {
                    owned: OwnedUnion { cap, len, ptr },
                }
            }
        }
    }

    fn static_str(value: &'static str) -> Self {
        match value.len() {
            0 => Self::empty(),
            len => Self {
                _static: StaticUnion {
                    _padding1: Unused::Unused,
                    len,
                    ptr: value.as_bytes().as_ptr() as *mut _,
                },
            },
        }
    }

    fn interned(value: InternedString) -> Self {
        Self {
            interned: ManuallyDrop::new(InternedUnion {
                _padding1: Unused::Unused,
                _padding2: Unused::Unused,
                state: value,
            }),
        }
    }

    fn try_inlined(value: &str) -> Option<Self> {
        let len = value.len();
        if len > INLINED_STR_MAX_LEN {
            return None;
        }

        // SAFETY: We know it fits because we just checked that the string length is 23 or less.
        let len_b = len as u8;

        let mut data = [0; INLINED_STR_DATA_BUF_LEN];
        data[INLINED_STR_LEN_BYTE_IDX] = len_b;

        let buf = value.as_bytes();
        data[0..len].copy_from_slice(buf);

        Some(Self {
            inlined: InlinedUnion { data },
        })
    }

    fn as_str(&self) -> &str {
        let union_type = unsafe { self.discriminant.get_union_type() };
        match union_type {
            UnionType::Empty => "",
            UnionType::Owned => {
                let owned = unsafe { &self.owned };
                owned.as_str()
            }
            UnionType::Static => {
                let _static = unsafe { &self._static };
                _static.as_str()
            }
            UnionType::Interned => {
                let interned = unsafe { &self.interned };
                &interned.state
            }
            UnionType::Inlined => {
                let inlined = unsafe { &self.inlined };
                inlined.as_str()
            }
        }
    }

    fn into_owned(mut self) -> String {
        let union_type = unsafe { self.discriminant.get_union_type() };
        match union_type {
            UnionType::Empty => String::new(),
            UnionType::Owned => {
                // We're (`Inner`) being consumed here, but we need to update our internal state to ensure that our drop
                // logic doesn't try to double free the string allocation.
                let owned = unsafe { self.owned };
                self.empty = EmptyUnion::new();

                owned.into_owned()
            }
            UnionType::Static => {
                let _static = unsafe { self._static };
                _static.as_str().to_owned()
            }
            UnionType::Interned => {
                let interned = unsafe { &self.interned };
                (*interned.state).to_owned()
            }
            UnionType::Inlined => {
                let inlined = unsafe { self.inlined };
                inlined.as_str().to_owned()
            }
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let union_type = unsafe { self.discriminant.get_union_type() };
        match union_type {
            UnionType::Owned => {
                let owned = unsafe { &mut self.owned };

                let len = owned.len;
                let cap = owned.cap;

                // Mask out our top byte discriminant value before dereferencing the pointer.
                let ptr = (owned.ptr as usize & !OWNED_PTR_TAG) as *mut _;

                // SAFETY: The pointer can't be null if we have a non-zero capacity, as that implies having to have
                // allocated memory, which can't come via a null pointer.
                let data = unsafe { Vec::<u8>::from_raw_parts(ptr, len, cap) };
                drop(data);
            }
            UnionType::Interned => {
                let interned = unsafe { &mut self.interned };

                // SAFETY: We're already dropping `Inner`, so nothing else can use the `InternedString` value after we
                // consume it here.
                let data = unsafe { ManuallyDrop::take(interned) };
                drop(data);
            }
            _ => {}
        }
    }
}

impl Clone for Inner {
    fn clone(&self) -> Self {
        let union_type = unsafe { self.discriminant.get_union_type() };
        match union_type {
            UnionType::Empty => Self::empty(),
            UnionType::Owned => {
                let owned = unsafe { self.owned };
                let s = owned.as_str();

                // We specifically try to inline here.
                //
                // At a high-level, when we're _given_ an owned string, we avoid inlining because we don't want to
                // trash the underlying allocation since we may be asked to give it back later (via
                // `MetaString::into_owned`). However, when we're _cloning_, we'd rather save an allocation if we can
                // help it.
                Self::try_inlined(s).unwrap_or_else(|| Self::owned(s.to_owned()))
            }
            UnionType::Static => Self {
                _static: unsafe { self._static },
            },
            UnionType::Interned => {
                let interned = unsafe { &self.interned };
                Self::interned(interned.state.clone())
            }
            UnionType::Inlined => Self {
                inlined: unsafe { self.inlined },
            },
        }
    }
}

// SAFETY: None of our union variants are tied to the original thread they were created on.
unsafe impl Send for Inner {}

// SAFETY: None of our union variants use any form of interior mutability and are thus safe to be shared between threads.
unsafe impl Sync for Inner {}

/// A string type that abstracts over various forms of string storage.
///
/// Normally, developers will work with either `String` (owned) or `&str` (borrowed) when dealing with strings. In some
/// cases, though, it can be useful to work with strings that use alternative storage, such as those that are atomically
/// shared (e.g. `InternedString`). While using those string types themselves isn't complex, using them and _also_
/// supporting normal string types can be complex.
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
/// - inlined (up to 23 bytes)
///
/// ### Owned and borrowed strings
///
/// `MetaString` can be created from `String` and `&str` directly. For owned scenarios (`String`), the string value is
/// simply wrapped. For borrowed strings, we attempt to inline them (see more below) or, if they cannot be inlined, they
/// are copied into a new `String`.
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
/// can be stored directly in a string type itself by utilizing a "union"-style layout.
///
/// As `MetaString` utilizes such a layout, we can provide a small string optimization that allows for strings up to
/// 23 bytes in length.
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
        Self { inner: Inner::empty() }
    }

    /// Creates a new `MetaString` from the given static string.
    ///
    /// This does not allocate.
    pub fn from_static(s: &'static str) -> Self {
        Self {
            inner: Inner::static_str(s),
        }
    }

    /// Attempts to create a new `MetaString` from the given string if it can be inlined.
    pub fn try_inline(s: &str) -> Option<Self> {
        Inner::try_inlined(s).map(|inner| Self { inner })
    }

    /// Consumes `self` and returns an owned `String`.
    ///
    /// If the `MetaString` is already owned, this will simply return the inner `String` directly. Otherwise, this will
    /// allocate an owned version (`String`) of the string data.
    pub fn into_owned(self) -> String {
        self.inner.into_owned()
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

impl Serialize for MetaString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.deref())
    }
}

impl From<String> for MetaString {
    fn from(s: String) -> Self {
        Self { inner: Inner::owned(s) }
    }
}

impl From<&str> for MetaString {
    fn from(s: &str) -> Self {
        Self::try_inline(s).unwrap_or_else(|| Self::from(s.to_owned()))
    }
}

impl From<InternedString> for MetaString {
    fn from(s: InternedString) -> Self {
        Self {
            inner: Inner::interned(s),
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
        // TODO: We're foregoing additional code/complexity to recover a static reference, when our string storage is
        // static, in order to optimize the conversion to `Bytes` and then `Chars`.
        //
        // Static strings being written to Protocol Buffers should be decently rare across the codebase, so no biggie
        // for now.
        value.deref().into()
    }
}

impl Deref for MetaString {
    type Target = str;

    fn deref(&self) -> &str {
        self.inner.as_str()
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

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use super::{interning::FixedSizeInterner, InlinedUnion, Inner, MetaString};

    #[test]
    fn struct_sizes() {
        // Overall, we expect `MetaString`, and thus `Inner`, to always be three machine words.
        assert_eq!(std::mem::size_of::<MetaString>(), std::mem::size_of::<usize>() * 3);
        assert_eq!(std::mem::size_of::<Inner>(), std::mem::size_of::<usize>() * 3);

        // We also expect all of inlined union variant to be the exact size of `Inner`, which means we're properly
        // maximizing the available space for inlining.
        assert_eq!(std::mem::size_of::<InlinedUnion>(), std::mem::size_of::<Inner>());
    }

    #[test]
    fn static_str() {
        let s = "hello";
        let meta = MetaString::from_static(s);

        assert_eq!(s, &*meta);
    }

    #[test]
    fn owned_string() {
        let s = String::from("hello");
        let meta = MetaString::from(s);

        assert_eq!("hello", &*meta);
    }

    #[test]
    fn inlined_string() {
        let s = "hello";
        let meta = MetaString::from(s);

        assert_eq!("hello", &*meta);
    }

    #[test]
    fn interned_string() {
        let intern_str = "hello interned str!";

        let interner = FixedSizeInterner::<1>::new(NonZeroUsize::new(1024).unwrap());
        let s = interner.try_intern(intern_str).unwrap();
        assert_eq!(intern_str, &*s);

        let meta = MetaString::from(s);
        assert_eq!(intern_str, &*meta);
    }
}
