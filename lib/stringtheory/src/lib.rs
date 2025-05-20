//! Sharing-optimized strings and string interning utilities.
//!
//! `stringtheory` provides two main components: a sharing-optimized string type, `MetaString`, and string interning
//! implementations (`FixedSizeInterner`, `GenericMapInterner`, etc). These components are meant to work in concert,
//! allowing for using a single string type that can handle owned, shared, and interned strings, and providing a way to
//! efficiently intern strings strings when possible.
#![deny(warnings)]
#![deny(missing_docs)]
// We only support 64-bit little-endian platforms anyways, so there's no risk of our enum variants having their values truncated.
#![allow(clippy::enum_clike_unportable_variant)]

use std::{
    borrow::Borrow, fmt, hash, mem::ManuallyDrop, ops::Deref, ptr::NonNull, slice::from_raw_parts,
    str::from_utf8_unchecked, sync::Arc,
};

pub mod interning;
use serde::Serialize;

use self::interning::InternedString;

const ZERO_VALUE: usize = 0;
const TOP_MOST_BIT: usize = usize::MAX & !(isize::MAX as usize);
const INLINED_STR_DATA_BUF_LEN: usize = std::mem::size_of::<usize>() * 3;
const INLINED_STR_MAX_LEN: usize = INLINED_STR_DATA_BUF_LEN - 1;
const INLINED_STR_MAX_LEN_U8: u8 = INLINED_STR_MAX_LEN as u8;

const UNION_TYPE_TAG_VALUE_STATIC: u8 = get_offset_tag_value(0);
const UNION_TYPE_TAG_VALUE_INTERNED: u8 = get_offset_tag_value(1);
const UNION_TYPE_TAG_VALUE_SHARED: u8 = get_offset_tag_value(2);

const fn get_offset_tag_value(tag: u8) -> u8 {
    const UNION_TYPE_TAG_VALUE_BASE: u8 = INLINED_STR_MAX_LEN as u8 + 1;

    if tag > (u8::MAX - INLINED_STR_MAX_LEN as u8) {
        panic!("Union type tag value must be less than 232 to fit.");
    }

    tag + UNION_TYPE_TAG_VALUE_BASE
}

const fn get_scaled_union_tag(tag: u8) -> usize {
    // We want to shift our tag value (single byte) to make it the top most byte in a `usize`.
    const UNION_TYPE_TAG_VALUE_SHIFT: u32 = (std::mem::size_of::<usize>() as u32 - 1) * 8;

    (tag as usize) << UNION_TYPE_TAG_VALUE_SHIFT
}

// High-level invariant checks to ensure `stringtheory` isn't being used on an unsupported platform.
#[cfg(not(all(target_pointer_width = "64", target_endian = "little")))]
const _INVARIANTS_CHECK: () = {
    compile_error!("`stringtheory` is only supported on 64-bit little-endian platforms.");
};

const fn is_tagged(cap: u8) -> bool {
    cap & 0b10000000 != 0
}

const fn tag_cap(cap: usize) -> usize {
    cap | TOP_MOST_BIT
}

