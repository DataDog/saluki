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

    pub(crate) fn get_resolved_origin(&self, origin: RawOrigin<'_>) -> Option<ResolvedOrigin> {
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

/*
#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use stringtheory::MetaString;

    use super::*;
    use crate::workload::{
        aggregator::MetadataStore as _,
        stores::{ExternalDataStore, TagStore},
        MetadataOperation,
    };

    const PROCESS_ID_A_RAW: u32 = 1;
    const PROCESS_ID_B_RAW: u32 = 2;
    const CONTAINER_ID_A_RAW: &str = "container-a";
    const CONTAINER_ID_B_RAW: &str = "container-b";
    const CONTAINER_ID_C_RAW: &str = "container-c";
    const PROCESS_ID_A: EntityId = EntityId::ContainerPid(PROCESS_ID_A_RAW);
    const PROCESS_ID_B: EntityId = EntityId::ContainerPid(PROCESS_ID_B_RAW);
    const CONTAINER_ID_A: EntityId = EntityId::Container(MetaString::from_static(CONTAINER_ID_A_RAW));
    const CONTAINER_ID_B: EntityId = EntityId::Container(MetaString::from_static(CONTAINER_ID_B_RAW));
    const CONTAINER_ID_C: EntityId = EntityId::Container(MetaString::from_static(CONTAINER_ID_C_RAW));

    fn create_raw_origin(process_id: u32, container_id: Option<&'static str>) -> RawOrigin<'static> {
        let mut raw_origin = RawOrigin::default();
        raw_origin.set_process_id(process_id);
        raw_origin.set_container_id(container_id);
        raw_origin
    }

    #[track_caller]
    fn create_origin_resolver<const N: usize>(aliases: [(EntityId, EntityId); N]) -> OriginResolver {
        // Create our tag store and seed it with any provided aliases.
        let mut tag_store = TagStore::with_entity_limit(NonZeroUsize::new(usize::MAX).unwrap());
        let tag_store_querier = tag_store.querier();

        for (entity_id, alias) in aliases {
            tag_store.process_operation(MetadataOperation::add_alias(entity_id.clone(), alias.clone()));
            assert_eq!(tag_store_querier.get_entity_alias(&entity_id), Some(alias));
        }

        let external_data_store = ExternalDataStore::with_entity_limit(NonZeroUsize::new(usize::MAX).unwrap());

        OriginResolver::new(external_data_store.resolver())
    }

    #[test]
    fn resolve_origin_no_aliases_different_process_id_no_container_id() {
        // Create our origin resolver with no aliases pre-loaded, so we're just resolving the raw origins based on only
        // the data they contain.
        let origin_resolver = create_origin_resolver([]);

        // Assert that the two resulting resolved origins are equal.
        //
        // While the raw origins should be different (different process IDs, no container ID), the resolved origins
        // should end up with no container ID, as the raw origins don't have one and no aliases were present, which
        // should resulting in both origins being the same due to effectively being empty.
        let raw_origin_a = create_raw_origin(PROCESS_ID_A_RAW, None);
        let raw_origin_b = create_raw_origin(PROCESS_ID_B_RAW, None);
        assert_ne!(raw_origin_a, raw_origin_b);

        let origin_key_a = origin_resolver.resolve_origin(raw_origin_a).unwrap();
        let origin_key_b = origin_resolver.resolve_origin(raw_origin_b).unwrap();
        assert_eq!(origin_key_a, origin_key_b);

        let resolved_origin_a = origin_resolver.get_resolved_origin_by_key(&origin_key_a).unwrap();
        let resolved_origin_b = origin_resolver.get_resolved_origin_by_key(&origin_key_b).unwrap();
        assert_eq!(resolved_origin_a, resolved_origin_b);
        assert_eq!(resolved_origin_a.container_id(), None);
        assert_eq!(resolved_origin_b.container_id(), None);
    }

    #[test]
    fn resolve_origin_no_aliases_different_process_id_different_container_id() {
        // Create our origin resolver with no aliases pre-loaded, so we're just resolving the raw origins based on only
        // the data they contain.
        let origin_resolver = create_origin_resolver([]);

        // Assert that the two resulting resolved origins are not equal.
        //
        // The raw origins should be different (different process IDs, different container IDs), and the resolved
        // origins should be different, given that even after resolving the process ID, the resulting origins should
        // have different container IDs.
        let raw_origin_a = create_raw_origin(PROCESS_ID_A_RAW, Some(CONTAINER_ID_A_RAW));
        let raw_origin_b = create_raw_origin(PROCESS_ID_B_RAW, Some(CONTAINER_ID_B_RAW));
        assert_ne!(raw_origin_a, raw_origin_b);

        let origin_key_a = origin_resolver.resolve_origin(raw_origin_a).unwrap();
        let origin_key_b = origin_resolver.resolve_origin(raw_origin_b).unwrap();
        assert_ne!(origin_key_a, origin_key_b);

        let resolved_origin_a = origin_resolver.get_resolved_origin_by_key(&origin_key_a).unwrap();
        let resolved_origin_b = origin_resolver.get_resolved_origin_by_key(&origin_key_b).unwrap();
        assert_ne!(resolved_origin_a, resolved_origin_b);
        assert_eq!(resolved_origin_a.container_id(), Some(&CONTAINER_ID_A));
        assert_eq!(resolved_origin_b.container_id(), Some(&CONTAINER_ID_B));
    }

    #[test]
    fn resolve_origin_same_alias_different_process_ids_no_container_id() {
        // Create our original resolver with two aliases pre-loaded: process ID A to container ID B, and process ID B to
        // container ID B.
        let origin_resolver = create_origin_resolver([(PROCESS_ID_A, CONTAINER_ID_B), (PROCESS_ID_B, CONTAINER_ID_B)]);

        // Assert that the two resulting resolved origins are equal.
        //
        // While the raw origins should be different (different process IDs, no container ID), the resolved origins
        // should use the aliased container ID for each process ID, which is the same for both origins.
        let raw_origin_a = create_raw_origin(PROCESS_ID_A_RAW, None);
        let raw_origin_b = create_raw_origin(PROCESS_ID_B_RAW, None);
        assert_ne!(raw_origin_a, raw_origin_b);

        let origin_key_a = origin_resolver.resolve_origin(raw_origin_a).unwrap();
        let origin_key_b = origin_resolver.resolve_origin(raw_origin_b).unwrap();
        assert_eq!(origin_key_a, origin_key_b);

        let resolved_origin_a = origin_resolver.get_resolved_origin_by_key(&origin_key_a).unwrap();
        let resolved_origin_b = origin_resolver.get_resolved_origin_by_key(&origin_key_b).unwrap();
        assert_eq!(resolved_origin_a.container_id(), Some(&CONTAINER_ID_B));
        assert_eq!(resolved_origin_b.container_id(), Some(&CONTAINER_ID_B));
    }

    #[test]
    fn resolve_origin_same_alias_different_process_ids_same_container_id() {
        // Create our origin resolver with two aliases pre-loaded: process ID A to container ID B, and process ID B to
        // container B.
        let origin_resolver = create_origin_resolver([(PROCESS_ID_A, CONTAINER_ID_B), (PROCESS_ID_B, CONTAINER_ID_B)]);

        // Assert that the two resulting resolved origins are equal.
        //
        // While the raw origins should be different (different process IDs, same container ID), the resolved origins
        // should ignore the process IDs and their aliases and use the provided container ID, which is the same for both
        // origins.
        let raw_origin_a = create_raw_origin(PROCESS_ID_A_RAW, Some(CONTAINER_ID_A_RAW));
        let raw_origin_b = create_raw_origin(PROCESS_ID_B_RAW, Some(CONTAINER_ID_A_RAW));
        assert_ne!(raw_origin_a, raw_origin_b);

        let origin_key_a = origin_resolver.resolve_origin(raw_origin_a).unwrap();
        let origin_key_b = origin_resolver.resolve_origin(raw_origin_b).unwrap();
        assert_eq!(origin_key_a, origin_key_b);

        let resolved_origin_a = origin_resolver.get_resolved_origin_by_key(&origin_key_a).unwrap();
        let resolved_origin_b = origin_resolver.get_resolved_origin_by_key(&origin_key_b).unwrap();
        assert_eq!(resolved_origin_a.container_id(), Some(&CONTAINER_ID_A));
        assert_eq!(resolved_origin_b.container_id(), Some(&CONTAINER_ID_A));
    }

    #[test]
    fn resolve_origin_same_alias_different_process_ids_different_container_id() {
        // Create our origin resolver with two aliases pre-loaded: process ID A to container ID C, and process ID B to
        // container C.
        let origin_resolver = create_origin_resolver([(PROCESS_ID_A, CONTAINER_ID_C), (PROCESS_ID_B, CONTAINER_ID_C)]);

        // Assert that the two resulting resolved origins are not equal.
        //
        // The raw origins should be different (different process IDs, different container IDs), and the resolved
        // origins should be different, given that the process ID resolution should not affect the explicitly provided
        // container IDs, which are different.
        let raw_origin_a = create_raw_origin(PROCESS_ID_A_RAW, Some(CONTAINER_ID_A_RAW));
        let raw_origin_b = create_raw_origin(PROCESS_ID_B_RAW, Some(CONTAINER_ID_B_RAW));
        assert_ne!(raw_origin_a, raw_origin_b);

        let origin_key_a = origin_resolver.resolve_origin(raw_origin_a).unwrap();
        let origin_key_b = origin_resolver.resolve_origin(raw_origin_b).unwrap();
        assert_ne!(origin_key_a, origin_key_b);

        let resolved_origin_a = origin_resolver.get_resolved_origin_by_key(&origin_key_a).unwrap();
        let resolved_origin_b = origin_resolver.get_resolved_origin_by_key(&origin_key_b).unwrap();
        assert_eq!(resolved_origin_a.container_id(), Some(&CONTAINER_ID_A));
        assert_eq!(resolved_origin_b.container_id(), Some(&CONTAINER_ID_B));
    }
}
*/
