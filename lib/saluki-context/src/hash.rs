use std::{
    collections::HashSet,
    hash::{self, BuildHasher, Hash as _, Hasher as _},
};

use crate::context::Resolvable;

pub type PrehashedHashSet = HashSet<u64, NoopU64HashBuilder>;

#[inline]
fn hash_string(s: &str) -> u64 {
    let mut hasher = ahash::AHasher::default();
    s.hash(&mut hasher);
    hasher.finish()
}

/// Hashes a `Resolvable`.
///
/// Tags are hashed in an order-oblivious (XOR) manner, which allows tags to be hashed in any order while still
/// resulting in the same overall hash. This function is _not_ oblivious to the actual tag values themselves, though, so
/// differences such as case (lower vs upper) or leading/trailing whitespace will influence the resulting hash.
///
/// If a tag is seen more than once, it will be ignored and not included in the overall hash. This function allocates a
/// hash set in order to track which tags have already been hashed, so it is preferable to allocate a single
/// [`PrehashedHashSet`] and use it with [`hash_context_with_seen`] in order to amortize the cost of allocating the hash
/// set.
///
/// Returns a hash that uniquely identifies the combination of name, tags, and origin info for the value.
pub fn hash_resolvable<R>(value: R) -> ContextHashKey
where
    R: Resolvable,
{
    let mut seen = PrehashedHashSet::with_hasher(NoopU64HashBuilder);
    hash_resolvable_with_seen(value, &mut seen)
}

/// Hashes a `Resolvable`, using a provided set to track which tags have already been hashed.
///
/// Tags are hashed in an order-oblivious (XOR) manner, which allows tags to be hashed in any order while still
/// resulting in the same overall hash. This function is _not_ oblivious to the actual tag values themselves, though, so
/// differences such as case (lower vs upper) or leading/trailing whitespace will influence the resulting hash.
///
/// If a tag is seen more than once, it will be ignored and not included in the overall hash. This function requires
/// the caller to provide the hash set used for tracking duplicates, and is more efficient than [`hash_context`] which
/// allocates a new hash set each time.
///
/// Returns a hash that uniquely identifies the combination of name, tags, and origin info for the value.
pub(super) fn hash_resolvable_with_seen<R>(value: R, seen: &mut PrehashedHashSet) -> ContextHashKey
where
    R: Resolvable,
{
    seen.clear();

    let mut hasher = ahash::AHasher::default();
    value.name().hash(&mut hasher);

    // Hash the tags individually and XOR their hashes together, which allows us to be order-oblivious:
    let mut combined_tags_hash = 0;
    let mut tag_count = 0;

    value.visit_tags(|tag| {
        let tag_hash = hash_string(tag);

        // If we've already seen this tag before, skip combining it again.
        if !seen.insert(tag_hash) {
            return;
        }

        combined_tags_hash ^= tag_hash;
        tag_count += 1;
    });

    hasher.write_u64(combined_tags_hash);

    if let Some(origin_info) = value.origin_info() {
        origin_info.hash(&mut hasher);
    }

    ContextHashKey(hasher.finish())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextHashKey(pub u64);

impl hash::Hash for ContextHashKey {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.0);
    }
}

/// A hasher for pre-hashed keys.
///
/// This hasher is specifically used for context hashing, as we forego the traditional approach of using a key value
/// that can also be checked for equality, as this is slower and we're willing to make the trade-off of using the hash
/// value directly as the context key.
pub struct NoopU64Hasher(u64);

impl hash::Hasher for NoopU64Hasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }

    fn write(&mut self, _: &[u8]) {
        panic!("NoopU64Hasher is only valid for hashing `u64` values");
    }
}

/// Hasher builder for [`NoopU64Hasher`].
#[derive(Clone)]
pub struct NoopU64HashBuilder;

impl BuildHasher for NoopU64HashBuilder {
    type Hasher = NoopU64Hasher;

    fn build_hasher(&self) -> Self::Hasher {
        NoopU64Hasher(0)
    }
}
