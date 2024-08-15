use std::{mem::ManuallyDrop, ops::Deref, slice::from_raw_parts, str::from_utf8_unchecked};

use crate::interning::InternedString;

const OWNED_PTR_TAG: usize = 0xFF00_0000_0000_0000;

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

#[repr(C)]
#[derive(Clone, Copy)]
struct OwnedUnion {
    cap: usize,   // Field one.
    len: usize,   // Field two.
    ptr: *mut u8, // Field three.
}

#[repr(C)]
#[derive(Clone, Copy)]
struct StaticUnion {
    _padding1: Unused, // Field one.
    len: usize,        // Field two.
    ptr: *mut u8,      // Field three.
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
    data: [u8; 24], // Fields one, two, and three.
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
    fn empty() -> Self {
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
        if len > 23 {
            return None;
        }

        // SAFETY: We know it fits because we just checked that the string length is 23 or less.
        let len_b = len as u8;

        let mut data = [0; 24];
        data[23] = len_b;

        let buf = value.as_bytes();
        data[0..len].copy_from_slice(buf);

        Some(Self {
            inlined: InlinedUnion { data },
        })
    }

    fn get_str_ref(&self) -> &str {
        let union_type = unsafe { self.discriminant.get_union_type() };
        match union_type {
            UnionType::Empty => "",
            UnionType::Owned => {
                let owned = unsafe { &self.owned };

                // Mask out our top byte discriminant value before dereferencing the pointer.
                let ptr = (owned.ptr as usize & !OWNED_PTR_TAG) as *const _;

                // SAFETY: The pointer can't be null if we have a non-zero capacity, as that implies having to have
                // allocated memory, which can't come via a null pointer.
                unsafe { from_utf8_unchecked(from_raw_parts(ptr, owned.len)) }
            }
            UnionType::Static => {
                let _static = unsafe { &self._static };

                // SAFETY: The pointer can't be null if it's from a valid reference.
                unsafe { from_utf8_unchecked(from_raw_parts(_static.ptr as *const _, _static.len)) }
            }
            UnionType::Interned => {
                let interned = unsafe { &self.interned };
                &interned.state
            }
            UnionType::Inlined => {
                let inlined = unsafe { &self.inlined };
                let len = inlined.data[23] as usize;

                // SAFETY: We know the length is valid because we just read it from the inlined data.
                unsafe { from_utf8_unchecked(&inlined.data[0..len]) }
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

struct MetaString2 {
    inner: Inner,
}

impl MetaString2 {
    pub fn from_static(s: &'static str) -> Self {
        Self {
            inner: Inner::static_str(s),
        }
    }
}

impl Deref for MetaString2 {
    type Target = str;

    fn deref(&self) -> &str {
        self.inner.get_str_ref()
    }
}

impl From<String> for MetaString2 {
    fn from(s: String) -> Self {
        MetaString2 { inner: Inner::owned(s) }
    }
}

impl From<&str> for MetaString2 {
    fn from(s: &str) -> Self {
        match Inner::try_inlined(s) {
            Some(inner) => Self { inner },
            None => Self {
                inner: Inner::owned(s.to_string()),
            },
        }
    }
}

impl From<InternedString> for MetaString2 {
    fn from(s: InternedString) -> Self {
        Self {
            inner: Inner::interned(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use super::*;
    use crate::interning::FixedSizeInterner;

    #[test]
    fn static_str() {
        let s = "hello";
        let meta = MetaString2::from_static(s);

        assert_eq!(s, &*meta);
    }

    #[test]
    fn owned_string() {
        let s = String::from("hello");
        let meta = MetaString2::from(s);

        assert_eq!("hello", &*meta);
    }

    #[test]
    fn inlined_string() {
        let s = "hello";
        let meta = MetaString2::from(s);

        assert_eq!("hello", &*meta);
    }

    #[test]
    fn interned_string() {
        let intern_str = "hello interned str!";

        let interner = FixedSizeInterner::<1>::new(NonZeroUsize::new(1024).unwrap());
        let s = interner.try_intern(intern_str).unwrap();
        assert_eq!(intern_str, &*s);

        let meta = MetaString2::from(s);
        assert_eq!(intern_str, &*meta);
    }
}
