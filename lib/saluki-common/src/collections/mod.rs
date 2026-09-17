use crate::hash::{FastBuildHasher, NoopU64BuildHasher};

mod bitset;
pub use self::bitset::ContiguousBitSet;

/// Finds the entry whose prefix matches `value` in a sorted, non-overlapping prefix table.
///
/// `entries` must be sorted lexicographically by the prefix returned from `prefix`. Distinct
/// prefixes must not overlap. Under those conditions, a matching prefix is either an exact match
/// for `value` or the entry immediately before its insertion point.
pub fn find_matching_prefix<'a, T>(entries: &'a [T], value: &str, prefix: impl Fn(&T) -> &str) -> Option<&'a T> {
    match entries.binary_search_by(|candidate| prefix(candidate).cmp(value)) {
        Ok(index) => Some(&entries[index]),
        Err(index) if index > 0 && value.starts_with(prefix(&entries[index - 1])) => Some(&entries[index - 1]),
        _ => None,
    }
}

/// A hash set based on the standard library's ([`HashSet`][std::collections::HashSet]) using [`FastHasher`][crate::hash::FastHasher].
pub type FastHashSet<T> = std::collections::HashSet<T, FastBuildHasher>;

/// A hash map based on the standard library's ([`HashMap`][std::collections::HashMap]) using [`FastHasher`][crate::hash::FastHasher].
pub type FastHashMap<K, V> = std::collections::HashMap<K, V, FastBuildHasher>;

/// A concurrent hash set based on `papaya` ([`HashSet`][papaya::HashSet]) using [`FastHasher`][crate::hash::FastHasher].
pub type FastConcurrentHashSet<T> = papaya::HashSet<T, FastBuildHasher>;

/// A concurrent hash map based on `papaya` ([`HashMap`][papaya::HashMap]) using [`FastHasher`][crate::hash::FastHasher].
pub type FastConcurrentHashMap<K, V> = papaya::HashMap<K, V, FastBuildHasher>;

/// A hash map with stable insertion order based on `indexmap` ([`IndexMap`][indexmap::IndexMap]) using [`FastHasher`][crate::hash::FastHasher].
pub type FastIndexMap<K, V> = indexmap::IndexMap<K, V, FastBuildHasher>;

/// A hash set with stable insertion order based on `indexset` ([`IndexSet`][indexmap::IndexSet]) using [`FastHasher`][crate::hash::FastHasher].
pub type FastIndexSet<K> = indexmap::IndexSet<K, FastBuildHasher>;

/// A hash set based on the standard library's ([`HashSet`][std::collections::HashSet]) using [`NoopU64Hasher`][crate::hash::NoopU64Hasher].
///
/// This is only suitable for `u64` values, or values which only wrap over a `u64` value. See
/// [`NoopU64Hasher`][crate::hash::NoopU64Hasher] for more details.
pub type PrehashedHashSet<T> = std::collections::HashSet<T, NoopU64BuildHasher>;

/// A hash map based on the standard library's ([`HashMap`][std::collections::HashMap]) using [`NoopU64Hasher`][crate::hash::NoopU64Hasher].
///
/// This is only suitable when using `u64` for the key type, or another type which only wraps over a `u64` value. See
/// [`NoopU64Hasher`][crate::hash::NoopU64Hasher] for more details.
pub type PrehashedHashMap<K, V> = std::collections::HashMap<K, V, NoopU64BuildHasher>;

#[cfg(test)]
mod tests {
    use super::find_matching_prefix;

    #[test]
    fn finds_a_prefix_or_reports_a_miss() {
        let prefixes = ["alpha.", "middle.", "zulu."];

        assert_eq!(
            find_matching_prefix(&prefixes, "middle.requests", |prefix| *prefix),
            Some(&"middle.")
        );
        assert_eq!(
            find_matching_prefix(&prefixes, "zulu.", |prefix| *prefix),
            Some(&"zulu.")
        );
        assert_eq!(
            find_matching_prefix(&prefixes, "between.requests", |prefix| *prefix),
            None
        );
        assert_eq!(find_matching_prefix(&prefixes, "aardvark", |prefix| *prefix), None);
    }
}
