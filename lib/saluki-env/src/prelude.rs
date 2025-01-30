// We specifically alias a number of common hash-based containers here to provide versions that use [`ahash`][ahash] as
// the hasher of choice. It's a high-performance hash implementation that is far faster than the standard library's
// `SipHash` implementation. We don't have a need for DoS resistance in our use case, because we're not dealing with
// adversarial, untrusted data.
//
// While there are other, faster hash implementations -- rapidhash, gxhash, etc -- we're using `ahash` as it is
// well-trodden and provides more than enough performance for our current needs.

/// A hash set based on the standard library's implementation ([`HashSet`][std::collections::HashSet]) using [`ahash`][ahash] as the configured hasher.
pub type FastHashSet<T> = std::collections::HashSet<T, ahash::RandomState>;

/// A hash map based on the standard library's implementation ([`HashMap`][std::collections::HashMap]) using [`ahash`][ahash] as the configured hasher.
pub type FastHashMap<K, V> = std::collections::HashMap<K, V, ahash::RandomState>;

/// A concurrent hash set based on `papaya` ([`HashSet``][papaya::HashSet]) using [`ahash`][ahash] as the configured hasher.
pub type FastConcurrentHashSet<T> = papaya::HashSet<T, ahash::RandomState>;

/// A concurrent hash map based on `papaya` ([`HashMap`][papaya::HashMap]) using [`ahash`][ahash] as the configured hasher.
pub type FastConcurrentHashMap<K, V> = papaya::HashMap<K, V, ahash::RandomState>;

/// A hash map with stable insertion order based on `indexmap` ([`IndexMap`][indexmap::IndexMap]) using [`ahash`][ahash] as the configured hasher.
pub type FastIndexMap<K, V> = indexmap::IndexMap<K, V, ahash::RandomState>;
