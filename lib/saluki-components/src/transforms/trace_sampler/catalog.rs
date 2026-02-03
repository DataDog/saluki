//! Service key catalog for reverse-mapping service signatures to hashes.
//!
//! This module provides an LRU cache that maps `ServiceSignature` (service name + env)
//! to their computed `Signature` hashes. This allows efficient lookup of sampling rates
//! by service when building the rates-by-service response.
//!
//! Note: This module is infrastructure for the priority sampler. The types are not yet
//! integrated into the main sampling pipeline.
use lru_slab::LruSlab;
use saluki_common::collections::FastHashMap;
use tracing::warn;

use super::signature::{ServiceSignature, Signature};

/// Maximum number of entries in the catalog before eviction.
const MAX_CATALOG_ENTRIES: usize = 5000;
/// Initial size of our LRU cache, will grow as needed.
const INITIAL_SIZE: usize = 1024;

/// Entry in the LRU catalog linking a service signature to its hash.
#[derive(Clone)]
struct CatalogEntry {
    key: ServiceSignature,
    sig: Signature,
}

/// Inner state protected by mutex.
struct Inner {
    /// Map from ServiceSignature to slot token in the LRU slab.
    items: FastHashMap<ServiceSignature, u32>,
    /// LRU list of entries (front = most recently used).
    entries: LruSlab<CatalogEntry>,
    /// Maximum number of entries before eviction.
    max_entries: usize,
}

/// LRU cache mapping service signatures to their computed hashes.
///
/// The catalog maintains a bounded cache of service signatures, evicting
/// the least recently used entries when the capacity is exceeded.
pub(super) struct ServiceKeyCatalog {
    inner: Inner,
}

impl ServiceKeyCatalog {
    /// Creates a new ServiceKeyCatalog with the default maximum entries.
    pub fn new() -> Self {
        Self::with_max_entries(MAX_CATALOG_ENTRIES)
    }

    /// Creates a new ServiceKeyCatalog with a custom maximum entries limit.
    ///
    /// If `max_entries` is 0, uses the default of 5000.
    pub fn with_max_entries(max_entries: usize) -> Self {
        let max = if max_entries == 0 {
            MAX_CATALOG_ENTRIES
        } else {
            max_entries
        };
        let prealloc = max.min(INITIAL_SIZE) as u32;
        Self {
            inner: Inner {
                items: FastHashMap::default(),
                entries: LruSlab::with_capacity(prealloc),
                max_entries: max,
            },
        }
    }

