//! Series identity and the Go-compatible storage key.
//!
//! A series is identified by `(namespace, name, host, tag set, aggregate)`. Tags are an unordered set:
//! order does not matter and duplicates do not change identity. The host is carried separately from tags
//! and an unset host is distinct from an explicitly empty one.
//!
//! [`context_key`] and [`storage_key`] reproduce the Agent's Go hashing bit-for-bit:
//!
//! * `context_key` is `pkg/aggregator/ckey`'s `ContextKey`: the per-tag contribution is the XOR of the
//!   MurmurHash3 x64 128-bit upper halves of the unique tags, then a two-step `SeedStringSum128` chain mixes
//!   in the name and then the host, returning the upper 64 bits.
//! * `storage_key` is `observer/impl/storage.go`'s `storageKeyForContextKey`: `avalanche64(context_key ^
//!   fnv1a64(namespace))`, where `avalanche64` is the MurmurHash3 64-bit finalizer.
//!
//! # Examples
//!
//! ```
//! use saluki_anomaly_detection::identity::{context_key, storage_key};
//!
//! let tags = vec![
//!     "bar".to_string(),
//!     "foo".to_string(),
//!     "key:value".to_string(),
//!     "key:value2".to_string(),
//! ];
//! // Ported from the Go `ckey` test vectors.
//! assert_eq!(context_key("metric.name", "hostname", &tags), 0x1e923504c1aad3ad);
//! assert_eq!(storage_key("parquet", "metric.name", "hostname", &tags), 0x5360f1417d052cd6);
//! ```

use std::collections::BTreeSet;
use std::fmt::{self, Display, Formatter};

// MurmurHash3 x64 128-bit mixing constants (twmb/murmur3 `c1_128`, `c2_128`).
const MURMUR_C1: u64 = 0x87c37b91114253d5;
const MURMUR_C2: u64 = 0x4cf5ad432745937f;

// FNV-1a 64-bit constants from `observer/impl/storage.go`.
const FNV_OFFSET_BASIS_64: u64 = 14_695_981_039_346_656_037;
const FNV_PRIME_64: u64 = 1_099_511_628_211;

/// Identifier for the component or data stream that produced a series.
///
/// The namespace is a mandatory part of series identity and is kept separate from the metric name so that
/// series from different sources (for example a Parquet replay namespace and a log extractor namespace)
/// never merge, even when their names collide.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NamespaceId(String);

impl NamespaceId {
    /// Creates a namespace identifier from any string-like value.
    pub fn new(namespace: impl Into<String>) -> Self {
        Self(namespace.into())
    }

    /// Returns the namespace as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for NamespaceId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for NamespaceId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl AsRef<str> for NamespaceId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Display for NamespaceId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The aggregation applied when a series is read.
///
/// Aggregate is a read-time dimension, not a separate ingestion series. The ordinals match the Go
/// `observer.Aggregate` enum (0 = none, 1 = average, 2 = sum, 3 = count), which matters for tie-breaking
/// where the Go code orders by aggregate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Aggregate {
    /// No aggregation; raw stored values are surfaced as-is.
    None = 0,
    /// Arithmetic mean of the stored samples.
    Average = 1,
    /// Sum of the stored samples.
    Sum = 2,
    /// Number of stored samples.
    Count = 3,
}

impl Aggregate {
    /// Returns the Go enum ordinal for this aggregate.
    pub const fn ordinal(self) -> u8 {
        self as u8
    }

    /// Returns the short string label used by the Go model (`none`, `avg`, `sum`, `count`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Average => "avg",
            Self::Sum => "sum",
            Self::Count => "count",
        }
    }
}

