use std::{num::NonZeroUsize, sync::Arc};

use arc_swap::ArcSwap;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use tracing::{debug, trace};

use crate::{
    prelude::*,
    workload::{
        aggregator::MetadataStore,
        external_data::{ExternalData, ExternalDataRef, ResolvedExternalData},
        EntityId, MetadataAction, MetadataOperation,
    },
};

/// A store for External Data entity mappings.
pub struct ExternalDataStore {
    snapshot: Arc<ArcSwap<ExternalDataSnapshot>>,

    entity_limit: NonZeroUsize,
    active_entities: FastHashSet<EntityId>,

    forward_mappings: FastIndexMap<ExternalData, ResolvedExternalData>,
    reverse_mappings: FastIndexMap<EntityId, ExternalData>,
}

impl ExternalDataStore {
    /// Creates a new `ExternalDataStore` with the given entity limit.
    ///
    /// The entity limit is the maximum number of unique entities that can be stored. Once the limit is reached, new
    /// entities will not be added to the store.
    pub fn with_entity_limit(entity_limit: NonZeroUsize) -> Self {
        Self {
            snapshot: Arc::new(ArcSwap::new(Arc::new(ExternalDataSnapshot::default()))),
            entity_limit,
            active_entities: FastHashSet::default(),
            forward_mappings: FastIndexMap::default(),
            reverse_mappings: FastIndexMap::default(),
        }
    }

    /// Returns the maximum number of unique entities that can be tracked by the store at any given time.
    pub fn entity_limit(&self) -> usize {
        self.entity_limit.get()
    }

    fn track_entity(&mut self, entity_id: &EntityId) -> bool {
        if self.active_entities.contains(entity_id) {
            return true;
        }

        if self.active_entities.len() >= self.entity_limit() {
            return false;
        }

        let _ = self.active_entities.insert(entity_id.clone());
        true
    }

    fn add_mapping(&mut self, external_data: ExternalData, entity_id: EntityId) {
        if !self.track_entity(&entity_id) {
            trace!(
                entity_limit = self.entity_limit(),
                %entity_id,
                "Entity limit reached, not adding mapping."
            );
            return;
        }

        // We create a "resolved" form of the External Data, which includes entity IDs for both the pod and the
        // container that this External Data is attached to.
        let resolved = ResolvedExternalData::new(EntityId::PodUid(external_data.pod_uid().clone()), entity_id.clone());

        let _ = self.forward_mappings.insert(external_data.clone(), resolved);
        let _ = self.reverse_mappings.insert(entity_id, external_data);
    }

    fn remove_mapping(&mut self, entity_id: EntityId) {
        if !self.active_entities.remove(&entity_id) {
            return;
        }

        if let Some(external_data) = self.reverse_mappings.swap_remove(&entity_id) {
            let _ = self.forward_mappings.swap_remove(&external_data);
        }
    }

    /// Returns a `ExternalDataStoreResolver` that can be used to concurrently resolve entity IDs from External Data.
    pub fn resolver(&self) -> ExternalDataStoreResolver {
        ExternalDataStoreResolver {
            snapshot: Arc::clone(&self.snapshot),
        }
    }
}

impl MetadataStore for ExternalDataStore {
    fn name(&self) -> &'static str {
        "external_data"
    }

    fn process_operation(&mut self, operation: MetadataOperation) {
        debug!(?operation, "Processing metadata operation.");

        // TODO: Maybe come up with a better pattern for doing "only clone for the first N-1 actions, don't clone for the
        // Nth" since we're needlessly cloning a lot with this current approach.
        let entity_id = operation.entity_id;
        for action in operation.actions {
            match action {
                MetadataAction::AttachExternalData { external_data } => {
                    self.add_mapping(external_data, entity_id.clone());
                }
                MetadataAction::Delete => {
                    self.remove_mapping(entity_id.clone());
                }

                // We only care about external data, and knowing when to clean up mappings.
                _ => {}
            }
        }

        // Update the snapshot.
        let snapshot = Arc::new(ExternalDataSnapshot {
            forward_mappings: self.forward_mappings.clone(),
        });

        self.snapshot.store(snapshot);
    }
}

impl MemoryBounds for ExternalDataStore {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .firm()
            // Active entities.
            .with_array::<EntityId>(self.entity_limit())
            // Forward and reverse mappings.
            .with_map::<ExternalData, EntityId>(self.entity_limit())
            .with_map::<EntityId, ExternalData>(self.entity_limit());
    }
}

#[derive(Default)]
struct ExternalDataSnapshot {
    forward_mappings: FastIndexMap<ExternalData, ResolvedExternalData>,
}

/// A handle for resolving entity IDs from an `ExternalDataStore`.
#[derive(Clone)]
pub struct ExternalDataStoreResolver {
    snapshot: Arc<ArcSwap<ExternalDataSnapshot>>,
}

