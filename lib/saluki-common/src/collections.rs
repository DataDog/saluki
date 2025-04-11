/// A hash set based on the `hashbrown`'s implementation ([`HashSet`][hashbrown::HashSet]) using [`foldhash`][foldhash] as the configured hasher.
pub type FastHashSet<T> = hashbrown::HashSet<T, foldhash::quality::RandomState>;

/// A hash map based on the `hashbrown`'s implementation ([`HashMap`][hashbrown::HashMap]) using [`foldhash`][foldhash] as the configured hasher.
pub type FastHashMap<K, V> = hashbrown::HashMap<K, V, foldhash::quality::RandomState>;

/// A concurrent hash set based on `papaya` ([`HashSet``][papaya::HashSet]) using [`foldhash`][foldhash] as the configured hasher.
pub type FastConcurrentHashSet<T> = papaya::HashSet<T, foldhash::quality::RandomState>;

/// A concurrent hash map based on `papaya` ([`HashMap`][papaya::HashMap]) using [`foldhash`][foldhash] as the configured hasher.
pub type FastConcurrentHashMap<K, V> = papaya::HashMap<K, V, foldhash::quality::RandomState>;

/// A hash map with stable insertion order based on `indexmap` ([`IndexMap`][indexmap::IndexMap]) using [`foldhash`][foldhash] as the configured hasher.
pub type FastIndexMap<K, V> = indexmap::IndexMap<K, V, foldhash::quality::RandomState>;