impl Display for Aggregate {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A compact numeric handle for a stored time series.
///
/// Storage assigns a ref when a series key is first created. A ref stays stable while the series is live
/// and is never reused within a storage instance, so a ref observed before eviction is invalid afterwards
/// rather than silently pointing at a different series.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SeriesRef(u64);

impl SeriesRef {
    /// Wraps a raw numeric ref value.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw numeric ref value.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl Display for SeriesRef {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Allocates [`SeriesRef`]s monotonically; refs are never reused.
///
/// The allocator hands out increasing refs starting at 0 so that allocation order is deterministic, which
/// the Go implementation relies on for capacity and contributor tie-breaking.
#[derive(Clone, Debug, Default)]
pub struct SeriesRefAllocator {
    next: u64,
}

impl SeriesRefAllocator {
    /// Creates an allocator whose next ref is 0.
    pub fn new() -> Self {
        Self { next: 0 }
    }

    /// Allocates the next ref and advances the counter.
    pub fn allocate(&mut self) -> SeriesRef {
        let series = SeriesRef(self.next);
        self.next += 1;
        series
    }

    /// Returns how many refs have been allocated so far.
    pub fn allocated(&self) -> u64 {
        self.next
    }
}

/// Pairs a storage ref with the aggregate that produced it.
///
/// The pair is the smallest amount of information needed to produce the compact identifier the Agent API
/// uses as a stable join key across endpoints (`42:avg`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct QueryHandle {
    /// The storage series ref.
    pub series: SeriesRef,
    /// The aggregate this handle reads.
    pub aggregate: Aggregate,
}

impl QueryHandle {
    /// Creates a handle from a series ref and an aggregate.
    pub const fn new(series: SeriesRef, aggregate: Aggregate) -> Self {
        Self { series, aggregate }
    }

    /// Returns the compact identifier (`<ref>:<aggregate>`), for example `42:avg`.
    pub fn compact_id(&self) -> String {
        format!("{}:{}", self.series, self.aggregate)
    }
}

/// The fully resolved identity of a time series.
///
/// Unlike [`SeriesDescriptor`], the tags here are normalized: sorted and deduplicated. Two identities are
/// equal exactly when they name the same series, regardless of the original tag order.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SeriesIdentity {
    /// Namespace of the producing component.
    pub namespace: NamespaceId,
    /// Base metric name.
    pub name: String,
    /// Host dimension, carried separately from tags. `None` is distinct from `Some("")`.
    pub host: Option<String>,
    /// The unique tags of the series, sorted lexicographically and deduplicated.
    pub tags: Vec<String>,
    /// Read-time aggregation.
    pub aggregate: Aggregate,
}

impl SeriesIdentity {
    /// Builds an identity, normalizing the tags (sorted and deduplicated).
    pub fn new(
        namespace: impl Into<NamespaceId>, name: impl Into<String>, host: Option<String>,
        tags: impl IntoIterator<Item = String>, aggregate: Aggregate,
    ) -> Self {
        let tags = normalize_tags(tags);
        Self {
            namespace: namespace.into(),
            name: name.into(),
            host,
            tags,
            aggregate,
        }
    }

    /// Returns the Go-compatible context key for this identity.
    ///
    /// An unset host is treated as the empty string, matching the Go model's single host field; identity
    /// comparison still distinguishes the two.
    pub fn context_key(&self) -> u64 {
        context_key(&self.name, self.host.as_deref().unwrap_or(""), &self.tags)
    }

    /// Returns the Go-compatible 64-bit storage key for this identity.
    pub fn storage_key(&self) -> u64 {
        storage_key_for_context_key(self.namespace.as_str(), self.context_key())
    }

    /// Returns the canonical string key, matching the Go `SeriesDescriptor.Key()` format:
    /// `namespace|name:aggregate|host|tag1,tag2,...` with tags sorted and deduplicated.
    pub fn key(&self) -> String {
        format!(
            "{}|{}:{}|{}|{}",
            self.namespace,
            self.name,
            self.aggregate,
            self.host.as_deref().unwrap_or(""),
            self.tags.join(",")
        )
    }
}

