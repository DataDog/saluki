use std::hash::{Hash as _, Hasher as _};

use saluki_common::{
    collections::PrehashedHashSet,
    hash::{get_fast_hasher, hash_single_fast},
};

const MEDIUM_TAG_CAPACITY: usize = 64;

/// Returns hash keys for the metric context and its non-origin tag set.
///
/// Tag order and duplicate tags do not affect either key. Tags and origin tags deduplicate separately.
/// Tag values remain case-sensitive and whitespace-sensitive. The hash keys can collide.
///
/// Small and medium tag sets use stack storage. Large sets can allocate temporary storage for deduplication.
pub fn hash_context<I, I2, T, T2>(name: &str, tags: I, origin_tags: I2) -> (ContextKey, TagSetKey)
where
    I: IntoIterator<Item = T>,
    T: AsRef<str>,
    I2: IntoIterator<Item = T2>,
    T2: AsRef<str>,
{
    let mut seen = PrehashedHashSet::default();
    let (context, tagset, _) = hash_context_with_host_and_seen(name, None, tags, origin_tags, &mut seen);
    (context, tagset)
}

/// Returns hash keys for the metric context with a host and for its non-origin tag set.
///
/// The metric name and host affect only the context key. An unset host and an explicitly empty host are distinct.
/// Tag order and duplicate tags do not affect either key. Tags and origin tags deduplicate separately.
pub fn hash_context_with_host<I, I2, T, T2>(
    name: &str, host: Option<&str>, tags: I, origin_tags: I2,
) -> (ContextKey, TagSetKey)
where
    I: IntoIterator<Item = T>,
    T: AsRef<str>,
    I2: IntoIterator<Item = T2>,
    T2: AsRef<str>,
{
    let mut seen = PrehashedHashSet::default();
    let (context, tagset, _) = hash_context_with_host_and_seen(name, host, tags, origin_tags, &mut seen);
    (context, tagset)
}

/// Computes both keys and reports whether the non-origin tag hashes can represent duplicates.
///
/// A repeated hash can be a duplicate tag or a collision. Distinct hashes rule out duplicate tags.
/// Reuse a `PrehashedHashSet` across contexts to amortize the allocation when a tag set outgrows the stack tables.
pub(super) fn hash_context_with_host_and_seen<I, I2, T, T2>(
    name: &str, host: Option<&str>, tags: I, origin_tags: I2, seen: &mut PrehashedHashSet<u64>,
) -> (ContextKey, TagSetKey, bool)
where
    I: IntoIterator<Item = T>,
    T: AsRef<str>,
    I2: IntoIterator<Item = T2>,
    T2: AsRef<str>,
{
    let mut hasher = get_fast_hasher();

    // Hash the name and host as separate scalar fields. Do not XOR them with tag hashes.
    name.hash(&mut hasher);
    host.hash(&mut hasher);

    let (combined_tags_hash, tags_may_repeat) = hash_tags(tags, seen);
    hasher.write_u64(combined_tags_hash);

    // Origin tags contribute their own order-independent hash. They do not affect the non-origin tag set key.
    hasher.write_u64(hash_tags(origin_tags, seen).0);

    let context_key = ContextKey { hash: hasher.finish() };
    let tagset_key = TagSetKey {
        hash: combined_tags_hash,
    };

    (context_key, tagset_key, tags_may_repeat)
}

/// XORs each distinct tag hash once so tag order does not matter and duplicates cannot cancel a hash.
///
/// An array scans up to 16 distinct hashes. A stack table handles up to 64 distinct nonzero hashes.
/// The iterator's lower bound can skip a small path. Unknown-length iterators promote as needed.
/// The heap fallback reuses the caller's set and inserts the hashes directly without hashing the strings again.
#[inline]
fn hash_tags<I, T>(tags: I, seen: &mut PrehashedHashSet<u64>) -> (u64, bool)
where
    I: IntoIterator<Item = T>,
    T: AsRef<str>,
{
    const INLINE_CAPACITY: usize = 16;

    let mut hashes = tags.into_iter().map(|tag| hash_single_fast(tag.as_ref()));
    let mut combined = 0;
    let mut may_repeat = false;

    let lower_bound = hashes.size_hint().0;
    if lower_bound <= INLINE_CAPACITY {
        let mut inline = [0; INLINE_CAPACITY];
        let mut len = 0;
        for hash in hashes.by_ref() {
            if inline[..len].contains(&hash) {
                may_repeat = true;
                continue;
            }
            if len == INLINE_CAPACITY {
                let (combined, repeats) =
                    hash_medium_tags(inline.into_iter().chain(std::iter::once(hash)).chain(hashes), seen);
                return (combined, may_repeat || repeats);
            }
            combined ^= hash;
            inline[len] = hash;
            len += 1;
        }
        return (combined, may_repeat);
    }
    if lower_bound <= MEDIUM_TAG_CAPACITY {
        return hash_medium_tags(hashes, seen);
    }

    seen.clear();
    for hash in hashes {
        if seen.insert(hash) {
            combined ^= hash;
        } else {
            may_repeat = true;
        }
    }

    (combined, may_repeat)
}

