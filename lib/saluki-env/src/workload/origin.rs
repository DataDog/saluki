//! Origin detection and resolution.

use std::sync::Arc;

use papaya::HashMap;
use saluki_context::origin::{OriginKey, OriginTagCardinality, RawOrigin};
use tracing::trace;

use super::{
    on_demand_pid::OnDemandPIDResolver,
    stores::{ExternalDataStoreResolver, TagStoreQuerier},
};
use crate::workload::EntityId;

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
    container_id: Option<EntityId>,
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
    /// Returns the cardinality of the origin.
    pub fn cardinality(&self) -> Option<OriginTagCardinality> {
        self.inner.cardinality
    }

    /// Returns the container ID of the origin.
    pub fn container_id(&self) -> Option<&EntityId> {
        self.inner.container_id.as_ref()
    }

    /// Returns the pod UID of the origin.
    pub fn pod_uid(&self) -> Option<&EntityId> {
        self.inner.pod_uid.as_ref()
    }

    /// Returns the resolved external data of the origin.
    pub fn resolved_external_data(&self) -> Option<&ResolvedExternalData> {
        self.inner.resolved_external_data.as_ref()
    }
}

/// Resolves and tracks origins.
#[derive(Clone)]
pub struct OriginResolver {
    ts_querier: TagStoreQuerier,
    ed_resolver: ExternalDataStoreResolver,
    on_demand_pid_resolver: OnDemandPIDResolver,
    key_mappings: Arc<HashMap<OriginKey, ResolvedOrigin>>,
}

impl OriginResolver {
    /// Creates a new `OriginResolver`.
    pub fn new(
        ts_querier: TagStoreQuerier, ed_resolver: ExternalDataStoreResolver,
        on_demand_pid_resolver: OnDemandPIDResolver,
    ) -> Self {
        Self {
            ts_querier,
            ed_resolver,
            on_demand_pid_resolver,
            key_mappings: Arc::new(HashMap::default()),
        }
    }

    fn build_resolved_raw_origin<'b>(
        &self, mut origin: RawOrigin<'b>, maybe_resolved_container_id: Option<&'b str>,
    ) -> RawOrigin<'b> {
        // This function perhaps looks stupid, but we use it to coerce the lifetimes in our favor.
        //
        // Essentially, we have `RawOrigin<'a>` as an input to `resolve_origin`, and we want to override the container
        // ID with a new value that has a lifetime `'b`, where `'a` outlives `'b`. We can't just directly update the
        // container ID field because the lifetimes don't line up. However, if we pass in the `RawOrigin<'a>` to a
        // function that associates the lifetime with that of the container ID's lifetime, we can successfully coerce
        // the lifetimes.

        // If the origin lacks its own container ID, but we have a resolved container ID, then we use the resolved one.
        if origin.container_id().is_none() && maybe_resolved_container_id.is_some() {
            origin.set_container_id(maybe_resolved_container_id);
        }

        // Clear the process ID now that we've handled any container ID resolution.
        origin.clear_process_id();

        origin
    }

    fn build_resolved_origin(&self, origin: RawOrigin<'_>) -> ResolvedOrigin {
        ResolvedOrigin {
            inner: Arc::new(ResolvedOriginInner {
                cardinality: origin.cardinality(),
                container_id: origin.container_id().and_then(EntityId::from_raw_container_id),
                pod_uid: origin.pod_uid().and_then(EntityId::from_pod_uid),
                resolved_external_data: origin
                    .external_data()
                    .and_then(|raw_ed| self.ed_resolver.resolve(raw_ed)),
            }),
        }
    }

    pub(crate) fn resolve_origin(&self, origin: RawOrigin<'_>) -> Option<OriginKey> {
        // Resolving a raw origin to an origin key involves a few steps:
        //
        // - We handle "resolving" the process ID if no container ID is provided. This tries to map the process ID to a
        //   container ID, if possible, and creates a "resolved" raw origin that we then key off of.
        // - Hash the "resolved" raw origin to generate the `OriginKey` we give to the caller.
        // - Check our resolved origin cache to see if we already have a "resolved origin" -- an owned copy of
        //   `RawOrigin`, essentially -- and create it if we don't.

        // If there's no origin information at all, then there's nothing to key off of.
        if origin.is_empty() {
            return None;
        }

        // Resolve the raw origin to map the process ID to a container ID, if possible.
        //
        // We only do this if the origin doesn't already have a container ID. If it does, then we don't need to do
        // bother because we treat the client-provided container ID as authoritative. We have to jump through a small
        // hoop to do this efficiently: see the doc comments in `build_resolved_raw_origin` for more details.
        let resolved_container_id = if origin.container_id().is_none() {
            origin.process_id().and_then(|process_id| {
                self.ts_querier
                    .get_entity_alias(&EntityId::ContainerPid(process_id))
                    .or_else(|| self.on_demand_pid_resolver.resolve(process_id))
                    .and_then(|id| id.try_into_container())
            })
        } else {
            None
        };
        let resolved_raw_origin = self.build_resolved_raw_origin(origin, resolved_container_id.as_deref());

        let origin_key = OriginKey::from_opaque(&resolved_raw_origin);

        // TODO: This is a slow leak because we never remove entries and have no signal to know when to do so.
        //
        // We should likely just use `quick-cache` here, but it's not a huge deal for now.
        let _ = self
            .key_mappings
            .pin()
            .get_or_insert_with(origin_key, || self.build_resolved_origin(resolved_raw_origin));

        Some(origin_key)
    }

    pub(crate) fn get_resolved_origin_by_key(&self, origin_key: &OriginKey) -> Option<ResolvedOrigin> {
        match self.key_mappings.pin().get(origin_key) {
            Some(origin) => Some(origin.clone()),
            None => {
                trace!(?origin_key, "No origin found for key.");
                None
            }
        }
    }
}

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

        OriginResolver::new(
            tag_store_querier.clone(),
            external_data_store.resolver(),
            OnDemandPIDResolver::noop(),
        )
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