/// The display-focused descriptor of a series.
///
/// This carries what is needed to display and key a series — namespace, name, host, tags, and aggregate —
/// without normalizing the tags. Use [`SeriesDescriptor::identity`] to get a canonical, comparable form.
///
/// Equality and hashing intentionally go through the canonical identity, so two descriptors with the same
/// tags in a different order (or with duplicates) are equal, and an unset host is distinct from an empty
/// one.
#[derive(Clone, Debug)]
pub struct SeriesDescriptor {
    /// Namespace of the producing component.
    pub namespace: NamespaceId,
    /// Base metric name.
    pub name: String,
    /// Host dimension, carried separately from tags. `None` is distinct from `Some("")`.
    pub host: Option<String>,
    /// Series-level tags, in their original order and including any duplicates.
    pub tags: Vec<String>,
    /// Read-time aggregation.
    pub aggregate: Aggregate,
}

impl SeriesDescriptor {
    /// Builds a descriptor from its parts.
    pub fn new(
        namespace: impl Into<NamespaceId>, name: impl Into<String>, host: Option<String>, tags: Vec<String>,
        aggregate: Aggregate,
    ) -> Self {
        Self {
            namespace: namespace.into(),
            name: name.into(),
            host,
            tags,
            aggregate,
        }
    }

    /// Returns the canonical identity of this descriptor.
    pub fn identity(&self) -> SeriesIdentity {
        SeriesIdentity::new(
            self.namespace.clone(),
            self.name.clone(),
            self.host.clone(),
            self.tags.clone(),
            self.aggregate,
        )
    }
}

impl PartialEq for SeriesDescriptor {
    fn eq(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }
}

impl Eq for SeriesDescriptor {}

impl std::hash::Hash for SeriesDescriptor {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.identity().hash(state);
    }
}

/// Returns the sorted, deduplicated tags of a series.
fn normalize_tags(tags: impl IntoIterator<Item = String>) -> Vec<String> {
    let set: BTreeSet<String> = tags.into_iter().collect();
    set.into_iter().collect()
}

/// Computes the Go-compatible context key for `(name, host, tags)`.
///
/// The host is a plain string here; pass `""` when the host is unset. Tags may be in any order and may
/// contain duplicates. This is the direct port of `ckey.KeyGenerator.Generate`.
pub fn context_key(name: &str, host: &str, tags: &[String]) -> u64 {
    let tags_hash = tags_hash(tags);
    combine_hash(name, host, tags_hash)
}

/// Computes the Go-compatible 64-bit storage key for a raw identity.
///
/// This combines [`context_key`] with the namespace hash, as `storage.go` does for raw storage and query
/// callers.
pub fn storage_key(namespace: &str, name: &str, host: &str, tags: &[String]) -> u64 {
    storage_key_for_context_key(namespace, context_key(name, host, tags))
}

/// Computes the Go-compatible storage key from a precomputed context key.
///
/// This is `storageKeyForContextKey`: `avalanche64(context_key ^ fnv1a64(namespace))`.
pub fn storage_key_for_context_key(namespace: &str, context_key: u64) -> u64 {
    avalanche64(context_key ^ fnv1a64(namespace))
}

/// Computes the XOR of the MurmurHash3 64-bit hashes of the unique tags.
///
/// Order does not matter because XOR is commutative, and duplicates are dropped exactly as the Go
/// `tagset.HashGenerator` drops them.
pub fn tags_hash(tags: &[String]) -> u64 {
    let mut hash: u64 = 0;
    let mut seen: Vec<&str> = Vec::with_capacity(tags.len());
    for tag in tags {
        if seen.contains(&tag.as_str()) {
            continue;
        }
        seen.push(tag.as_str());
        hash ^= murmur3_string_sum64(tag);
    }
    hash
}

