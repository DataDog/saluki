use std::{
    hash::{BuildHasher, Hasher},
    sync::LazyLock,
};

use sha3::digest::{ExtendableOutput as _, Update as _};

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

// Single global instance of the fast hasher state since we need a consistently-seeded state for `hash_single_fast` to
// consistently hash things across the application.
static FAST_BUILD_HASHER: LazyLock<FastBuildHasher> = LazyLock::new(get_fast_build_hasher);

/// A no-op hasher that writes `u64` values directly to the internal state.
///
/// In some cases, pre-hashed values (`u64`) may be used as the key to a hash table or similar data structure. In those
/// cases, re-hashing the key each time is unnecessary and potentially even undesirable.
///
/// `NoopU64Hasher` is a hash implementation that simply forwards `u64` values to the internal state and uses that as
/// the final hashed value. It can used to hash a `u64` value directly, or to hash a value that wraps a `u64` value,
/// such as a newtype (e.g., `struct HashKey(u64)`).
///
/// # Behavior
///
/// `NoopU64Hasher` stores a single `u64` value internally. The last value written via `write_u64` is the final hash
/// value. Writing any other value type to the hasher will panic.
#[derive(Default)]
pub struct NoopU64Hasher(u64);

impl Hasher for NoopU64Hasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write_u64(&mut self, i: u64) {
        self.0 = i;
    }

    fn write(&mut self, bytes: &[u8]) {
        panic!("non-u64 value written to NoopU64Hasher: {:?}", bytes);
    }
}

/// A [`BuildHasher`][std::hash::BuildHasher] implementation for [`NoopU64Hasher`].
#[derive(Clone, Default)]
pub struct NoopU64BuildHasher;

impl BuildHasher for NoopU64BuildHasher {
    type Hasher = NoopU64Hasher;

    fn build_hasher(&self) -> Self::Hasher {
        NoopU64Hasher::default()
    }
}

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
    FAST_BUILD_HASHER.build_hasher()
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

/// Hashes a single value using the "stable" hash implementation, and returns the 64-bit hash value.
///
/// Utilizes the "stable" hash implementation provided by [`StableHasher`]. See [`get_fast_hasher`] for more details on
#[inline]
pub fn hash_single_stable<H: std::hash::Hash>(value: H) -> u64 {
    let mut hasher = StableHasher::default();
    value.hash(&mut hasher);
    hasher.finish()
}

/// A non-cryptographic hash implementation that is meant to be stable over time.
///
/// The implementation is intended to be stable over time and across different runs of the applications, such that it
/// can be depended on across different builds/versions of Saluki.
///
/// At a minimum, the hasher implementation will not change within major versions of Saluki, including v0 and v1.
///
/// Currently, [`sha3`][sha3] (specifically SHAKE128) is used as the underlying implementation. While SHAKE128 is a
/// cryptographic hash algorithm, the way it is used effectively makes it a non-cryptographic hash algorithm given how
/// much the output is truncated.
///
/// [sha3]: https://crates.io/crates/sha3
#[derive(Default)]
pub struct StableHasher(sha3::Shake128);

impl std::hash::Hasher for StableHasher {
    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    fn finish(&self) -> u64 {
        let mut buf = [0; std::mem::size_of::<u64>()];
        self.0.clone().finalize_xof_into(&mut buf);

        u64::from_le_bytes(buf)
    }
}