const fn untag_cap(cap: usize) -> usize {
    cap & !(TOP_MOST_BIT)
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
enum Zero {
    Zero = ZERO_VALUE,
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
enum Tag {
    Static = get_scaled_union_tag(UNION_TYPE_TAG_VALUE_STATIC),
    Interned = get_scaled_union_tag(UNION_TYPE_TAG_VALUE_INTERNED),
    Shared = get_scaled_union_tag(UNION_TYPE_TAG_VALUE_SHARED),
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
    _cap: Tag,    // Field three.
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
    state: InternedString, // Fields one and two.
    _cap: Tag,             // Field three.
}

#[repr(C)]
struct SharedUnion {
    ptr: NonNull<str>, // Fields one and two
    _cap: Tag,         // Field three.
}

impl SharedUnion {
    #[inline]
    const fn as_str(&self) -> &str {
        // SAFETY: The pointee is still live by virtue of being held in an `Arc`.
        unsafe { self.ptr.as_ref() }
    }
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
    unused: [u8; INLINED_STR_DATA_BUF_LEN - 1], // Fields one, two, and three, minus the last byte.
    tag_byte: u8,                               // Last byte, overlapped with the "highest" byte of field three.
}

#[derive(Debug, Eq, PartialEq)]
enum UnionType {
    Empty,
    Owned,
    Static,
    Interned,
    Inlined,
    Shared,
}

impl DiscriminantUnion {
    #[inline]
    const fn get_union_type(&self) -> UnionType {
        // At a high level, we encode the type of the union into the last byte of the struct, which overlaps with the
        // capacity of owned strings, the length of an inlined string, and the unused field of other string types.
        //
        // Our logic is simple here:
        //
        // - Allocations can only ever be as large as `isize::MAX`, which means that the top-most bit of the capacity
        //   value would never be used for a valid allocation.
        // - In turn, the length of a string can also only ever be as large as `isize::MAX`, which means that the
        //   top-most bit of the length byte for any string type would never be used
        // - Inlined strings can only ever be up to 23 bytes long, which means that the top-most bit of the length byte
        //   for an inlined string would never be used.
        // - Static strings and interned strings only occupy the first two fields, which means their capacity should not
        //   be used.
        //
        // As such, we encode the five possible string types as follows:
        //
        // - when all fields are zero, we have an empty string
        // - when the last byte does have the top-bit set, we have an owned string
        // - when the last byte does _not_ have the top-bit set, and the value is less than or equal to 23, we have an
        //   inlined string
        // - when the last byte does _not_ have the top-bit set, and the value is greater than 23, we interpret the
        //   specific value of the last byte as a discriminant for the remaining string types (static, interned, etc)
        //
        // We abuse the layout of little endian integers here to ensure that the upper-most bits of the capacity
        // field overlaps with the last byte of an inlined string, and the unused field of a static/interned string.
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

        // If the top bit is set, we know we're dealing with an owned string.
        if is_tagged(self.tag_byte) {
            return UnionType::Owned;
        }

        // The top-most bit has to be set for an owned string, but isn't set for any other type, so try differentiating
        // at this point.
        match self.tag_byte {
            // Empty string. Easy.
            0 => UnionType::Empty,

            // Anything between 1 and INLINED_STR_MAX_LEN (23, inclusive) is an inlined string.
            1..=INLINED_STR_MAX_LEN_U8 => UnionType::Inlined,

            // These are fixed values between 24 and 128, so we just match them directly.
            UNION_TYPE_TAG_VALUE_STATIC => UnionType::Static,
            UNION_TYPE_TAG_VALUE_INTERNED => UnionType::Interned,
            UNION_TYPE_TAG_VALUE_SHARED => UnionType::Shared,

            // If we haven't matched any specific type tag value, then this is something else that we don't handle or
            // know about... which we handle as just acting like we're an empty string for simplicity.
            _ => UnionType::Empty,
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
/// 2. The data pointers for `String` and `&'static str` cannot ever be null when the strings are non-empty.
/// 3. Allocations can never be larger than `isize::MAX` (see [here][rust_isize_alloc_limit]), meaning that any
///    length/capacity field for a string cannot ever be larger than `isize::MAX`, implying the 64th bit (top-most bit)
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
    shared: ManuallyDrop<SharedUnion>,
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

    const fn static_str(value: &'static str) -> Self {
        match value.len() {
            0 => Self::empty(),
            len => Self {
                static_: StaticUnion {
                    ptr: value.as_bytes().as_ptr() as *mut _,
                    len,
                    _cap: Tag::Static,
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
                    _cap: Tag::Interned,
                }),
            },
        }
    }

    fn shared(value: Arc<str>) -> Self {
        match value.len() {
            0 => Self::empty(),
            _len => Self {
                shared: ManuallyDrop::new(SharedUnion {
                    // SAFETY: We know `ptr` is non-null because `Arc::into_raw` is called on a valid `Arc<str>`.
                    ptr: unsafe { NonNull::new_unchecked(Arc::into_raw(value).cast_mut()) },
                    _cap: Tag::Shared,
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
            UnionType::Shared => {
                let shared = unsafe { &self.shared };
                shared.as_str()
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
            UnionType::Shared => {
                let shared = unsafe { &self.shared };
                shared.as_str().to_owned()
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
            UnionType::Shared => {
                let shared = unsafe { &mut self.shared };

                // Decrement the strong count before we drop, ensuring the `Arc` has a chance to clean itself up if this
                // is the less strong reference.
                //
                // SAFETY: We know `shared.ptr` was obtained from `Arc::into_raw`, so it's valid to decrement on. We
                // also know the backing storage is still live because it has to be by virtue of us being here.
                unsafe {
                    Arc::decrement_strong_count(shared.ptr.as_ptr().cast_const());
                }
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
            UnionType::Shared => {
                let shared = unsafe { &self.shared };

                // We have to increment the strong count before cloning.
                //
                // SAFETY: We know `shared.ptr` was obtained from `Arc::into_raw`. We also know that if we're cloning
                // this value, that the underlying `Arc` must still be live, since we're holding a reference to it.
                unsafe {
                    Arc::increment_strong_count(shared.ptr.as_ptr().cast_const());
                }

                Self {
                    shared: ManuallyDrop::new(SharedUnion {
                        ptr: shared.ptr,
                        _cap: Tag::Shared,
                    }),
                }
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
/// - shared (`Arc<str>`)
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
/// `MetaString` can also be created from `InternedString`, which is a string that has been interned (using an interner
/// like [`FixedSizeInterner`][crate::interning::FixedSizeInterner] or
/// [`GenericMapInterner`][crate::interning::GenericMapInterner]). Interned strings are essentially a combination of the
/// properties of `Arc<T>` -- owned wrappers around an atomically reference counted piece of data -- and a fixed-size
/// buffer, where we allocate one large buffer, and write many small strings into it, and provide references to those
/// strings through `InternedString`.
///
/// ### Shared strings
///
/// `MetaString` can be created from `Arc<str>`, which is a string slice that can be atomically shared between threads.
/// This is a simpler version of interned strings where strict memory control and re-use is not required.
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
    pub const fn from_static(s: &'static str) -> Self {
        Self {
            inner: Inner::static_str(s),
        }
    }

    /// Attempts to create a new `MetaString` from the given string if it can be inlined.
    pub fn try_inline(s: &str) -> Option<Self> {
        Inner::try_inlined(s).map(|inner| Self { inner })
    }

    /// Returns `true` if `self` has a length of zero bytes.
    pub fn is_empty(&self) -> bool {
        self.deref().is_empty()
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

impl From<Arc<str>> for MetaString {
    fn from(s: Arc<str>) -> Self {
        Self {
            inner: Inner::shared(s),
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

#[cfg(test)]
mod tests {
    use std::{num::NonZeroUsize, sync::Arc};

    use proptest::{prelude::*, proptest};

    use super::{interning::GenericMapInterner, InlinedUnion, Inner, MetaString, UnionType};

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
        // We always use the static variant, even if the string is inlineable, because this lets us make `from_static` const.
        let s = "hello";
        let meta = MetaString::from_static(s);

        assert_eq!(meta.inner.get_union_type(), UnionType::Static);
        assert_eq!(s, &*meta);
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

        let interner = GenericMapInterner::new(NonZeroUsize::new(1024).unwrap());
        let s = interner.try_intern(intern_str).unwrap();
        assert_eq!(intern_str, &*s);

        let meta = MetaString::from(s);
        assert_eq!(intern_str, &*meta);
        assert_eq!(meta.inner.get_union_type(), UnionType::Interned);
        assert_eq!(intern_str, meta.into_owned());
    }

    #[test]
    fn shared_string() {
        let shared_str = "hello shared str!";

        let s = Arc::<str>::from(shared_str);
        assert_eq!(shared_str, &*s);

        let meta = MetaString::from(s);
        assert_eq!(shared_str, &*meta);
        assert_eq!(meta.inner.get_union_type(), UnionType::Shared);
        assert_eq!(shared_str, meta.into_owned());
    }

    #[test]
    fn shared_string_clone() {
        let shared_str = "hello shared str!";
        let s = Arc::<str>::from(shared_str);
        let meta = MetaString::from(s);

        // Clone the `MetaString` to make sure we can still access the original string.
        let meta2 = meta.clone();
        assert_eq!(shared_str, &*meta2);

        // Drop the original `MetaString` to ensure we can still access the string from our clone after going through
        // the drop logic for the shared variant.
        drop(meta);
        assert_eq!(shared_str, &*meta2);
    }

    #[test]
    fn empty_string_shared() {
        let shared_str = "";

        let s = Arc::<str>::from(shared_str);
        assert_eq!(shared_str, &*s);

        let meta = MetaString::from(s);
        assert_eq!(shared_str, &*meta);
        assert_eq!(meta.inner.get_union_type(), UnionType::Empty);
        assert_eq!(shared_str, meta.into_owned());
    }

    #[test]
    fn empty_string_interned() {
        let intern_str = "";

        let interner = GenericMapInterner::new(NonZeroUsize::new(1024).unwrap());
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
