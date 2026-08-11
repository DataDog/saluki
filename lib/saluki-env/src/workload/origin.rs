//! Origin detection and resolution.

use std::{num::NonZeroUsize, sync::Arc, time::Duration};

use saluki_common::{
    cache::{Cache, CacheBuilder},
    hash::hash_single_fast,
};
use saluki_context::origin::{OriginTagCardinality, RawOrigin};
use tracing::trace;

use super::stores::ExternalDataStoreResolver;
use crate::workload::EntityId;

// SAFETY: This number is obviously non-zero.
const DEFAULT_ORIGIN_CACHE_ITEM_LIMIT: NonZeroUsize = NonZeroUsize::new(500_000).unwrap();
const DEFAULT_ORIGIN_CACHE_ITEM_TIME_TO_IDLE: Duration = Duration::from_secs(30);

/// A resolved External Data entry.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ResolvedExternalData {
    pod_entity_id: EntityId,
    container_entity_id: EntityId,
}

impl ResolvedExternalData {
    /// Creates a new `ResolvedExternalData` from the given pod and container entity IDs.
    pub fn new(pod_entity_id: EntityId, container_entity_id: EntityId) -> Self {
        Self {
            pod_entity_id,
            container_entity_id,
        }
    }

    /// Returns a reference to the pod entity ID.
    pub fn pod_entity_id(&self) -> &EntityId {
        &self.pod_entity_id
    }

    /// Returns a reference to the container entity ID.
    pub fn container_entity_id(&self) -> &EntityId {
        &self.container_entity_id
    }
}

#[derive(Debug, Hash, Eq, PartialEq)]
struct ResolvedOriginInner {
    cardinality: Option<OriginTagCardinality>,
    process_id: Option<EntityId>,
    local_data: Option<EntityId>,
    pod_uid: Option<EntityId>,
    resolved_external_data: Option<ResolvedExternalData>,
}

/// An resolved representation of `RawOrigin<'a>`
///
/// This representation is used to store the pre-calculated entity IDs derived from a borrowed `RawOrigin<'a>` in order
/// to speed the lookup of origin tags attached to each individual entity ID that comprises an origin.
///
/// This type can be cheaply cloned and shared across threads.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct ResolvedOrigin {
    inner: Arc<ResolvedOriginInner>,
}

impl ResolvedOrigin {
    /// Creates a new `ResolvedOrigin` from the given parts.
    pub fn from_parts(
        cardinality: Option<OriginTagCardinality>, process_id: Option<EntityId>, local_data: Option<EntityId>,
        pod_uid: Option<EntityId>, resolved_external_data: Option<ResolvedExternalData>,
    ) -> Self {
        Self {
            inner: Arc::new(ResolvedOriginInner {
                cardinality,
                process_id,
                local_data,
                pod_uid,
                resolved_external_data,
            }),
        }
    }

    /// Returns the cardinality of the origin.
    pub fn cardinality(&self) -> Option<OriginTagCardinality> {
        self.inner.cardinality
    }

    /// Returns the process ID of the origin.
    pub fn process_id(&self) -> Option<&EntityId> {
        self.inner.process_id.as_ref()
    }

    /// Returns the Local Data-based entity ID of the origin.
    pub fn local_data(&self) -> Option<&EntityId> {
        self.inner.local_data.as_ref()
    }

    /// Returns the pod UID of the origin.
    pub fn pod_uid(&self) -> Option<&EntityId> {
        self.inner.pod_uid.as_ref()
    }

    /// Returns the resolved External Data of the origin.
    pub fn resolved_external_data(&self) -> Option<&ResolvedExternalData> {
        self.inner.resolved_external_data.as_ref()
    }
}

/// Resolves and tracks origins.
#[derive(Clone)]
pub struct OriginResolver {
    ed_resolver: ExternalDataStoreResolver,
    origin_cache: Cache<u64, ResolvedOrigin>,
}

impl OriginResolver {
    /// Creates a new `OriginResolver`.
    pub fn new(ed_resolver: ExternalDataStoreResolver) -> Self {
        Self {
            ed_resolver,
            origin_cache: CacheBuilder::from_identifier("origin_cache")
                .expect("identifier cannot be invalid")
                .with_capacity(DEFAULT_ORIGIN_CACHE_ITEM_LIMIT)
                .with_time_to_idle(Some(DEFAULT_ORIGIN_CACHE_ITEM_TIME_TO_IDLE))
                .build(),
        }
    }

    fn build_resolved_origin(&self, origin: RawOrigin<'_>) -> ResolvedOrigin {
        ResolvedOrigin::from_parts(
            origin.cardinality(),
            origin.process_id().map(EntityId::ContainerPid),
            origin.local_data().and_then(EntityId::from_local_data),
            origin.pod_uid().and_then(EntityId::from_pod_uid),
            origin
                .external_data()
                .and_then(|raw_ed| self.ed_resolver.resolve(raw_ed)),
        )
    }

