//! Bounded cache of SQL obfuscation results.
//!
//! Mirrors the trace-agent's cache in `pkg/obfuscate/sql.go`: only successful results are cached, and each entry costs
//! its query and result bytes plus a fixed overhead, against a byte budget (`apm_config.obfuscation.cache.max_size`).
//! Results are held as shared strings, so every span with the same query shares one allocation.

use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};

use quick_cache::{sync::Cache, Equivalent, Weighter};
use saluki_common::hash::FastBuildHasher;
use stringtheory::MetaString;

/// Fixed cost added to every entry, as in the trace-agent.
const ENTRY_OVERHEAD: u64 = 320;

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
    /// Every entry costs at least `ENTRY_OVERHEAD`, which bounds the entry count to `max_size / ENTRY_OVERHEAD`.
    pub fn new(max_size: u64) -> Self {
        let estimated_items = (max_size / ENTRY_OVERHEAD).max(1) as usize;
        Self {
            inner: Arc::new(Cache::with(
                estimated_items,
                max_size.max(1),
                CostWeighter,
                FastBuildHasher::default(),
                Default::default(),
            )),
            max_size,
        }
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
