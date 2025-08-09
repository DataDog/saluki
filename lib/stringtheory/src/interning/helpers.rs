use std::{alloc::Layout, hash::BuildHasher as _, num::NonZeroUsize, sync::LazyLock};

const PACKED_CAP_MASK: usize = usize::MAX << (usize::BITS / 2);
const PACKED_CAP_SHIFT: u32 = usize::BITS / 2;
const PACKED_LEN_MASK: usize = usize::MAX >> (usize::BITS / 2);

#[inline]
pub fn hash_string(s: &str) -> u64 {
    // Single global instance of the hasher state since we need a consistently-seeded state for `hash_single_fast` to
    // consistently hash things across the application.
    static BUILD_HASHER: LazyLock<foldhash::fast::RandomState> = LazyLock::new(foldhash::fast::RandomState::default);

    BUILD_HASHER.hash_one(s)
}

/// Rounds up `len` to the nearest multiple of the alignment of `T`.
pub const fn aligned<T>(len: usize) -> usize {
    let align = std::mem::align_of::<T>();
    len.wrapping_add(align).wrapping_sub(1) & !align.wrapping_sub(1)
}

/// Gets the number of bytes for the given string, rounded up to the nearest multiple of the alignment of `T`.
pub const fn aligned_string<T>(s: &str) -> usize {
    aligned::<T>(s.len())
}

/// A packed length/capacity representation in a single `usize`.
///
/// The upper half of the `usize` contains the capacity, while the lower half contains the length. This is useful for
/// tracking both the capacity and length of a value in a more efficient way, when the known upper bound on the size of
/// the values can be represented in half of the bits of `usize`, which on 64-bit platforms means being less than or
/// equal to 4GB.
pub struct PackedLengthCapacity(usize);

impl PackedLengthCapacity {
    /// Creates a new packed length/capacity value.
    pub const fn new(capacity: usize, len: usize) -> Self {
        Self((capacity << PACKED_CAP_SHIFT) | len)
    }

    /// Returns the capacity from the packed value.
    pub const fn capacity(&self) -> usize {
        (self.0 & PACKED_CAP_MASK) >> PACKED_CAP_SHIFT
    }

    /// Returns the length from the packed value.
    pub const fn len(&self) -> usize {
        self.0 & PACKED_LEN_MASK
    }

    /// Returns the maximum capacity or length that can be stored in the packed representation.
    pub const fn maximum_value() -> usize {
        (usize::MAX & PACKED_CAP_MASK) >> PACKED_CAP_SHIFT
    }
}

impl std::fmt::Debug for PackedLengthCapacity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PackedLengthCapacity")
            .field("cap", &self.capacity())
            .field("len", &self.len())
            .finish()
    }
}

/// Computes the layout for the given `size` aligned to `T`.
///
/// The resulting layout will always be well-aligned for `T`, such that the given size will be rounded up to the nearest
/// multiple of the alignment of `T`. This means that if `size` is smaller than the alignment of `T`, the resulting
/// layout will at least large enough to hold a single `T`.
pub const fn layout_for_data<T>(size: NonZeroUsize) -> Layout {
    let element_len = std::mem::size_of::<T>();
    let element_align = std::mem::align_of::<T>();
    assert!(element_len > 0, "`T` cannot be a zero-sized type");

    // Round up the capacity to the nearest multiple of the element size.
    let size = size.get().div_ceil(element_len) * element_len;

    // SAFETY: The size of the layout cannot be zero (since `capacity` is non-zero and we always divide rounding up,
    // meaning `size` will always be at least `element_len`), and alignment comes directly from `std::mem::align_of`,
    // where alignment preconditions are already upheld.
    //
    // SAFETY: The caller is responsible for ensuring that `capacity`, when rounded up to `element_align`, does not
    // overflow `isize` (i.e. less than or equal to `isize::MAX`).
    unsafe { Layout::from_size_align_unchecked(size, element_align) }
}
