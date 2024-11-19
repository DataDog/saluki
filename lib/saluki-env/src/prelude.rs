use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;

/// A [`HashSet`][std::collections::HashSet] using [`ahash`][ahash] as the configured hasher.
pub type FastHashSet<T> = HashSet<T, ahash::RandomState>;

/// A [`HashMap`][std::collections::HashMap] using [`ahash`][ahash] as the configured hasher.
pub type FastHashMap<K, V> = HashMap<K, V, ahash::RandomState>;

/// An [`IndexMap`][indexmap::IndexMap] using [`ahash`][ahash] as the configured hasher.
pub type FastIndexMap<K, V> = IndexMap<K, V, ahash::RandomState>;
