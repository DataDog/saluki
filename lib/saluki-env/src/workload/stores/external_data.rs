use std::{num::NonZeroUsize, sync::Arc};

use arc_swap::ArcSwap;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use tracing::debug;

use crate::{
    prelude::*,
    workload::{
        aggregator::MetadataStore,
        external_data::{ExternalData, ExternalDataRef},
        EntityId, MetadataAction, MetadataOperation,
    },
};

/// A store for External Data entity mappings.
pub struct ExternalDataStore {
    snapshot: Arc<ArcSwap<ExternalDataSnapshot>>,

    entity_limit: NonZeroUsize,
    active_entities: FastHashSet<EntityId>,

    forward_mappings: FastIndexMap<ExternalData, EntityId>,
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
            // TODO: Emit a warning log and/or a metric here.
            return;
        }

        let _ = self.forward_mappings.insert(external_data.clone(), entity_id.clone());
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

    /// Returns a `ExternalDataStoreResolver` that can be used to concurrently resolve entity IDs from external data.
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
    forward_mappings: FastIndexMap<ExternalData, EntityId>,
}

/// A handle for resolving entity IDs from a `ExternalDataStore`.
#[derive(Clone)]
pub struct ExternalDataStoreResolver {
    snapshot: Arc<ArcSwap<ExternalDataSnapshot>>,
}

impl ExternalDataStoreResolver {
    /// Resolves the entity ID attached to the given external data.
    ///
    /// If the external data is invalid, or no attached entity ID was found, `None` is returned.
    pub fn resolve_entity_id(&self, external_data: &str) -> Option<EntityId> {
        let snapshot = self.snapshot.load();

        let external_data_ref = ExternalDataRef::from_raw(external_data)?;
        snapshot.forward_mappings.get(&external_data_ref).cloned()
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use super::ExternalDataStore;
    use crate::workload::{aggregator::MetadataStore as _, external_data::ExternalData, EntityId, MetadataOperation};

    const DEFAULT_ENTITY_LIMIT: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(10) };

    fn entity_id_container(id: &str) -> EntityId {
        EntityId::Container(id.into())
    }

    fn build_external_data(pod_uid: &str, container_name: &str) -> (ExternalData, String) {
        let raw = format!("pu-{},cn-{}", pod_uid, container_name);

        (ExternalData::new(pod_uid.into(), container_name.into()), raw)
    }

    #[test]
    fn basic() {
        let (external_data, raw_external_data) = build_external_data("1234", "redis");

        let mut store = ExternalDataStore::with_entity_limit(DEFAULT_ENTITY_LIMIT);
        let resolver = store.resolver();

        assert_eq!(resolver.resolve_entity_id(&raw_external_data), None);

        let entity_id = entity_id_container("abcdef");
        store.process_operation(MetadataOperation::attach_external_data(
            entity_id.clone(),
            external_data,
        ));

        assert_eq!(resolver.resolve_entity_id(&raw_external_data), Some(entity_id.clone()));

        store.process_operation(MetadataOperation::delete(entity_id));

        assert_eq!(resolver.resolve_entity_id(&raw_external_data), None);
    }

    #[test]
    fn obeys_entity_limit() {
        let entity_limit = NonZeroUsize::new(2).unwrap();

        let (external_data1, raw_external_data1) = build_external_data("1234", "redis");
        let (external_data2, raw_external_data2) = build_external_data("1234", "init-volume");
        let (external_data3, raw_external_data3) = build_external_data("1234", "chmod-dir");

        let mut store = ExternalDataStore::with_entity_limit(entity_limit);
        let resolver = store.resolver();

        assert_eq!(resolver.resolve_entity_id(&raw_external_data1), None);
        assert_eq!(resolver.resolve_entity_id(&raw_external_data2), None);
        assert_eq!(resolver.resolve_entity_id(&raw_external_data3), None);

        let entity_id1 = entity_id_container("abcdef");
        let entity_id2 = entity_id_container("bcdefg");
        let entity_id3 = entity_id_container("cdefgh");

        store.process_operation(MetadataOperation::attach_external_data(
            entity_id1.clone(),
            external_data1,
        ));

        store.process_operation(MetadataOperation::attach_external_data(
            entity_id2.clone(),
            external_data2,
        ));

        store.process_operation(MetadataOperation::attach_external_data(
            entity_id3.clone(),
            external_data3.clone(),
        ));

        assert_eq!(
            resolver.resolve_entity_id(&raw_external_data1),
            Some(entity_id1.clone())
        );
        assert_eq!(
            resolver.resolve_entity_id(&raw_external_data2),
            Some(entity_id2.clone())
        );
        assert_eq!(resolver.resolve_entity_id(&raw_external_data3), None);

        store.process_operation(MetadataOperation::delete(entity_id1));

        assert_eq!(resolver.resolve_entity_id(&raw_external_data1), None);
        assert_eq!(
            resolver.resolve_entity_id(&raw_external_data2),
            Some(entity_id2.clone())
        );
        assert_eq!(resolver.resolve_entity_id(&raw_external_data3), None);

        store.process_operation(MetadataOperation::attach_external_data(
            entity_id3.clone(),
            external_data3,
        ));

        assert_eq!(resolver.resolve_entity_id(&raw_external_data1), None);
        assert_eq!(
            resolver.resolve_entity_id(&raw_external_data2),
            Some(entity_id2.clone())
        );
        assert_eq!(
            resolver.resolve_entity_id(&raw_external_data3),
            Some(entity_id3.clone())
        );
    }
}