impl ExternalDataStoreResolver {
    /// Resolves the given raw external data and calls the given closure.
    ///
    /// The given raw external data is parsed and lookups are performed to resolve the underlying entity IDs (pod and
    /// container). If the external data maps to valid, and the referenced entities exist, the closure is called with
    /// `Some(ResolvedExternalData)`, which provides the entity IDs for both pod and container. Otherwise, if the raw
    /// external data is invalid, or the referenced entities don't exist, the closure is called with `None`.
    pub fn resolve_external_data<F>(&self, raw_external_data: &str, mut f: F)
    where
        F: FnMut(Option<&ResolvedExternalData>),
    {
        let snapshot = self.snapshot.load();
        let resolved = ExternalDataRef::from_raw(raw_external_data)
            .and_then(|external_data_ref| snapshot.forward_mappings.get(&external_data_ref));
        f(resolved)
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use super::{ExternalDataStore, ExternalDataStoreResolver};
    use crate::workload::{aggregator::MetadataStore as _, external_data::{ExternalData, ResolvedExternalData}, EntityId, MetadataOperation};

    const DEFAULT_ENTITY_LIMIT: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(10) };

    fn entity_id_container(id: &str) -> EntityId {
        EntityId::Container(id.into())
    }

    fn build_external_data(pod_uid: &str, container_name: &str) -> (ExternalData, ResolvedExternalData, String) {
        let raw = format!("pu-{},cn-{}", pod_uid, container_name);
        let pod_entity_id = EntityId::from_pod_uid(pod_uid).unwrap();
        let container_entity_id = EntityId::from_raw_container_id(container_name).unwrap();

        let external_data = ExternalData::new(pod_uid.into(), container_name.into());
        let resolved_external_data = ResolvedExternalData::new(pod_entity_id.clone(), container_entity_id.clone());
        (external_data, resolved_external_data, raw)
    }

    fn get_resolved_external_data(resolver: &ExternalDataStoreResolver, raw_external_data: &str) -> Option<ResolvedExternalData> {
        let mut maybe_resolved_external_data = None;
        resolver.resolve_external_data(raw_external_data, |maybe_external_data| maybe_resolved_external_data = maybe_external_data.cloned());
        maybe_resolved_external_data
    }

    #[test]
    fn basic() {
        let mut store = ExternalDataStore::with_entity_limit(DEFAULT_ENTITY_LIMIT);
        let resolver = store.resolver();

        let container_eid = entity_id_container("abcdef");
        let (ed, resolved_ed, raw_ed) = build_external_data("1234", "redis");

        // Make sure we don't get anything back for this External Data yet:
        assert_eq!(get_resolved_external_data(&resolver, &raw_ed), None);

        // Attach the External Data to the given container:
        store.process_operation(MetadataOperation::attach_external_data(container_eid.clone(), ed));

        // Now we should be able to resolve the External Data:
        assert_eq!(get_resolved_external_data(&resolver, &raw_ed), Some(resolved_ed));

        // Delete the container entity, which should drop the attached External Data:
        store.process_operation(MetadataOperation::delete(container_eid));

        assert_eq!(get_resolved_external_data(&resolver, &raw_ed), None);
    }

    #[test]
    fn obeys_entity_limit() {
        // Create our `ExternalDataStore` with a reduced entity limit of two:
        let mut store = ExternalDataStore::with_entity_limit(NonZeroUsize::new(2).unwrap());
        let resolver = store.resolver();

        // Make sure we don't get anything back for any of this External Data yet:
        let container_eid1 = entity_id_container("abcdef");
        let container_eid2 = entity_id_container("bcdefg");
        let container_eid3 = entity_id_container("cdefgh");
        let (ed1, resolved_ed1, raw_ed1) = build_external_data("1234", "redis");
        let (ed2, resolved_ed2, raw_ed2) = build_external_data("1234", "init-volume");
        let (ed3, resolved_ed3, raw_ed3) = build_external_data("1234", "chmod-dir");

        assert_eq!(get_resolved_external_data(&resolver, &raw_ed1), None);
        assert_eq!(get_resolved_external_data(&resolver, &raw_ed2), None);
        assert_eq!(get_resolved_external_data(&resolver, &raw_ed3), None);

        // Attach the External Data to all of the containers:
        store.process_operation(MetadataOperation::attach_external_data(container_eid1.clone(), ed1));
        store.process_operation(MetadataOperation::attach_external_data(container_eid2, ed2));
        store.process_operation(MetadataOperation::attach_external_data(container_eid3.clone(), ed3.clone()));

        // Now we should be able to resolve External Data for the first two container entities, but not the third, as we
        // have hit our entity limit:
        assert_eq!(get_resolved_external_data(&resolver, &raw_ed1), Some(resolved_ed1));
        assert_eq!(get_resolved_external_data(&resolver, &raw_ed2), Some(resolved_ed2));
        assert_eq!(get_resolved_external_data(&resolver, &raw_ed3), None);

        // Delete the first container entity, which should drop the attached External Data:
        store.process_operation(MetadataOperation::delete(container_eid1));
        assert_eq!(get_resolved_external_data(&resolver, &raw_ed1), None);

        // Try again to attach the External Data to the third container entity, which we should now be able to resolve:
        store.process_operation(MetadataOperation::attach_external_data(container_eid3, ed3));
        assert_eq!(get_resolved_external_data(&resolver, &raw_ed3), Some(resolved_ed3));
    }
}
