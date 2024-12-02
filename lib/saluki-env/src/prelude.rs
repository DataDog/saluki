use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;

// We specifically alias a number of common hash-based containers here to provide versions that use [`ahash`][ahash] as
// the hasher of choice. It's a high-performance hash implementation that is far faster than the standard library's
// `SipHash` implementation. We don't have a need for DoS resistance in our use case, because we're not dealing with
// adversarial, untrusted data.
//
// While there are other, faster hash implementations -- rapidhash, gxhash, etc -- we're using `ahash` as it is
// well-trodden and provides more than enough performance for our current needs.

/// A [`HashSet`][std::collections::HashSet] using [`ahash`][ahash] as the configured hasher.
pub type FastHashSet<T> = HashSet<T, ahash::RandomState>;

/// A [`HashMap`][std::collections::HashMap] using [`ahash`][ahash] as the configured hasher.
pub type FastHashMap<K, V> = HashMap<K, V, ahash::RandomState>;

/// An [`IndexMap`][indexmap::IndexMap] using [`ahash`][ahash] as the configured hasher.
pub type FastIndexMap<K, V> = IndexMap<K, V, ahash::RandomState>;
