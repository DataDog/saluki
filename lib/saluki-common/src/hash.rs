use std::{
    hash::{BuildHasher as _, Hasher as _},
    sync::LazyLock,
};

/// A fast, non-cryptographic hash implementation that is optimized for quality.
///
/// The implementation is reasonably suitable for hash tables and other data structures that require fast hashing and
/// some degree of collision resistance.
///
/// Currently, [`foldhash`][foldhash] is used as the underlying implementation.
///
/// [foldhash]: http://github.com/orlp/foldhash
pub type FastHasher = foldhash::quality::FoldHasher;

/// [`BuildHasher`][std::hash::BuildHasher] implementation for [`FastHasher`].
pub type FastBuildHasher = foldhash::quality::RandomState;

// Single global instance of the hasher state since we need a consistently-seeded state for `hash_single_fast` to
// consistently hash things across the application.
static BUILD_HASHER: LazyLock<FastBuildHasher> = LazyLock::new(foldhash::quality::RandomState::default);

/// Returns a fresh `FastBuildHasher` instance.
///
/// This instance should not be used to generate hashes that will be compared against hashes generated either with a
/// hasher acquired from `get_fast_hasher` or `hash_single_fast`, and those methods use a shared global state to both
/// speed up hashing and ensure that the hashes are consistent across runs of those functions in particular.
#[inline]
pub fn get_fast_build_hasher() -> FastBuildHasher {
    foldhash::quality::RandomState::default()
}

/// Returns a `FastHasher` instance backed by a shared, global state.
///
/// Values hashed with a `FastHasher` instance created with this method will be consistent within the same process, but
/// will not be consistent across different runs of the application. Additionally, values hashed with this instance will
/// not e consistent with those hashed by `get_fast_build_hasher`, as that function returns a randomly-seeded state for
/// each call.
#[inline]
pub fn get_fast_hasher() -> FastHasher {
    BUILD_HASHER.build_hasher()
}

/// Hashes a single value using the "fast" hash implementation, and returns the 64-bit hash value.
///
/// Utilizes the "fast" hash implementation provided by [`FastHasher`]. See [`get_fast_hasher`] for more details on hash
/// values and the consistency of hash output between different hashing approaches and application states.
#[inline]
pub fn hash_single_fast<H: std::hash::Hash>(value: H) -> u64 {
    let mut hasher = get_fast_hasher();
    value.hash(&mut hasher);
    hasher.finish()
}