    /// Registers a service signature and returns its hash.
    ///
    /// If the signature already exists, moves it to the front (most recently used)
    /// and returns the cached hash. Otherwise, computes the hash, stores the entry,
    /// and evicts the least recently used entry if at capacity.
    pub fn register(&mut self, svc_sig: ServiceSignature) -> Signature {
        // Check if signature already exists
        if let Some(&slot) = self.inner.items.get(&svc_sig) {
            // Move to front (most recently used) and return cached hash.
            let sig = self.inner.entries.get_mut(slot).sig;
            return sig;
        }

        // New signature - compute hash
        let hash = svc_sig.hash();
        let entry = CatalogEntry {
            key: svc_sig.clone(),
            sig: hash,
        };

        // Add to front
        let slot = self.inner.entries.insert(entry);
        self.inner.items.insert(svc_sig.clone(), slot);

        // Evict if over capacity
        if self.inner.entries.len() as usize > self.inner.max_entries {
            if let Some(slot) = self.inner.entries.lru() {
                let evicted = self.inner.entries.remove(slot);
                self.inner.items.remove(&evicted.key);
                warn!(
                    max_entries = self.inner.max_entries,
                    evicted_service = %evicted.key,
                    "Service-rates catalog exceeded capacity, dropping oldest entry"
                );
            }
        }

        hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog_contains(catalog: &ServiceKeyCatalog, expected: &FastHashMap<ServiceSignature, Signature>) {
        assert_eq!(catalog.inner.items.len(), expected.len(), "too many items in map");
        assert_eq!(
            catalog.inner.entries.len() as usize,
            expected.len(),
            "too many elements in list",
        );

        for (key, slot) in &catalog.inner.items {
            let entry = catalog.inner.entries.peek(*slot);
            assert_eq!(&entry.key, key, "list element in map incorrect for key");
            let expected_sig = expected
                .get(key)
                .unwrap_or_else(|| panic!("foreign item in map: {}", key));
            assert_eq!(&entry.sig, expected_sig, "invalid value in list at key {}", key);
        }

        for (key, expected_sig) in expected {
            let slot = catalog
                .inner
                .items
                .get(key)
                .unwrap_or_else(|| panic!("missing item in map: {}", key));
            let entry = catalog.inner.entries.peek(*slot);
            assert_eq!(&entry.sig, expected_sig, "invalid value in map at key {}", key);
        }
    }

    #[test]
    fn test_new_service_lookup() {
        let catalog = ServiceKeyCatalog::with_max_entries(0);
        assert!(catalog.inner.items.is_empty());
        assert_eq!(catalog.inner.entries.len(), 0);
        assert_eq!(catalog.inner.max_entries, MAX_CATALOG_ENTRIES);
    }

    #[test]
    fn test_service_key_catalog_register() {
        let mut catalog = ServiceKeyCatalog::with_max_entries(0);

        let sig1 = ServiceSignature::new("service1", "testEnv");
        let hash1 = catalog.register(sig1.clone());
        let mut expected = FastHashMap::default();
        expected.insert(sig1.clone(), hash1);
        catalog_contains(&catalog, &expected);

        let sig2 = ServiceSignature::new("service2", "testEnv");
        let hash2 = catalog.register(sig2.clone());
        expected.insert(sig2, hash2);
        catalog_contains(&catalog, &expected);
    }

    #[test]
    fn test_service_key_catalog_lru_size() {
        let mut catalog = ServiceKeyCatalog::with_max_entries(3);

        let _ = catalog.register(ServiceSignature::new("service1", "env1"));
        let sig2 = catalog.register(ServiceSignature::new("service2", "env2"));
        let sig3 = catalog.register(ServiceSignature::new("service3", "env3"));
        let sig4 = catalog.register(ServiceSignature::new("service4", "env4"));

        let mut expected = FastHashMap::default();
        expected.insert(ServiceSignature::new("service2", "env2"), sig2);
        expected.insert(ServiceSignature::new("service3", "env3"), sig3);
        expected.insert(ServiceSignature::new("service4", "env4"), sig4);
        catalog_contains(&catalog, &expected);

        let sig5 = catalog.register(ServiceSignature::new("service5", "env5"));
        expected.clear();
        expected.insert(ServiceSignature::new("service3", "env3"), sig3);
        expected.insert(ServiceSignature::new("service4", "env4"), sig4);
        expected.insert(ServiceSignature::new("service5", "env5"), sig5);
        catalog_contains(&catalog, &expected);
    }

    #[test]
    fn test_service_key_catalog_lru_move() {
        let mut catalog = ServiceKeyCatalog::with_max_entries(3);

        let sig1 = catalog.register(ServiceSignature::new("service1", "env1"));
        let _ = catalog.register(ServiceSignature::new("service2", "env2"));
        let sig3 = catalog.register(ServiceSignature::new("service3", "env3"));
        catalog.register(ServiceSignature::new("service1", "env1"));
        let sig4 = catalog.register(ServiceSignature::new("service4", "env4"));

        let mut expected = FastHashMap::default();
        expected.insert(ServiceSignature::new("service1", "env1"), sig1);
        expected.insert(ServiceSignature::new("service3", "env3"), sig3);
        expected.insert(ServiceSignature::new("service4", "env4"), sig4);
        catalog_contains(&catalog, &expected);
    }
}