// Keep the stack table at most half full. XOR ignores zero, so zero can also mark empty slots.
fn hash_medium_tags(mut hashes: impl Iterator<Item = u64>, seen: &mut PrehashedHashSet<u64>) -> (u64, bool) {
    const BUCKETS: usize = MEDIUM_TAG_CAPACITY * 2;
    let mut slots = [0; BUCKETS];
    let mut combined = 0;
    let mut len = 0;
    let mut may_repeat = false;
    for hash in hashes.by_ref() {
        let mut index = hash as usize & (BUCKETS - 1);
        loop {
            let slot = &mut slots[index];
            if *slot == hash {
                may_repeat = true;
                break;
            }
            if *slot == 0 {
                *slot = hash;
                combined ^= hash;
                len += 1;
                break;
            }
            index = (index + 1) & (BUCKETS - 1);
        }
        if len == MEDIUM_TAG_CAPACITY {
            if let Some(next) = hashes.next() {
                // Transfer the hashes already counted so the remaining tags cannot XOR them in again.
                seen.clear();
                seen.extend(slots.into_iter().filter(|&slot| slot != 0));
                for hash in std::iter::once(next).chain(hashes) {
                    if seen.insert(hash) {
                        combined ^= hash;
                    } else {
                        may_repeat = true;
                    }
                }
            }
            break;
        }
    }
    (combined, may_repeat)
}

/// A compact hash key that identifies a metric context.
///
/// The key is process-local and collision-prone by nature because it stores a 64-bit hash.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ContextKey {
    hash: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TagSetKey {
    hash: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_hash_matches_hash_set_deduplication() {
        fn baseline_tags(tags: &[String]) -> u64 {
            tags.iter()
                .map(|tag| hash_single_fast(tag.as_str()))
                .collect::<PrehashedHashSet<_>>()
                .into_iter()
                .fold(0, |combined, hash| combined ^ hash)
        }

        let name = "sample.metric";
        let mut seen = PrehashedHashSet::default();
        for (regular_count, origin_count) in [(0, 0), (16, 17), (17, 16), (50, 63), (64, 65), (2, 1)] {
            let build_tags = |count| {
                let mut tags: Vec<_> = (0..count).map(|i| format!("tag:{i}")).collect();
                if let Some(first) = tags.first() {
                    tags.push(first.clone());
                }
                tags
            };
            let tags = build_tags(regular_count);
            let mut origin_tags = build_tags(origin_count);
            origin_tags.reverse();

            for host in [None, Some(""), Some("my-host")] {
                let tags_hash = baseline_tags(&tags);
                let mut hasher = get_fast_hasher();
                name.hash(&mut hasher);
                host.hash(&mut hasher);
                hasher.write_u64(tags_hash);
                hasher.write_u64(baseline_tags(&origin_tags));

                let (context_key, tagset_key, may_repeat) =
                    hash_context_with_host_and_seen(name, host, &tags, &origin_tags, &mut seen);
                assert_eq!(context_key.hash, hasher.finish());
                assert_eq!(tagset_key.hash, tags_hash);
                assert_eq!(may_repeat, regular_count != 0);

                // Filters hide the length, forcing large inputs to spill from the stack while hashing.
                let streaming = hash_context_with_host_and_seen(
                    name,
                    host,
                    tags.iter().filter(|_| true),
                    origin_tags.iter().filter(|_| true),
                    &mut seen,
                );
                assert_eq!(streaming, (context_key, tagset_key, may_repeat));
            }
        }
    }
}
