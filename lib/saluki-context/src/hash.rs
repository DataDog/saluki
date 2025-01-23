use std::{
    collections::HashSet,
    hash::{Hash as _, Hasher as _},
};

use crate::origin::OriginKey;

pub type FastHashSet<T> = HashSet<T, ahash::RandomState>;

pub fn new_fast_hashset<T>() -> FastHashSet<T> {
    HashSet::with_hasher(ahash::RandomState::default())
}

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
/// Returns a hash that uniquely identifies the combination of name, tags, and origin of the value.
pub fn hash_context<I, T>(name: &str, tags: I, origin_key: Option<OriginKey>) -> ContextKey
where
    I: IntoIterator<Item = T>,
    T: AsRef<str>,
{
    let mut seen = new_fast_hashset();
    hash_context_with_seen(name, tags, origin_key, &mut seen)
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
/// Returns a hash that uniquely identifies the combination of name, tags, and origin of the value.
pub(super) fn hash_context_with_seen<I, T>(
    name: &str, tags: I, origin_key: Option<OriginKey>, seen: &mut FastHashSet<u64>,
) -> ContextKey
where
    I: IntoIterator<Item = T>,
    T: AsRef<str>,
{
    seen.clear();

    let mut hasher = ahash::AHasher::default();
    name.hash(&mut hasher);

    // Hash the tags individually and XOR their hashes together, which allows us to be order-oblivious:
    let mut combined_tags_hash = 0;

    for tag in tags {
        let tag_hash = hash_string(tag.as_ref());

        // If we've already seen this tag before, skip combining it again.
        if !seen.insert(tag_hash) {
            continue;
        }

        combined_tags_hash ^= tag_hash;
    }

    hasher.write_u64(combined_tags_hash);

    let metric_key = hasher.finish();

    ContextKey { metric_key, origin_key }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ContextKey {
    metric_key: u64,
    origin_key: Option<OriginKey>,
}

impl ContextKey {
    pub fn origin_key(&self) -> Option<OriginKey> {
        self.origin_key
    }
}
