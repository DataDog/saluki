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

const ZERO_VALUE: usize = 0;
const TOP_MOST_BIT: usize = usize::MAX & !(isize::MAX as usize);
const INLINED_STR_DATA_BUF_LEN: usize = std::mem::size_of::<usize>() * 3;
const INLINED_STR_MAX_LEN: usize = INLINED_STR_DATA_BUF_LEN - 1;

// High-level invariant checks to ensure `stringtheory` isn't being used on an unsupported platform.
#[cfg(not(all(target_pointer_width = "64", target_endian = "little")))]
const _INVARIANTS_CHECK: () = {
    compile_error!("`stringtheory` is only supported on 64-bit little-endian platforms.");
};

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
enum Zero {
    Zero = ZERO_VALUE,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EmptyUnion {
    ptr: Zero, // Field one.
    len: Zero, // Field two.
    cap: Zero, // Field three.
}

impl EmptyUnion {
    const fn new() -> Self {
        Self {
            ptr: Zero::Zero,
            len: Zero::Zero,
            cap: Zero::Zero,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct OwnedUnion {
    ptr: *mut u8, // Field one
    len: usize,   // Field two.
    cap: usize,   // Field three.
}

impl OwnedUnion {
    #[inline]
    fn as_str(&self) -> &str {
        // SAFETY: We know our pointer is valid, and non-null, since it's derived from a valid `String`, and that the
        // data it points to is valid UTF-8, again, by virtue of it being derived from a valid `String`.
        unsafe { from_utf8_unchecked(from_raw_parts(self.ptr, self.len)) }
    }

    fn into_owned(self) -> String {
        // SAFETY: We know our pointer is valid, and non-null, since it's derived from a valid `String`, and that the
        // data it points to is valid UTF-8, again, by virtue of it being derived from a valid `String`.
        unsafe { String::from_raw_parts(self.ptr, self.len, untag_cap(self.cap)) }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct StaticUnion {
    ptr: *mut u8, // Field one.
    len: usize,   // Field two.
    _cap: Zero,   // Field three.
}

impl StaticUnion {
    #[inline]
    const fn as_str(&self) -> &str {
        // SAFETY: We know our pointer is valid, and non-null, since it's derived from a valid static string reference,
        // and that the data it points to is valid UTF-8, again, by virtue of it being derived from a valid static
        // string reference.
        unsafe { from_utf8_unchecked(from_raw_parts(self.ptr, self.len)) }
    }
}

#[repr(C)]
struct InternedUnion {
    state: InternedString, // Field one.
    _len: Zero,            // Field two.
    _cap: Zero,            // Field three.
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InlinedUnion {
    // Data is arranged as 23 bytes for string data, and the remaining 1 byte for the string length.
    data: [u8; INLINED_STR_DATA_BUF_LEN], // Fields one, two, and three.
}

impl InlinedUnion {
    #[inline]
    fn as_str(&self) -> &str {
        let len = self.data[INLINED_STR_MAX_LEN] as usize;

        // SAFETY: We know our data is valid UTF-8 since we only ever derive inlined strings from a valid string
        // reference.
        unsafe { from_utf8_unchecked(&self.data[0..len]) }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DiscriminantUnion {
    maybe_ptr: usize, // Field one.
    maybe_len: usize, // Field two.
    maybe_cap: usize, // Field three.
}

#[derive(Debug, Eq, PartialEq)]
enum UnionType {
    Empty,
    Owned,
    Static,
    Interned,
    Inlined,
}

const fn is_tagged(cap: usize) -> bool {
    cap & TOP_MOST_BIT != 0
}

const fn tag_cap(cap: usize) -> usize {
    cap | TOP_MOST_BIT
}

const fn untag_cap(cap: usize) -> usize {
    cap & !(TOP_MOST_BIT)
}

impl DiscriminantUnion {
    #[inline]
    const fn get_union_type(&self) -> UnionType {
        // All union variants are laid out in the same way -- pointer, length, and capacity -- so we can examine the
        // values in our discriminant union to determine what those values would be in the actual union variant.
        match (self.maybe_ptr, self.maybe_len, self.maybe_cap) {
            // Empty string.
            (ZERO_VALUE, ZERO_VALUE, ZERO_VALUE) => UnionType::Empty,
            // Interned string.
            (_ptr, ZERO_VALUE, ZERO_VALUE) => UnionType::Interned,
            // Static string.
            (_ptr, _len, ZERO_VALUE) => UnionType::Static,
            // Owned string _or_ inlined string.
            //
            // We abuse the layout of little endian integers here to coincide with the layout of an inlined string.
            //
            // In an inlined string, its length byte is situated as the very last byte (data[23]) in its layout. In an
            // owned string, we instead have a pointer, length, and capacity, in that order. This means that the length
            // byte of the inlined string and the capacity field of the owned string overlap, as seen below:
            //
            //                ~ an inlined string, "hello, world", with a length of 12 (0C) ~
            //      ┌───────────────────────────────────────────────────────────────────────────────┐
            //      │ 68 65 6C 6C 6F 20 77 6F    72 6C 64 21 ?? ?? ?? ??    ?? ?? ?? ?? ?? ?? ?? 0C │
            //      └───────────────────────────────────────────────────────────────────────────────┘
            //                                                                                    ▲
            //                                                                                    ├──── overlapping
            //                          ~ an owned string with a capacity of 64 ~                 ▼         byte
            //      ┌─────────────────────────┐┌─────────────────────────┐┌─────────────────────────┐
            //      │ ?? ?? ?? ?? ?? ?? ?? ?? ││ ?? ?? ?? ?? ?? ?? ?? ?? ││ 40 00 00 00 00 00 00 80 │
            //      └─────────────────────────┘└─────────────────────────┘└─────────────────────────┘
            //                                                                                    ▲
            //                 original capacity: 64                  (0x0000000000000040)        │
            //                 "tagged" capacity: 9223372036854775872 (0x8000000000000040)        │
            //                                                           ▲                        │
            //                     (tag bit) ────────────────────────────┘                        │
            //                                                                                    │
            //                                                                                    │
            //                       inlined last byte (0x0C)  [0 0 0 0 1 1 0 0] ◀────────────────┤
            //                       owned last byte (0x80)    [1 0 0 0 0 0 0 0] ◀────────────────┤
            //                                                                                    │
            //                       inlined last byte                                            │
            //                       maximum of 23 (0x17)      [0 0 0 1 0 1 1 1] ◀────────────────┤
            //                                                                                    │
            //                       "owned" discriminant                                         │
            //                       bitmask (any X bit)       [X X X ? ? ? ? ?] ◀────────────────┘
            //
            // As such, the length byte of an inlined string overlaps with the capacity field of an owned string.
            // Additionally, due to the integer fields being written in little-endian order, the "upper" byte of the
            // capacity field -- the eight highest bits -- is actually written in the same location as the length byte
            // of an inlined string.
            //
            // Given that we know an inlined string cannot be any longer than 23 bytes, we know that the top-most bit in
            // the last byte (data[23]) can never be set, as it would imply a length of _at least_ 128. With that, we
            // utilize invariant #3 of `Inner` -- allocations can never be larger than `isize::MAX` -- which lets us
            // safely "tag" an owned string's capacity -- setting the upper most bit to 1 -- to indicate that it's an
            // owned string.
            //
            // An inlined string couldn't possibly have a length that occupied that bit, and so we know if we're down
            // here, and our pointer field isn't all zeroes and our length field isn't all zeroes, that we _have_ to be
            // an owned string if our capacity is tagged.
            (_ptr, _len, cap) => {
                if is_tagged(cap) {
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
/// This union has five data fields -- one for each possible string variant -- and a discriminant field, used to
/// determine which string variant is actually present. The discriminant field interrogates the bits in each machine
/// word field (all variants are three machine words) to determine which bit patterns are valid or not for a given
/// variant, allowing the string variant to be authoritatively determined.
///
/// ## Invariants
///
/// This code depends on a number of invariants in order to work correctly:
///
/// 1. Only used on 64-bit little-endian platforms. (checked at compile-time via _INVARIANTS_CHECK)
/// 2. The data pointers for `String` and `&'static str` cannot  ever be null when the strings are non-empty.
/// 3. Allocations can never be larger than `isize::MAX` (see [here][rust_isize_alloc_limit]), meaning that any
///    length/capacity field for a string cannot ever be larger than `isize::MAX`, implying the 63rd bit (top-most bit)
///    for length/capacity should always be 0.
/// 4. An inlined string can only hold up to 23 bytes of data, meaning that the length byte for that string can never
///    have a value greater than 23. (_We_ have to provide this invariant, which is handled in `Inner::try_inlined`.)
///
/// [rust_isize_alloc_limit]: https://doc.rust-lang.org/stable/std/alloc/struct.Layout.html#method.from_size_align
union Inner {
    empty: EmptyUnion,
    owned: OwnedUnion,
    static_: StaticUnion,
    interned: ManuallyDrop<InternedUnion>,
    inlined: InlinedUnion,
    discriminant: DiscriminantUnion,
}

impl Inner {
    const fn empty() -> Self {
        Self {
            empty: EmptyUnion::new(),
        }
    }

    fn owned(value: String) -> Self {
        match value.capacity() {
            // The string we got is empty.
            0 => Self::empty(),
            cap => {
                let mut value = value.into_bytes();

                let ptr = value.as_mut_ptr();
                let len = value.len();

                // We're taking ownership of the underlying string allocation so we can't let it drop.
                std::mem::forget(value);

                Self {
                    owned: OwnedUnion {
                        ptr,
                        len,
                        cap: tag_cap(cap),
                    },
                }
            }
        }
    }

    fn static_str(value: &'static str) -> Self {
        match value.len() {
            0 => Self::empty(),
            len => Self {
                static_: StaticUnion {
                    ptr: value.as_bytes().as_ptr() as *mut _,
                    len,
                    _cap: Zero::Zero,
                },
            },
        }
    }

    fn interned(value: InternedString) -> Self {
        match value.len() {
            0 => Self::empty(),
            _len => Self {
                interned: ManuallyDrop::new(InternedUnion {
                    state: value,
                    _len: Zero::Zero,
                    _cap: Zero::Zero,
                }),
            },
        }
    }

    fn try_inlined(value: &str) -> Option<Self> {
        match value.len() {
            0 => Some(Self::empty()),
            len => {
                if len > INLINED_STR_MAX_LEN {
                    return None;
                }

                let mut data = [0; INLINED_STR_DATA_BUF_LEN];

                // SAFETY: We know it fits because we just checked that the string length is 23 or less.
                data[INLINED_STR_MAX_LEN] = len as u8;

                let buf = value.as_bytes();
                data[0..len].copy_from_slice(buf);

                Some(Self {
                    inlined: InlinedUnion { data },
                })
            }
        }
    }

    #[inline]
    fn as_str(&self) -> &str {
        let union_type = unsafe { self.discriminant.get_union_type() };
        match union_type {
            UnionType::Empty => "",
            UnionType::Owned => {
                let owned = unsafe { &self.owned };
                owned.as_str()
            }
            UnionType::Static => {
                let static_ = unsafe { &self.static_ };
                static_.as_str()
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

    #[cfg(test)]
    fn get_union_type(&self) -> UnionType {
        unsafe { self.discriminant.get_union_type() }
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
                let static_ = unsafe { self.static_ };
                static_.as_str().to_owned()
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

                let ptr = owned.ptr;
                let len = owned.len;
                let cap = untag_cap(owned.cap);

                // SAFETY: The pointer has to be non-null, because we only ever construct an owned variant when the
                // `String` has a non-zero capacity, which implies a valid allocation, and thus a valid, non-null
                // pointer.
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
                // At a high-level, when we're _given_ an owned string, we avoid inlining because we don't want to trash
                // the underlying allocation since we may be asked to give it back later (via `MetaString::into_owned`).
                // However, when we're _cloning_, we'd rather avoiding allocating if we can help it.
                Self::try_inlined(s).unwrap_or_else(|| Self::owned(s.to_owned()))
            }
            UnionType::Static => Self {
                static_: unsafe { self.static_ },
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

// SAFETY: None of our union variants use any form of interior mutability and are thus safe to be shared between
// threads.
unsafe impl Sync for Inner {}

/// An immutable string type that abstracts over various forms of string storage.
///
/// Normally, developers will work with either `String` (owned) or `&str` (borrowed) when dealing with strings. In some
/// cases, though, it can be useful to work with strings that use alternative storage, such as those that are atomically
/// shared (e.g. `InternedString`). While using those string types themselves isn't complex, using them and _also_
/// supporting normal string types can be complex.
///
/// `MetaString` is an opinionated string type that abstracts over the normal string types like `String` and `&str`
/// while also supporting alternative storage, such as interned strings from `FixedSizeInterner`, unifying these
/// different variants behind a single concrete type.
///
/// ## Supported types
///
/// `MetaString` supports the following "variants":
///
/// - owned (`String` and non-inlineable `&str`)
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
/// [`FixedSizeInterner`][crate::interning::FixedSizeInterner]. Interned strings are essentially a combination of the
/// properties of `Arc<T>` -- owned wrappers around an atomically reference counted piece of data -- and a fixed-size
/// buffer, where we allocate one large buffer, and write many small strings into it, and provide references to those
/// strings through `InternedString`.
///
/// ### Inlined strings
///
/// Finally, `MetaString` can also be created by inlining small strings into `MetaString` itself, avoiding the need for
/// any backing allocation. "Small string optimization" is a common optimization for string types where small strings
/// can be stored directly in a string type itself by utilizing a "union"-style layout.
///
/// As `MetaString` utilizes such a layout, we can provide a small string optimization that allows for strings up to 23
/// bytes in length.
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
        Self::try_inline(s).unwrap_or_else(|| Self {
            inner: Inner::static_str(s),
        })
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

    use proptest::{prelude::*, proptest};

    use super::{interning::FixedSizeInterner, InlinedUnion, Inner, MetaString, UnionType};

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
    fn static_str_inlineable() {
        let s = "hello";
        let meta = MetaString::from_static(s);

        assert_eq!(s, &*meta);
        assert_eq!(meta.inner.get_union_type(), UnionType::Inlined);
        assert_eq!(s, meta.into_owned());
    }

    #[test]
    fn static_str_not_inlineable() {
        let s = "hello there, world! it's me, margaret!";
        let meta = MetaString::from_static(s);

        assert_eq!(s, &*meta);
        assert_eq!(meta.inner.get_union_type(), UnionType::Static);
        assert_eq!(s, meta.into_owned());
    }

    #[test]
    fn owned_string() {
        let s_orig = "hello";
        let s = String::from(s_orig);
        let meta = MetaString::from(s);

        assert_eq!(s_orig, &*meta);
        assert_eq!(meta.inner.get_union_type(), UnionType::Owned);
        assert_eq!(s_orig, meta.into_owned());
    }

    #[test]
    fn inlined_string() {
        let s = "hello";
        let meta = MetaString::from(s);

        assert_eq!(s, &*meta);
        assert_eq!(meta.inner.get_union_type(), UnionType::Inlined);
        assert_eq!(s, meta.into_owned());
    }

    #[test]
    fn interned_string() {
        let intern_str = "hello interned str!";

        let interner = FixedSizeInterner::<1>::new(NonZeroUsize::new(1024).unwrap());
        let s = interner.try_intern(intern_str).unwrap();
        assert_eq!(intern_str, &*s);

        let meta = MetaString::from(s);
        assert_eq!(intern_str, &*meta);
        assert_eq!(meta.inner.get_union_type(), UnionType::Interned);
        assert_eq!(intern_str, meta.into_owned());
    }

    #[test]
    fn empty_string_interned() {
        let intern_str = "";

        let interner = FixedSizeInterner::<1>::new(NonZeroUsize::new(1024).unwrap());
        let s = interner.try_intern(intern_str).unwrap();
        assert_eq!(intern_str, &*s);

        let meta = MetaString::from(s);
        assert_eq!(intern_str, &*meta);
        assert_eq!(meta.inner.get_union_type(), UnionType::Empty);
        assert_eq!(intern_str, meta.into_owned());
    }

    #[test]
    fn empty_string_static() {
        let s = "";

        let meta = MetaString::from_static(s);
        assert_eq!(s, &*meta);
        assert_eq!(meta.inner.get_union_type(), UnionType::Empty);
        assert_eq!(s, meta.into_owned());
    }

    #[test]
    fn empty_string_inlined() {
        let s = "";

        let meta = MetaString::try_inline(s).expect("empty string definitely 'fits'");
        assert_eq!(s, &*meta);
        assert_eq!(meta.inner.get_union_type(), UnionType::Empty);
        assert_eq!(s, meta.into_owned());
    }

    #[test]
    fn empty_string_owned() {
        // When a string has capacity, we don't care if it's actually empty or not, because we want to preserve the
        // allocation... so our string here is empty but _does_ have capacity.
        let s = String::with_capacity(32);
        let actual_cap = s.capacity();

        let meta = MetaString::from(s);
        assert_eq!("", &*meta);
        assert_eq!(meta.inner.get_union_type(), UnionType::Owned);

        let owned = meta.into_owned();
        assert_eq!(owned.capacity(), actual_cap);
        assert_eq!("", owned);
    }

    #[test]
    fn empty_string_owned_zero_capacity() {
        // When a string has _no_ capacity, it's effectively empty, and we treat it that way.
        let s = String::new();

        let meta = MetaString::from(s);
        assert_eq!("", &*meta);
        assert_eq!(meta.inner.get_union_type(), UnionType::Empty);
        assert_eq!("", meta.into_owned());
    }

    fn arb_unicode_str_max_len(max_len: usize) -> impl Strategy<Value = String> {
        ".{0,23}".prop_filter("resulting string is too longer", move |s| s.len() <= max_len)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10000))]

        #[test]
        #[cfg_attr(miri, ignore)]
        fn property_test_inlined_string(
            input in arb_unicode_str_max_len(23),
        ) {
            assert!(input.len() <= 23, "input should be 23 bytes or less");
            let meta = MetaString::try_inline(&input).expect("input should fit");

            if input.is_empty() {
                assert_eq!(meta.inner.get_union_type(), UnionType::Empty);
            } else {
                assert_eq!(input, &*meta);
                assert_eq!(meta.inner.get_union_type(), UnionType::Inlined);
            }
        }

        #[test]
        #[cfg_attr(miri, ignore)]
        fn property_test_owned_string(
            input in ".*",
        ) {
            let is_empty = input.is_empty();
            let meta = MetaString::from(input);

            if is_empty {
                assert_eq!(meta.inner.get_union_type(), UnionType::Empty);
            } else {
                assert_eq!(meta.inner.get_union_type(), UnionType::Owned);
            }
        }
    }
}
