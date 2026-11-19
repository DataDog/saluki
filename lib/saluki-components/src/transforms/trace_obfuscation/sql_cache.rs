//! Bounded cache of SQL obfuscation results.
//!
//! Mirrors the trace-agent's cache in `pkg/obfuscate/sql.go`: only successful results are cached, and each entry costs
//! its query and result bytes plus a fixed overhead, against a byte budget (`apm_config.obfuscation.cache.max_size`).
//! Results are held as shared strings, so every span with the same query shares one allocation.

use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};

use quick_cache::{sync::Cache, Equivalent, OptionsBuilder, Weighter};
use saluki_common::hash::FastBuildHasher;
use stringtheory::MetaString;

/// Fixed cost added to every entry, as in the trace-agent.
const ENTRY_OVERHEAD: u64 = 320;

/// Share of the entry estimate tracked as recently evicted (ghost) keys.
const GHOST_ALLOCATION: f64 = 0.5;

/// A cached obfuscation result.
#[derive(Clone, Debug)]
pub struct CachedSql {
    pub query: MetaString,
    pub table_names: MetaString,
}

/// The cache key: the query and the DBMS it was obfuscated for.
///
/// The trace-agent's key ignores the DBMS, so a query cached for one DBMS can be returned for another. Keying on it
/// keeps the cached output equal to the uncached output. The rest of the SQL configuration is fixed per cache.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Key {
    query: MetaString,
    dbms: MetaString,
}

impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (&*self.query, &*self.dbms).hash(state);
    }
}

/// A borrowed key, to look up without allocating.
#[derive(Hash)]
struct KeyRef<'a>(&'a str, &'a str);

impl Equivalent<Key> for KeyRef<'_> {
    fn equivalent(&self, key: &Key) -> bool {
        self.0 == &*key.query && self.1 == &*key.dbms
    }
}

#[derive(Clone)]
struct CostWeighter;

impl Weighter<Key, CachedSql> for CostWeighter {
    fn weight(&self, key: &Key, value: &CachedSql) -> u64 {
        cost(key.query.len() + key.dbms.len(), value)
    }
}

fn cost(key_len: usize, value: &CachedSql) -> u64 {
    (key_len + value.query.len() + value.table_names.len()) as u64 + ENTRY_OVERHEAD
}

/// A bounded, thread-safe cache of SQL obfuscation results.
#[derive(Clone)]
pub struct SqlCache {
    inner: Arc<Cache<Key, CachedSql, CostWeighter, FastBuildHasher>>,
    max_size: u64,
}

impl SqlCache {
    /// Creates a cache holding at most `max_size` bytes of entries.
    ///
    /// Every entry costs at least `ENTRY_OVERHEAD`, which bounds the resident entry count to
    /// `max_size / ENTRY_OVERHEAD`. One shard with the whole budget as its hot allocation admits any entry up to
    /// `max_size`: sharding or a smaller hot share would reject large entries the budget can hold.
    pub fn new(max_size: u64) -> Self {
        let options = OptionsBuilder::new()
            .shards(1)
            .hot_allocation(1.0)
            .ghost_allocation(GHOST_ALLOCATION)
            .estimated_items_capacity(Self::max_entries(max_size))
            .weight_capacity(max_size.max(1))
            .build()
            .expect("all options set");
        Self {
            // ponytail: one lock for every caller; shard by query hash if contention shows up.
            inner: Arc::new(Cache::with_options(
                options,
                CostWeighter,
                FastBuildHasher::default(),
                Default::default(),
            )),
            max_size,
        }
    }

    fn max_entries(max_size: u64) -> usize {
        (max_size / ENTRY_OVERHEAD).max(1) as usize
    }

    /// Upper bound of the memory the cache's bookkeeping may hold beyond its entries' cost, in bytes.
    ///
    /// The cache keeps at most `max_entries` resident entries and `GHOST_ALLOCATION` times as many ghost keys, each
    /// in a slab slot with a hash table index. Slots are counted at the size of a full entry, and doubled for the
    /// slab's and table's growth slack.
    pub fn metadata_bound(&self) -> usize {
        let slots = Self::max_entries(self.max_size) as f64 * (1.0 + GHOST_ALLOCATION);
        let slot_size = std::mem::size_of::<(Key, CachedSql)>() + 64;
        (slots.ceil() as usize) * slot_size * 2
    }

    /// Returns the cached result for `query` obfuscated for `dbms`, or computes it with `obfuscate` and caches it on
    /// success. The lock is never held while obfuscating.
    pub fn get_or_obfuscate<E>(
        &self, query: &str, dbms: &str, obfuscate: impl FnOnce() -> Result<CachedSql, E>,
    ) -> Result<CachedSql, E> {
        if let Some(hit) = self.inner.get(&KeyRef(query, dbms)) {
            return Ok(hit);
        }
        let result = obfuscate()?;
        if cost(query.len() + dbms.len(), &result) <= self.max_size {
            let key = Key {
                query: query.into(),
                dbms: dbms.into(),
            };
            self.inner.insert(key, result.clone());
        }
        Ok(result)
    }