/// Mixes the tag hash with the name and host, as `ckey.KeyGenerator.combineHash` does.
fn combine_hash(name: &str, host: &str, tags_hash: u64) -> u64 {
    let (mut i, mut j) = (tags_hash, tags_hash);
    let (next_i, next_j) = murmur3_seed_string_sum128(i, j, name);
    i = next_i;
    j = next_j;
    let (next_i, _) = murmur3_seed_string_sum128(i, j, host);
    next_i
}

/// Computes FNV-1a over the bytes of a string.
///
/// This mirrors the Go `fnv64aString` helper in `observer/impl/storage.go`.
pub fn fnv1a64(value: &str) -> u64 {
    let mut hash = FNV_OFFSET_BASIS_64;
    for byte in value.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME_64);
    }
    hash
}

/// The MurmurHash3 64-bit finalizer, as used by `storage.go`'s `avalanche64`.
pub fn avalanche64(mut value: u64) -> u64 {
    value ^= value >> 33;
    value = value.wrapping_mul(0xff51afd7ed558ccd);
    value ^= value >> 33;
    value = value.wrapping_mul(0xc4ceb9fe1a85ec53);
    value ^= value >> 33;
    value
}

/// Returns `murmur3.StringSum64(value)`, the upper 64 bits of the 128-bit sum with zero seeds.
fn murmur3_string_sum64(value: &str) -> u64 {
    let (h1, _) = murmur3_x64_128(value.as_bytes(), 0, 0);
    h1
}

/// Returns `murmur3.SeedStringSum128(seed1, seed2, value)`.
fn murmur3_seed_string_sum128(seed1: u64, seed2: u64, value: &str) -> (u64, u64) {
    murmur3_x64_128(value.as_bytes(), seed1, seed2)
}

/// MurmurHash3 x64 128-bit, matching twmb/murmur3 v1.2.0's one-shot `sum128`.
///
/// Data is consumed as little-endian 16-byte blocks; the final partial block is mixed as two
/// little-endian words (`k1` from the first bytes, `k2` from any bytes past the eighth).
fn murmur3_x64_128(data: &[u8], seed1: u64, seed2: u64) -> (u64, u64) {
    let mut h1 = seed1;
    let mut h2 = seed2;

    let mut chunks = data.chunks_exact(16);
    for chunk in &mut chunks {
        let k1 = u64::from_le_bytes(chunk[0..8].try_into().expect("chunk is 16 bytes"));
        let k2 = u64::from_le_bytes(chunk[8..16].try_into().expect("chunk is 16 bytes"));
        mix_block(&mut h1, &mut h2, k1, k2);
    }

    let tail = chunks.remainder();
    if !tail.is_empty() {
        let mut k1: u64 = 0;
        let mut k2: u64 = 0;
        for (index, byte) in tail.iter().enumerate() {
            if index < 8 {
                k1 |= u64::from(*byte) << (8 * index);
            } else {
                k2 |= u64::from(*byte) << (8 * (index - 8));
            }
        }

        if tail.len() > 8 {
            k2 = k2.wrapping_mul(MURMUR_C2);
            k2 = k2.rotate_left(33);
            k2 = k2.wrapping_mul(MURMUR_C1);
            h2 ^= k2;
        }
        k1 = k1.wrapping_mul(MURMUR_C1);
        k1 = k1.rotate_left(31);
        k1 = k1.wrapping_mul(MURMUR_C2);
        h1 ^= k1;
    }

    let length = data.len() as u64;
    h1 ^= length;
    h2 ^= length;

    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);

    h1 = avalanche64(h1);
    h2 = avalanche64(h2);

    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);

    (h1, h2)
}

