use std::{
    collections::HashSet,
    hash::{self, BuildHasher, Hash as _, Hasher as _},
};

pub type PrehashedHashSet = HashSet<u64, NoopU64HashBuilder>;

/// Hashes a name and tags.
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
/// Returns the hash of the name and tags, as well as the number of unique tags that were hashed.
pub fn hash_context<I, T>(name: &str, tags: I) -> (u64, usize)
where
    I: IntoIterator<Item = T>,
    T: hash::Hash,
{
    let mut seen = PrehashedHashSet::with_hasher(NoopU64HashBuilder);
    hash_context_with_seen(name, tags, &mut seen)
}

/// Hashes a name and tags, using a provided set to track which tags have already been hashed.
///
/// Tags are hashed in an order-oblivious (XOR) manner, which allows tags to be hashed in any order while still
/// resulting in the same overall hash. This function is _not_ oblivious to the actual tag values themselves, though, so
/// differences such as case (lower vs upper) or leading/trailing whitespace will influence the resulting hash.
///
/// If a tag is seen more than once, it will be ignored and not included in the overall hash. This function requires
/// the caller to provide the hash set used for tracking duplicates, and is more efficient than [`hash_context`] which
/// allocates a new hash set each time.
///
/// Returns the hash of the name and tags, as well as the number of unique tags that were hashed.
pub fn hash_context_with_seen<I, T>(name: &str, tags: I, seen: &mut PrehashedHashSet) -> (u64, usize)
where
    I: IntoIterator<Item = T>,
    T: hash::Hash,
{
    seen.clear();

    let mut hasher = ahash::AHasher::default();
    name.hash(&mut hasher);

    // Hash the tags individually and XOR their hashes together, which allows us to be order-oblivious:
    let mut combined_tags_hash = 0;
    let mut tag_count = 0;
    for tag in tags {
        let mut tag_hasher = ahash::AHasher::default();
        tag.hash(&mut tag_hasher);
        let tag_hash = tag_hasher.finish();

        // If we've already seen this tag before, skip combining it again.
        if !seen.insert(tag_hash) {
            continue;
        }

        combined_tags_hash ^= tag_hash;
        tag_count += 1;
    }

    hasher.write_u64(combined_tags_hash);

    (hasher.finish(), tag_count)
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