    /// Returns the resolved origin for the given raw origin.
    ///
    /// If the raw origin is "empty" -- no origin information is available -- then `None` is returned.
    ///
    /// The resolved origin may be cached for speeding up future lookups.
    pub fn get_resolved_origin(&self, origin: RawOrigin<'_>) -> Option<ResolvedOrigin> {
        // If there's no origin information at all, then there's nothing to key off of.
        if origin.is_empty() {
            return None;
        }

        // Create the origin key, and populate our cache with the resolved origin if we don't already have it.
        let origin_key = hash_single_fast(&origin);
        match self.origin_cache.get(&origin_key) {
            Some(resolved_origin) => {
                trace!(?origin_key, "Found origin in cache.");
                Some(resolved_origin)
            }
            None => {
                trace!(?origin_key, "Origin not found in cache. Resolving.");
                let resolved_origin = self.build_resolved_origin(origin);
                self.origin_cache.insert(origin_key, resolved_origin.clone());

                Some(resolved_origin)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // These tests were restored from a module that was commented out in mid-2025 (commit 6635adbf9f) and never
    // brought back. In the interim, `OriginResolver` was reworked: it no longer performs process-ID -> container-ID
    // alias redirection (that behavior moved to `TagStoreQuerier::get_entity_tags`, whose alias handling is now
    // covered in `stores/tag.rs`), and the old `resolve_origin`/`get_resolved_origin_by_key`/`ResolvedOrigin::container_id`
    // API was replaced by `get_resolved_origin` returning a cached `ResolvedOrigin`. These tests therefore cover the
    // current documented contract of `OriginResolver`: field-to-entity resolution and its caching behavior.
    use std::num::NonZeroUsize;

    use super::*;
    use crate::workload::stores::ExternalDataStore;

    fn origin_resolver() -> OriginResolver {
        let external_data_store = ExternalDataStore::with_entity_limit(NonZeroUsize::new(usize::MAX).unwrap());
        OriginResolver::new(external_data_store.resolver())
    }

    fn raw_origin(
        process_id: Option<u32>, local_data: Option<&'static str>, pod_uid: Option<&'static str>,
    ) -> RawOrigin<'static> {
        let mut origin = RawOrigin::default();
        if let Some(process_id) = process_id {
            origin.set_process_id(process_id);
        }
        origin.set_local_data(local_data);
        origin.set_pod_uid(pod_uid);
        origin
    }

    #[tokio::test]
    async fn get_resolved_origin_returns_none_for_empty_origin() {
        // An origin with no information at all can't be keyed off of, so resolution yields `None`.
        let resolver = origin_resolver();

        assert!(resolver.get_resolved_origin(RawOrigin::default()).is_none());
    }

    #[tokio::test]
    async fn get_resolved_origin_maps_process_id_and_local_data_to_entities() {
        // Resolution derives entity IDs directly from the raw origin's fields: the process ID becomes a `ContainerPid`
        // entity, and the Local Data (here, a bare container ID) becomes a `Container` entity.
        let resolver = origin_resolver();

        let resolved = resolver
            .get_resolved_origin(raw_origin(Some(1234), Some("container-a"), None))
            .expect("non-empty origin should resolve");

        assert_eq!(resolved.process_id(), Some(&EntityId::ContainerPid(1234)));
        assert_eq!(resolved.local_data(), Some(&EntityId::Container("container-a".into())));
        assert_eq!(resolved.pod_uid(), None);
    }

    #[tokio::test]
    async fn get_resolved_origin_caches_identical_raw_origins() {
        // Two lookups for equal raw origins must return the very same cached `ResolvedOrigin` instance rather than a
        // freshly rebuilt one -- the documented caching contract.
        let resolver = origin_resolver();

        let origin = raw_origin(Some(1234), Some("container-a"), None);
        let first = resolver.get_resolved_origin(origin.clone()).expect("should resolve");
        let second = resolver.get_resolved_origin(origin).expect("should resolve");

        assert!(
            Arc::ptr_eq(&first.inner, &second.inner),
            "identical raw origins should resolve to the same cached instance"
        );
    }

    #[tokio::test]
    async fn get_resolved_origin_distinguishes_different_local_data() {
        // Raw origins that differ only in their Local Data resolve to distinct, non-equal resolved origins.
        let resolver = origin_resolver();

        let resolved_a = resolver
            .get_resolved_origin(raw_origin(Some(1234), Some("container-a"), None))
            .expect("should resolve");
        let resolved_b = resolver
            .get_resolved_origin(raw_origin(Some(1234), Some("container-b"), None))
            .expect("should resolve");

        assert_ne!(resolved_a, resolved_b);
        assert_eq!(
            resolved_a.local_data(),
            Some(&EntityId::Container("container-a".into()))
        );
        assert_eq!(
            resolved_b.local_data(),
            Some(&EntityId::Container("container-b".into()))
        );
    }
}