/// Folds one 16-byte block into the running hash, matching twmb/murmur3's `mix128`.
fn mix_block(h1: &mut u64, h2: &mut u64, k1: u64, k2: u64) {
    let k1 = k1.wrapping_mul(MURMUR_C1).rotate_left(31).wrapping_mul(MURMUR_C2);
    *h1 ^= k1;
    *h1 = h1.rotate_left(27).wrapping_add(*h2);
    *h1 = h1.wrapping_mul(5).wrapping_add(0x52dce729);

    let k2 = k2.wrapping_mul(MURMUR_C2).rotate_left(33).wrapping_mul(MURMUR_C1);
    *h2 ^= k2;
    *h2 = h2.rotate_left(31).wrapping_add(*h1);
    *h2 = h2.wrapping_mul(5).wrapping_add(0x38495ab5);
}

#[cfg(test)]
mod tests {
    use super::*;

    // The four canonical tags from the Go `ckey` test suite.
    fn canonical_tags() -> Vec<String> {
        vec![
            "bar".to_string(),
            "foo".to_string(),
            "key:value".to_string(),
            "key:value2".to_string(),
        ]
    }

    #[test]
    fn context_key_matches_go_vectors() {
        // Ported verbatim from `pkg/aggregator/ckey/key_test.go` (TestGenerateReproductible).
        let tags = canonical_tags();
        assert_eq!(context_key("metric.name", "hostname", &tags), 0x1e923504c1aad3ad);
        assert_eq!(context_key("othername", "hostname", &tags), 0xd298ae9740130f30);
    }

    #[test]
    fn tags_key_matches_go_vector() {
        // Ported from `key_test.go` (TestGenerateReproductible2): XOR of the unique tag hashes.
        let tags = canonical_tags();
        assert_eq!(tags_hash(&tags), 0x437b13a371a1c7d3);
    }

    #[test]
    fn storage_key_matches_go_vectors() {
        // Generated against the Go `ckey` package plus the `storage.go` namespace hashing.
        let tags = canonical_tags();
        assert_eq!(
            storage_key("parquet", "metric.name", "hostname", &tags),
            0x5360f1417d052cd6
        );
        assert_eq!(
            storage_key("parquet", "othername", "hostname", &tags),
            0x210873c5c108d224
        );
    }

    #[test]
    fn storage_key_uses_the_namespace() {
        let tags = canonical_tags();
        // Same series identity, different namespace -> different storage key.
        assert_ne!(
            storage_key("parquet", "metric.name", "hostname", &tags),
            storage_key("log_metrics_extractor", "metric.name", "hostname", &tags)
        );
        assert_eq!(
            storage_key("log_metrics_extractor", "metric.name", "hostname", &tags),
            0x52494c6c0a7500df
        );
    }

    #[test]
    fn context_key_is_order_and_duplicate_insensitive() {
        let canonical = canonical_tags();
        let permuted = vec![
            "key:value2".to_string(),
            "foo".to_string(),
            "bar".to_string(),
            "key:value".to_string(),
        ];
        let duplicated = vec![
            "bar".to_string(),
            "foo".to_string(),
            "bar".to_string(),
            "key:value".to_string(),
            "key:value2".to_string(),
            "foo".to_string(),
        ];

        assert_eq!(context_key("metric.name", "hostname", &permuted), 0x1e923504c1aad3ad);
        assert_eq!(context_key("metric.name", "hostname", &duplicated), 0x1e923504c1aad3ad);
        assert_eq!(
            storage_key("parquet", "metric.name", "hostname", &permuted),
            storage_key("parquet", "metric.name", "hostname", &canonical)
        );
    }

    #[test]
    fn identity_distinguishes_unset_and_empty_host() {
        let unset = SeriesIdentity::new("parquet", "metric.name", None, canonical_tags(), Aggregate::Average);
        let empty = SeriesIdentity::new(
            "parquet",
            "metric.name",
            Some(String::new()),
            canonical_tags(),
            Aggregate::Average,
        );

        // The identities are distinct values...
        assert_ne!(unset, empty);
        // ... but the Go-compatible key and string form collapse both to the empty host, because the Go
        // model has a single host field.
        assert_eq!(unset.key(), empty.key());
        assert_eq!(unset.context_key(), empty.context_key());
        assert_eq!(unset.storage_key(), empty.storage_key());
        assert_eq!(empty.storage_key(), 0x9c84c600e619a733);
    }

