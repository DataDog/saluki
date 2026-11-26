use std::{num::NonZeroUsize, sync::Arc};

use arc_swap::ArcSwap;
use saluki_common::collections::{FastHashSet, FastIndexMap};
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    data_model::origin::{ExternalData, RawExternalData},
};
use saluki_metrics::{static_metrics, Counter, Gauge};
use tracing::{debug, trace};

use crate::workload::{
    aggregator::MetadataStore, origin::ResolvedExternalData, EntityId, MetadataAction, MetadataOperation,
};

#[static_metrics(prefix = external_data_store)]
#[derive(Clone)]
struct Telemetry {
    entity_limit: Gauge,
    active_entities: Gauge,
    ops_delete_total: Counter,
    ops_attach_external_data_total: Counter,
}

/// A store for External Data entity mappings.
///
/// "External Data" is a concept that's used to aid origin detection of workloads running in Kubernetes environments
/// where introspection isn't possible or may return incorrect information. Origin detection generally centers around
/// determining the container where a metric originates from, and then enriching the metric with tags that describe that
/// container, as well as the pod the container is running within, and so on. In some cases, the origin of a metric
/// can't be detected from the outside (such as by using peer credentials over Unix Domain sockets) and can't be
/// detected by the workload itself (such as when running in nested virtualization environments). In these cases, we
/// need a mechanism to attach metadata to the workload such that it can send the necessary information to allow for the
/// origin of a metric to be correctly detected.
///
/// "External Data" supports this by allowing for an external Kubernetes admission controller to attach specific
/// metadata -- pod UID and container name -- to application pods, which is then read and sent along with metrics. This
/// information is then used during origin detection in order to correlate the container ID of the origin, which is
/// sufficient to allow enriching the metric with container-specific tags.
///
/// See [`ExternalData`] for more information on the External Data format itself.
pub struct ExternalDataStore {
    snapshot: Arc<ArcSwap<ExternalDataSnapshot>>,
    entity_limit: NonZeroUsize,
    active_entities: FastHashSet<EntityId>,
    forward_mappings: FastIndexMap<ExternalData, ResolvedExternalData>,
    reverse_mappings: FastIndexMap<EntityId, ExternalData>,
    telemetry: Telemetry,
}

