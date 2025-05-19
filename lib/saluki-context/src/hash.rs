use std::hash::{Hash as _, Hasher as _};

use saluki_common::{
    collections::PrehashedHashSet,
    hash::{get_fast_hasher, hash_single_fast},
};

use crate::{origin::OriginKey, resolver::ContextTag};

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
    T: ContextTag,
{
    let mut seen = PrehashedHashSet::default();
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
    name: &str, tags: I, origin_key: Option<OriginKey>, seen: &mut PrehashedHashSet<u64>,
) -> ContextKey
where
    I: IntoIterator<Item = T>,
    T: ContextTag,
{
    seen.clear();

    let mut hasher = get_fast_hasher();

    // Hash the metric name.
    name.hash(&mut hasher);

    // Hash the metric tags individually and XOR their hashes together, which allows us to be order-oblivious:
    let mut combined_tags_hash = 0;

    for tag in tags {
        let tag_hash = hash_single_fast(tag.as_str());

        // If we've already seen this tag before, skip combining it again.
        if !seen.insert(tag_hash) {
            continue;
        }

        combined_tags_hash ^= tag_hash;
    }

    hasher.write_u64(combined_tags_hash);

    // Finally, hash the origin key.
    if let Some(origin_key) = origin_key {
        origin_key.hash(&mut hasher);
    }

    ContextKey { hash: hasher.finish() }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ContextKey {
    hash: u64,
}