    #[test]
    fn identity_distinguishes_namespaces() {
        let tags = vec!["bar".to_string(), "foo".to_string()];
        let parquet = SeriesIdentity::new(
            "parquet",
            "metric.name",
            Some("hostname".to_string()),
            tags.clone(),
            Aggregate::None,
        );
        let dogstatsd = SeriesIdentity::new(
            "dogstatsd",
            "metric.name",
            Some("hostname".to_string()),
            tags,
            Aggregate::None,
        );

        assert_ne!(parquet, dogstatsd);
        assert_ne!(parquet.storage_key(), dogstatsd.storage_key());
        assert_eq!(parquet.storage_key(), 0x5cc3d90c0de7a0bc);
        assert_eq!(dogstatsd.storage_key(), 0xf87f5f372a1eef85);
    }

    #[test]
    fn identity_normalizes_tags() {
        let shuffled = SeriesIdentity::new(
            "parquet",
            "metric.name",
            Some("hostname".to_string()),
            vec![
                "key:value2".to_string(),
                "foo".to_string(),
                "bar".to_string(),
                "key:value".to_string(),
                "bar".to_string(),
            ],
            Aggregate::Average,
        );
        let canonical = SeriesIdentity::new(
            "parquet",
            "metric.name",
            Some("hostname".to_string()),
            canonical_tags(),
            Aggregate::Average,
        );

        assert_eq!(shuffled, canonical);
        assert_eq!(shuffled.key(), canonical.key());
        assert_eq!(shuffled.storage_key(), 0x5360f1417d052cd6);
    }

    #[test]
    fn empty_tags_are_stable() {
        assert_eq!(context_key("metric.name", "hostname", &[]), 0x578e429c8432eb27);
        assert_eq!(
            storage_key("parquet", "metric.name", "hostname", &[]),
            0x0496eed4e402f51d
        );

        let identity = SeriesIdentity::new(
            "parquet",
            "metric.name",
            Some("hostname".to_string()),
            Vec::new(),
            Aggregate::None,
        );
        assert_eq!(identity.storage_key(), 0x0496eed4e402f51d);
    }

    #[test]
    fn descriptor_identity_roundtrip() {
        let descriptor = SeriesDescriptor::new(
            "parquet",
            "metric.name",
            Some("hostname".to_string()),
            vec![
                "key:value2".to_string(),
                "foo".to_string(),
                "bar".to_string(),
                "key:value".to_string(),
            ],
            Aggregate::Average,
        );

        let identity = descriptor.identity();
        assert_eq!(identity.storage_key(), 0x5360f1417d052cd6);
        assert_eq!(
            identity.key(),
            "parquet|metric.name:avg|hostname|bar,foo,key:value,key:value2"
        );
    }

    #[test]
    fn series_refs_are_monotonic_and_never_reused() {
        let mut allocator = SeriesRefAllocator::new();
        let first = allocator.allocate();
        let second = allocator.allocate();

        assert_eq!(first.raw(), 0);
        assert_eq!(second.raw(), 1);
        assert_eq!(allocator.allocated(), 2);
    }

    #[test]
    fn query_handle_compact_id_matches_go_format() {
        let handle = QueryHandle::new(SeriesRef::new(42), Aggregate::Average);
        assert_eq!(handle.compact_id(), "42:avg");
        assert_eq!(
            QueryHandle::new(SeriesRef::new(7), Aggregate::None).compact_id(),
            "7:none"
        );
        assert_eq!(
            QueryHandle::new(SeriesRef::new(3), Aggregate::Sum).compact_id(),
            "3:sum"
        );
        assert_eq!(
            QueryHandle::new(SeriesRef::new(9), Aggregate::Count).compact_id(),
            "9:count"
        );
    }
}