impl ExternalDataStore {
    /// Creates a new `ExternalDataStore` with the given entity limit.
    ///
    /// The entity limit is the maximum number of unique entities that can be stored. Once the limit is reached, new
    /// entities won't be added to the store.
    pub fn with_entity_limit(entity_limit: NonZeroUsize) -> Self {
        let telemetry = Telemetry::new();
        telemetry.entity_limit().set(entity_limit.get() as f64);

        Self {
            snapshot: Arc::new(ArcSwap::new(Arc::new(ExternalDataSnapshot::default()))),
            entity_limit,
            active_entities: FastHashSet::default(),
            forward_mappings: FastIndexMap::default(),
            reverse_mappings: FastIndexMap::default(),
            telemetry,
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

        self.telemetry.active_entities().increment(1);
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

        if let Some(previous) = self.reverse_mappings.insert(entity_id.clone(), external_data.clone()) {
            if previous != external_data {
                self.remove_forward_mapping_if_owned(&previous, &entity_id);
            }
        }

        // We create a "resolved" form of the External Data, which includes entity IDs for both the pod and the
        // container that this External Data is attached to.
        let resolved = ResolvedExternalData::new(EntityId::PodUid(external_data.pod_uid().clone()), entity_id);
        let _ = self.forward_mappings.insert(external_data, resolved);
    }

    fn remove_mapping(&mut self, entity_id: EntityId) {
        if !self.active_entities.remove(&entity_id) {
            return;
        }

        self.telemetry.active_entities().decrement(1);

        if let Some(external_data) = self.reverse_mappings.swap_remove(&entity_id) {
            self.remove_forward_mapping_if_owned(&external_data, &entity_id);
        }
    }

    /// Removes the forward mapping for the given External Data if it still resolves to the given entity.
    ///
    /// A newer entity can take over the same External Data, such as when Kubernetes restarts a container under the same
    /// pod UID and container name. The forward mapping then belongs to the newer entity, and a late delete of the older
    /// entity must not remove it.
    fn remove_forward_mapping_if_owned(&mut self, external_data: &ExternalData, entity_id: &EntityId) {
        let is_owned = self
            .forward_mappings
            .get(external_data)
            .is_some_and(|resolved| resolved.container_entity_id() == entity_id);
        if is_owned {
            let _ = self.forward_mappings.swap_remove(external_data);
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
                    self.telemetry.ops_attach_external_data_total().increment(1);
                    self.add_mapping(external_data, entity_id.clone());
                }
                MetadataAction::Delete => {
                    self.telemetry.ops_delete_total().increment(1);
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
            .with_array::<EntityId>("entity ids", self.entity_limit())
            // Forward and reverse mappings.
            .with_map::<ExternalData, EntityId>("ext data entity map", self.entity_limit())
            .with_map::<EntityId, ExternalData>("entity ext data map", self.entity_limit());
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
    /// Resolves the given raw external data.
    ///
    /// The given raw external data is parsed and lookups are performed to resolve the underlying entity IDs (pod and
    /// container). If the external data maps to valid, and the referenced entities exist, `Some(ResolvedExternalData)`
    /// is returned, containing the entity IDs for both pod and container. Otherwise, if the raw external data is
    /// invalid, or the referenced entities don't exist, `None` is returned.
    pub fn resolve(&self, external_data: &RawExternalData<'_>) -> Option<ResolvedExternalData> {
        let snapshot = self.snapshot.load();
        snapshot.forward_mappings.get(external_data).cloned()
    }

    /// Executes the given function for each forward mapping in the latest snapshot.
    pub fn with_latest_snapshot<F>(&self, mut f: F)
    where
        F: FnMut(&ExternalData, &EntityId),
    {
        let snapshot = self.snapshot.load();
        for (external_data, resolved_ed) in snapshot.forward_mappings.iter() {
            f(external_data, resolved_ed.container_entity_id());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use saluki_core::data_model::origin::{ExternalData, RawExternalData};

    use super::ExternalDataStore;
    use crate::workload::{aggregator::MetadataStore as _, origin::ResolvedExternalData, EntityId, MetadataOperation};

    const DEFAULT_ENTITY_LIMIT: NonZeroUsize = NonZeroUsize::new(10).unwrap();

    fn entity_id_container(id: &str) -> EntityId {
        EntityId::Container(id.into())
    }

    fn build_external_data(
        pod_uid: &str, container_name: &str, container_id: &EntityId, init_container: bool,
    ) -> (String, ExternalData, ResolvedExternalData) {
        let raw_external_data = format!("pu-{},cn-{},it-{}", pod_uid, container_name, init_container);
        let pod_entity_id = EntityId::from_pod_uid(pod_uid).unwrap();

        let external_data = ExternalData::new(pod_uid.into(), container_name.into(), init_container);
        let resolved_external_data = ResolvedExternalData::new(pod_entity_id.clone(), container_id.clone());
        (raw_external_data, external_data, resolved_external_data)
    }

    #[test]
    fn basic() {
        let mut store = ExternalDataStore::with_entity_limit(DEFAULT_ENTITY_LIMIT);
        let resolver = store.resolver();

        let container_eid = entity_id_container("abcdef");
        let (raw_ed, ed, resolved_ed) = build_external_data("1234", "redis", &container_eid, false);
        let ed_ref = RawExternalData::try_from_str(&raw_ed).unwrap();

        // Make sure we don't get anything back for this External Data yet:
        assert_eq!(resolver.resolve(&ed_ref), None);

        // Attach the External Data to the given container:
        store.process_operation(MetadataOperation::attach_external_data(container_eid.clone(), ed));

        // Now we should be able to resolve the External Data:
        assert_eq!(resolver.resolve(&ed_ref), Some(resolved_ed));

        // Delete the container entity, which should drop the attached External Data:
        store.process_operation(MetadataOperation::delete(container_eid));

        assert_eq!(resolver.resolve(&ed_ref), None);
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
        let (raw_ed1, ed1, resolved_ed1) = build_external_data("1234", "redis", &container_eid1, false);
        let (raw_ed2, ed2, resolved_ed2) = build_external_data("1234", "init-volume", &container_eid2, true);
        let (raw_ed3, ed3, resolved_ed3) = build_external_data("1234", "chmod-dir", &container_eid3, true);
        let ed_ref1 = RawExternalData::try_from_str(&raw_ed1).unwrap();
        let ed_ref2 = RawExternalData::try_from_str(&raw_ed2).unwrap();
        let ed_ref3 = RawExternalData::try_from_str(&raw_ed3).unwrap();

        assert_eq!(resolver.resolve(&ed_ref1), None);
        assert_eq!(resolver.resolve(&ed_ref2), None);
        assert_eq!(resolver.resolve(&ed_ref3), None);

        // Attach the External Data to all of the containers:
        store.process_operation(MetadataOperation::attach_external_data(container_eid1.clone(), ed1));
        store.process_operation(MetadataOperation::attach_external_data(container_eid2, ed2));
        store.process_operation(MetadataOperation::attach_external_data(
            container_eid3.clone(),
            ed3.clone(),
        ));

        // Now we should be able to resolve External Data for the first two container entities, but not the third, as we
        // have hit our entity limit:
        assert_eq!(resolver.resolve(&ed_ref1), Some(resolved_ed1));
        assert_eq!(resolver.resolve(&ed_ref2), Some(resolved_ed2));
        assert_eq!(resolver.resolve(&ed_ref3), None);

        // Delete the first container entity, which should drop the attached External Data:
        store.process_operation(MetadataOperation::delete(container_eid1));
        assert_eq!(resolver.resolve(&ed_ref1), None);

        // Try again to attach the External Data to the third container entity, which we should now be able to resolve:
        store.process_operation(MetadataOperation::attach_external_data(container_eid3, ed3));
        assert_eq!(resolver.resolve(&ed_ref3), Some(resolved_ed3));
    }

    #[test]
    fn late_delete_of_replaced_container_keeps_mapping() {
        let mut store = ExternalDataStore::with_entity_limit(DEFAULT_ENTITY_LIMIT);
        let resolver = store.resolver();

        let old_eid = entity_id_container("old");
        let new_eid = entity_id_container("new");
        let (raw_ed, ed, _) = build_external_data("1234", "app", &old_eid, false);
        let (_, _, new_resolved_ed) = build_external_data("1234", "app", &new_eid, false);
        let ed_ref = RawExternalData::try_from_str(&raw_ed).unwrap();

        // Kubernetes restarts the container under the same pod UID and container name, so the new container gets the
        // same External Data:
        store.process_operation(MetadataOperation::attach_external_data(old_eid.clone(), ed.clone()));
        store.process_operation(MetadataOperation::attach_external_data(new_eid.clone(), ed));
        assert_eq!(resolver.resolve(&ed_ref), Some(new_resolved_ed.clone()));

        // A late delete of the old container must not remove the mapping that now belongs to the new container:
        store.process_operation(MetadataOperation::delete(old_eid));
        assert_eq!(resolver.resolve(&ed_ref), Some(new_resolved_ed));
        assert_eq!(store.active_entities.len(), 1);

        // Deleting the new container still removes the mapping:
        store.process_operation(MetadataOperation::delete(new_eid));
        assert_eq!(resolver.resolve(&ed_ref), None);
        assert!(store.active_entities.is_empty());
        assert!(store.forward_mappings.is_empty());
        assert!(store.reverse_mappings.is_empty());
    }

    #[test]
    fn reattach_with_different_external_data_drops_previous_mapping() {
        let mut store = ExternalDataStore::with_entity_limit(DEFAULT_ENTITY_LIMIT);
        let resolver = store.resolver();

        let eid = entity_id_container("abcdef");
        let other_eid = entity_id_container("bcdefg");
        let (raw_ed1, ed1, _) = build_external_data("1234", "app", &eid, false);
        let (raw_ed2, ed2, resolved_ed2) = build_external_data("1234", "sidecar", &eid, false);
        let (raw_ed3, ed3, _) = build_external_data("5678", "app", &eid, false);
        let (_, _, other_resolved_ed3) = build_external_data("5678", "app", &other_eid, false);
        let ed_ref1 = RawExternalData::try_from_str(&raw_ed1).unwrap();
        let ed_ref2 = RawExternalData::try_from_str(&raw_ed2).unwrap();
        let ed_ref3 = RawExternalData::try_from_str(&raw_ed3).unwrap();

        // Re-attaching with different External Data must not leave the previous External Data resolving to the entity:
        store.process_operation(MetadataOperation::attach_external_data(eid.clone(), ed1));
        store.process_operation(MetadataOperation::attach_external_data(eid.clone(), ed2));
        assert_eq!(resolver.resolve(&ed_ref1), None);
        assert_eq!(resolver.resolve(&ed_ref2), Some(resolved_ed2));

        // Re-attaching must not remove a previous External Data mapping that another entity took over:
        store.process_operation(MetadataOperation::attach_external_data(eid.clone(), ed3.clone()));
        store.process_operation(MetadataOperation::attach_external_data(other_eid, ed3));
        let (_, ed4, _) = build_external_data("5678", "other", &eid, false);
        store.process_operation(MetadataOperation::attach_external_data(eid.clone(), ed4));
        assert_eq!(resolver.resolve(&ed_ref2), None);
        assert_eq!(resolver.resolve(&ed_ref3), Some(other_resolved_ed3.clone()));

        // Deleting the entity must not remove it either:
        store.process_operation(MetadataOperation::delete(eid));
        assert_eq!(resolver.resolve(&ed_ref3), Some(other_resolved_ed3));
    }
}