    /// Upper bound of the memory the entries may hold, in bytes.
    pub fn max_size(&self) -> u64 {
        self.max_size
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.len()
    }

    #[cfg(test)]
    fn weight(&self) -> u64 {
        self.inner.weight()
    }
}

/// Wraps an obfuscated string as a shared string, so clones share the allocation.
pub fn shared(s: String) -> MetaString {
    if s.is_empty() {
        MetaString::empty()
    } else {
        MetaString::from(Arc::<str>::from(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(q: &str) -> Result<CachedSql, ()> {
        Ok(CachedSql {
            query: shared(q.to_owned()),
            table_names: MetaString::empty(),
        })
    }

    #[test]
    fn entries_stay_within_the_byte_and_count_bounds() {
        let max_size = 10_000;
        let cache = SqlCache::new(max_size);
        for i in 0..2_000 {
            let q = format!("SELECT {i}");
            cache.get_or_obfuscate(&q, "", || ok(&q)).unwrap();
            assert!(cache.weight() <= max_size);
            assert!(cache.len() as u64 <= max_size / ENTRY_OVERHEAD);
        }
        assert!(cache.len() > 0);
    }

    #[test]
    fn large_admissible_entries_are_cached() {
        let max_size = 5_000_000;
        let cache = SqlCache::new(max_size);
        let big = "x".repeat(2_000_000);
        let mut calls = 0;
        for _ in 0..3 {
            cache
                .get_or_obfuscate(&big, "", || {
                    calls += 1;
                    ok(&big)
                })
                .unwrap();
        }
        assert_eq!(calls, 1, "a 4 MB entry under a 5 MB budget is cached");
        assert!(cache.weight() <= max_size);
    }

    // The bookkeeping is measured with quick_cache's `memory_used`, which reports the capacity of its slab of entry
    // slots (resident and ghost) and of its hash table: exactly what `metadata_bound` claims to cover. Entry contents
    // (the strings' heap bytes) are excluded there, as they are counted by the weight against `max_size`.
    #[test]
    fn small_entry_churn_then_large_entries_stays_bounded() {
        let max_size = 100_000;
        let cache = SqlCache::new(max_size);
        let bound = cache.metadata_bound();
        let mut peak = 0;
        let mut check = |cache: &SqlCache| {
            let used = cache.inner.memory_used().total();
            assert!(used <= bound, "bookkeeping {used} exceeds the advertised bound {bound}");
            assert!(cache.weight() <= max_size);
            assert!(cache.len() <= SqlCache::max_entries(max_size));
            peak = peak.max(used);
        };
        for i in 0..50_000 {
            let q = format!("SELECT {i}");
            cache.get_or_obfuscate(&q, "", || ok(&q)).unwrap();
            check(&cache);
        }
        for i in 0..20 {
            let q = format!("{i}{}", "y".repeat(20_000));
            cache.get_or_obfuscate(&q, "", || ok(&q)).unwrap();
            check(&cache);
        }
        assert!(peak > 0, "the measure must see the bookkeeping");
    }

    #[test]
    fn oversized_entries_and_errors_are_not_cached() {
        let cache = SqlCache::new(1_000);
        let big = "x".repeat(1_000);
        cache.get_or_obfuscate(&big, "", || ok(&big)).unwrap();
        assert_eq!(cache.len(), 0);

        let mut calls = 0;
        for _ in 0..3 {
            let r: Result<CachedSql, ()> = cache.get_or_obfuscate("bad", "", || {
                calls += 1;
                Err(())
            });
            assert!(r.is_err());
        }
        assert_eq!(calls, 3);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn hits_share_one_allocation_and_dbms_is_part_of_the_key() {
        let cache = SqlCache::new(1_000_000);
        let first = cache.get_or_obfuscate("q", "mysql", || ok("a")).unwrap();
        let hit = cache.get_or_obfuscate("q", "mysql", || ok("unused")).unwrap();
        assert_eq!(hit.query.as_ptr(), first.query.as_ptr());
        let other = cache.get_or_obfuscate("q", "postgresql", || ok("b")).unwrap();
        assert_eq!(&*other.query, "b");
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn concurrent_use_keeps_bounds_and_results() {
        let max_size = 20_000;
        let cache = SqlCache::new(max_size);
        std::thread::scope(|s| {
            for t in 0..8 {
                let cache = cache.clone();
                s.spawn(move || {
                    for i in 0..1_000 {
                        let q = format!("SELECT {}", (i * 7 + t) % 300);
                        let r = cache.get_or_obfuscate(&q, "", || ok(&q.to_uppercase())).unwrap();
                        assert_eq!(&*r.query, q.to_uppercase());
                    }
                });
            }
        });
        assert!(cache.weight() <= max_size);
    }
}
