use crate::hash::{FastBuildHasher, NoopU64BuildHasher};

/// A hash set based on `hashbrown` ([`HashSet`][hashbrown::HashSet]) using [`FastHasher`][crate::hash::FastHasher].
pub type FastHashSet<T> = hashbrown::HashSet<T, FastBuildHasher>;

/// A hash map based on `hashbrown` ([`HashMap`][hashbrown::HashMap]) using [`FastHasher`][crate::hash::FastHasher].
pub type FastHashMap<K, V> = hashbrown::HashMap<K, V, FastBuildHasher>;

/// A concurrent hash set based on `papaya` ([`HashSet`][papaya::HashSet]) using [`FastHasher`][crate::hash::FastHasher].
pub type FastConcurrentHashSet<T> = papaya::HashSet<T, FastBuildHasher>;

/// A concurrent hash map based on `papaya` ([`HashMap`][papaya::HashMap]) using [`FastHasher`][crate::hash::FastHasher].
pub type FastConcurrentHashMap<K, V> = papaya::HashMap<K, V, FastBuildHasher>;

/// A hash map with stable insertion order based on `indexmap` ([`IndexMap`][indexmap::IndexMap]) using [`FastHasher`][crate::hash::FastHasher].
pub type FastIndexMap<K, V> = indexmap::IndexMap<K, V, FastBuildHasher>;

/// A hash set based on `hashbrown`'s implementation ([`HashSet`][hashbrown::HashSet]) using [`NoopU64Hasher`][crate::hash::NoopU64Hasher].
///
/// This is only suitable for `u64` values, or values which only wrap over a `u64` value. See
/// [`NoopU64Hasher`][crate::hash::NoopU64Hasher] for more details.
pub type PrehashedHashSet<T> = hashbrown::HashSet<T, NoopU64BuildHasher>;

/// A hash map based on `hashbrown`'s implementation ([`HashMap`][hashbrown::HashMap]) using [`NoopU64Hasher`][crate::hash::NoopU64Hasher].
///
/// This is only suitable when using `u64` for the key type, or another type which only wraps over a `u64` value. See
/// [`NoopU64Hasher`][crate::hash::NoopU64Hasher] for more details.
pub type PrehashedHashMap<K, V> = hashbrown::HashMap<K, V, NoopU64BuildHasher>;
