use std::{collections::VecDeque, num::NonZeroUsize, sync::Arc};

use arc_swap::ArcSwap;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_context::TagSet;
use saluki_event::metric::OriginTagCardinality;
use tracing::{debug, trace};

use crate::{
    prelude::*,
    workload::{
        aggregator::MetadataStore,
        entity::EntityId,
        metadata::{MetadataAction, MetadataOperation},
    },
};

// TODO: This will be very slow if we deliver metadata operations one-by-one, especially when collectors will likely be
// generating many of these operations in a single go, and we could potentially defer incremental resolving until after
// processing a whole batch of operations.
//
// We at least will likely want to support taking a batch of operations, and maybe move the incremental resolving logic
// to `process_operations` (theoretical name for method) so that we can give it a list of entity IDs to resolve, based
// on the operations we processed, and then adjust the resolving code to start from a list of IDs instead of a single
// one.

/// A unified tag store for entities.
///
/// Entities will, in general, have a number of specific tags associated with them. Additionally, many entities will be
/// related to other entities, such as a container belong to a specific pod, and so on. [`TagStore`] is designed to hold
/// all of the information necessary to compute the "unified" tag set of an entity based on combining the tags of the
/// entity itself and its ancestors.
pub struct TagStore {
    snapshot: Arc<ArcSwap<TagSnapshot>>,

    entity_limit: NonZeroUsize,
    active_entities: FastHashSet<EntityId>,

    entity_hierarchy_mappings: FastHashMap<EntityId, EntityId>,

    low_cardinality_entity_tags: FastHashMap<EntityId, TagSet>,
    orchestrator_cardinality_entity_tags: FastHashMap<EntityId, TagSet>,
    high_cardinality_entity_tags: FastHashMap<EntityId, TagSet>,

    unified_low_cardinality_entity_tags: FastHashMap<EntityId, TagSet>,
    unified_orchestrator_cardinality_entity_tags: FastHashMap<EntityId, TagSet>,
    unified_high_cardinality_entity_tags: FastHashMap<EntityId, TagSet>,
}

impl TagStore {
    /// Creates a new `TagStore` with the given entity limit.
    ///
    /// The entity limit is the maximum number of unique entities that can be stored. Once the limit is reached, new
    /// entities will not be added to the store.
    pub fn with_entity_limit(entity_limit: NonZeroUsize) -> Self {
        Self {
            snapshot: Arc::new(ArcSwap::new(Arc::new(TagSnapshot::default()))),
            entity_limit,
            active_entities: FastHashSet::default(),
            entity_hierarchy_mappings: FastHashMap::default(),
            low_cardinality_entity_tags: FastHashMap::default(),
            orchestrator_cardinality_entity_tags: FastHashMap::default(),
            high_cardinality_entity_tags: FastHashMap::default(),
            unified_low_cardinality_entity_tags: FastHashMap::default(),
            unified_orchestrator_cardinality_entity_tags: FastHashMap::default(),
            unified_high_cardinality_entity_tags: FastHashMap::default(),
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

    fn delete_entity(&mut self, entity_id: EntityId) {
        self.active_entities.remove(&entity_id);

        // Delete all of the tags for the entity, both raw and unified.
        self.low_cardinality_entity_tags.remove(&entity_id);
        self.high_cardinality_entity_tags.remove(&entity_id);
        self.unified_low_cardinality_entity_tags.remove(&entity_id);
        self.unified_high_cardinality_entity_tags.remove(&entity_id);

        // Delete the ancestry mapping, if it exists, for the entity itself.
        self.entity_hierarchy_mappings.remove(&entity_id);

        // Iterate over all ancestry mappings, and delete any which reference this entity as an ancestor. We'll keep a
        // list of the entity IDs which reference this entity as an ancestor, as we'll need to regenerate their tags
        // after breaking the ancestry link.
        let mut entity_ids_to_resolve = Vec::new();
        for (descendant_entity_id, ancestor_entity_id) in self.entity_hierarchy_mappings.iter() {
            if ancestor_entity_id == &entity_id {
                entity_ids_to_resolve.push(descendant_entity_id.clone());
            }
        }

        for entity_id in entity_ids_to_resolve {
            self.entity_hierarchy_mappings.remove(&entity_id);
            self.regenerate_entity_tags(entity_id);
        }
    }

    fn add_hierarchy_mapping(&mut self, child_entity_id: EntityId, parent_entity_id: EntityId) {
        let _ = self
            .entity_hierarchy_mappings
            .insert(child_entity_id.clone(), parent_entity_id);
        self.regenerate_entity_tags(child_entity_id);
    }

    fn add_entity_tags(&mut self, entity_id: EntityId, tags: TagSet, cardinality: OriginTagCardinality) {
        if !self.track_entity(&entity_id) {
            trace!(
                entity_limit = self.entity_limit(),
                %entity_id,
                "Entity limit reached, not adding tags for entity."
            );
            return;
        }

        let existing_tags = match cardinality {
            OriginTagCardinality::Low => self.low_cardinality_entity_tags.entry(entity_id.clone()).or_default(),
            OriginTagCardinality::Orchestrator => self
                .orchestrator_cardinality_entity_tags
                .entry(entity_id.clone())
                .or_default(),
            OriginTagCardinality::High => self.high_cardinality_entity_tags.entry(entity_id.clone()).or_default(),
        };
        existing_tags.extend(tags.into_iter().map(Into::into));

        self.regenerate_entity_tags(entity_id);
    }

    fn set_entity_tags(&mut self, entity_id: EntityId, tags: TagSet, cardinality: OriginTagCardinality) {
        if !self.track_entity(&entity_id) {
            trace!(
                entity_limit = self.entity_limit(),
                %entity_id,
                "Entity limit reached, not setting tags for entity."
            );
            return;
        }

        let existing_tags = match cardinality {
            OriginTagCardinality::Low => self.low_cardinality_entity_tags.entry(entity_id.clone()).or_default(),
            OriginTagCardinality::Orchestrator => self
                .orchestrator_cardinality_entity_tags
                .entry(entity_id.clone())
                .or_default(),
            OriginTagCardinality::High => self.high_cardinality_entity_tags.entry(entity_id.clone()).or_default(),
        };
        *existing_tags = tags;

        self.regenerate_entity_tags(entity_id);
    }

    fn regenerate_entity_tags(&mut self, entity_id: EntityId) {
        // We want to incrementally resolve the unified tags for the given entity, which is directional: we have to
        // regenerate the tags for both the entity itself _and_ any descendants of the entity, based on our ancestry
        // mapping.
        //
        // We'll regenerate unified tags for the entity itself first, and then for any descendants.
        let new_low_cardinality_entity_tags = self.resolve_entity_tags(&entity_id, OriginTagCardinality::Low);
        let new_orchestrator_cardinality_entity_tags =
            self.resolve_entity_tags(&entity_id, OriginTagCardinality::Orchestrator);
        let new_high_cardinality_entity_tags = self.resolve_entity_tags(&entity_id, OriginTagCardinality::High);

        match self.unified_low_cardinality_entity_tags.get_mut(&entity_id) {
            Some(existing_tags) => *existing_tags = new_low_cardinality_entity_tags,
            None => {
                self.unified_low_cardinality_entity_tags
                    .insert(entity_id.clone(), new_low_cardinality_entity_tags);
            }
        }

        match self.unified_orchestrator_cardinality_entity_tags.get_mut(&entity_id) {
            Some(existing_tags) => *existing_tags = new_orchestrator_cardinality_entity_tags,
            None => {
                self.unified_orchestrator_cardinality_entity_tags
                    .insert(entity_id.clone(), new_orchestrator_cardinality_entity_tags);
            }
        }

        match self.unified_high_cardinality_entity_tags.get_mut(&entity_id) {
            Some(existing_tags) => *existing_tags = new_high_cardinality_entity_tags,
            None => {
                self.unified_high_cardinality_entity_tags
                    .insert(entity_id.clone(), new_high_cardinality_entity_tags);
            }
        }

        // Now, we'll re-resolve tags for any descendants of the entity.
        //
        // We take a breadth-first approach, where we start with the entity itself, and then process its direct
        // descendants: re-resolve their tags and add _their_ descendants to the stack. We keep popping entities from
        // the stack, processing them in this way, until the stack is empty.
        let mut entity_stack = VecDeque::new();

        // Seed the entity stack with our initial entity.
        for (descendant_entity_id, ancestor_entity_id) in self.entity_hierarchy_mappings.iter() {
            if ancestor_entity_id == &entity_id {
                entity_stack.push_back(descendant_entity_id.clone());
            }
        }

        while let Some(sub_entity_id) = entity_stack.pop_front() {
            let new_low_cardinality_entity_tags = self.resolve_entity_tags(&sub_entity_id, OriginTagCardinality::Low);
            let new_orchestrator_cardinality_entity_tags =
                self.resolve_entity_tags(&sub_entity_id, OriginTagCardinality::Orchestrator);
            let new_high_cardinality_entity_tags = self.resolve_entity_tags(&sub_entity_id, OriginTagCardinality::High);

            match self.unified_low_cardinality_entity_tags.get_mut(&sub_entity_id) {
                Some(existing_tags) => *existing_tags = new_low_cardinality_entity_tags,
                None => {
                    self.unified_low_cardinality_entity_tags
                        .insert(sub_entity_id.clone(), new_low_cardinality_entity_tags);
                }
            }

            match self
                .unified_orchestrator_cardinality_entity_tags
                .get_mut(&sub_entity_id)
            {
                Some(existing_tags) => *existing_tags = new_orchestrator_cardinality_entity_tags,
                None => {
                    self.unified_orchestrator_cardinality_entity_tags
                        .insert(sub_entity_id.clone(), new_orchestrator_cardinality_entity_tags);
                }
            }

            match self.unified_high_cardinality_entity_tags.get_mut(&sub_entity_id) {
                Some(existing_tags) => *existing_tags = new_high_cardinality_entity_tags,
                None => {
                    self.unified_high_cardinality_entity_tags
                        .insert(sub_entity_id.clone(), new_high_cardinality_entity_tags);
                }
            }

            // Add any descendants of the current entity to the stack.
            for (descendant_entity_id, ancestor_entity_id) in self.entity_hierarchy_mappings.iter() {
                if ancestor_entity_id == &sub_entity_id {
                    entity_stack.push_back(descendant_entity_id.clone());
                }
            }
        }
    }

    fn get_raw_entity_tags(&self, entity_id: &EntityId, cardinality: OriginTagCardinality) -> Option<TagSet> {
        match cardinality {
            OriginTagCardinality::Low => self.low_cardinality_entity_tags.get(entity_id).cloned(),
            OriginTagCardinality::Orchestrator => {
                // First we'll get the low cardinality tags and then append the orchestrator cardinality tags to those.
                let low_cardinality_tags = self.get_raw_entity_tags(entity_id, OriginTagCardinality::Low);
                let orchestrator_cardinality_tags = self.orchestrator_cardinality_entity_tags.get(entity_id).cloned();

                match (low_cardinality_tags, orchestrator_cardinality_tags) {
                    (Some(mut lct), Some(oct)) => {
                        lct.extend(oct);
                        Some(lct)
                    }
                    (Some(tags), None) => Some(tags),
                    (None, Some(tags)) => Some(tags),
                    (None, None) => None,
                }
            }
            OriginTagCardinality::High => {
                // First we'll get the orchestrator cardinality tags and then append the high cardinality tags to those.
                let orchestrator_cardinality_tags =
                    self.get_raw_entity_tags(entity_id, OriginTagCardinality::Orchestrator);
                let high_cardinality_tags = self.high_cardinality_entity_tags.get(entity_id).cloned();

                match (orchestrator_cardinality_tags, high_cardinality_tags) {
                    (Some(mut oct), Some(hct)) => {
                        oct.extend(hct);
                        Some(oct)
                    }
                    (Some(tags), None) => Some(tags),
                    (None, Some(tags)) => Some(tags),
                    (None, None) => None,
                }
            }
        }
    }

    fn resolve_entity_tags(&self, entity_id: &EntityId, cardinality: OriginTagCardinality) -> TagSet {
        // Build the ancestry chain for the entity, starting with the entity itself.
        let mut entity_chain = VecDeque::new();
        entity_chain.push_back(entity_id);

        loop {
            let current = entity_chain.front().expect("entity chain can never be empty");
            if let Some(ancestor) = self.entity_hierarchy_mappings.get(*current) {
                entity_chain.push_front(ancestor);
            } else {
                break;
            }
        }

        // For each entity in the chain, grab their tags, and merge them into the unified tags.
        let mut unified_tags = TagSet::default();

        for ancestor_entity_id in entity_chain {
            if let Some(tags) = self.get_raw_entity_tags(ancestor_entity_id, cardinality) {
                unified_tags.merge_missing(tags);
            }
        }

        // Finally, merge in any global tags that are present.
        let global_tags = self
            .get_raw_entity_tags(&EntityId::Global, cardinality)
            .unwrap_or_default();
        unified_tags.merge_missing(global_tags);

        unified_tags
    }

    /// Returns a `TagStoreQuerier` that can be used to concurrently query the tag store.
    pub fn querier(&self) -> TagStoreQuerier {
        TagStoreQuerier {
            snapshot: Arc::clone(&self.snapshot),
        }
    }
}

impl MetadataStore for TagStore {
    fn name(&self) -> &'static str {
        "tag_store"
    }

    fn process_operation(&mut self, operation: MetadataOperation) {
        debug!(?operation, "Processing metadata operation.");

        // TODO: Maybe come up with a better pattern for doing "only clone for the first N-1 actions, don't clone for the
        // Nth" since we're needlessly cloning a lot with this current approach.
        let entity_id = operation.entity_id;
        for action in operation.actions {
            match action {
                MetadataAction::Delete => self.delete_entity(entity_id.clone()),
                MetadataAction::LinkAncestor { ancestor_entity_id } => {
                    self.add_hierarchy_mapping(entity_id.clone(), ancestor_entity_id)
                }
                MetadataAction::LinkDescendant { descendant_entity_id } => {
                    self.add_hierarchy_mapping(descendant_entity_id.clone(), entity_id.clone())
                }
                MetadataAction::AddTag { cardinality, tag } => {
                    self.add_entity_tags(entity_id.clone(), tag.clone().into(), cardinality)
                }
                MetadataAction::AddTags { cardinality, tags } => {
                    self.add_entity_tags(entity_id.clone(), tags, cardinality)
                }
                MetadataAction::SetTags { cardinality, tags } => {
                    self.set_entity_tags(entity_id.clone(), tags, cardinality)
                }
                // We don't care about External Data.
                MetadataAction::AttachExternalData { .. } => {}
            }
        }

        // Update the snapshot.
        let snapshot = Arc::new(TagSnapshot {
            low_cardinality_entity_tags: self.unified_low_cardinality_entity_tags.clone(),
            high_cardinality_entity_tags: self.unified_high_cardinality_entity_tags.clone(),
        });

        self.snapshot.store(snapshot);
    }
}

impl MemoryBounds for TagStore {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // TODO: We don't properly consider the hierarchy mappings here.
        //
        // The problem with entity hierarchy mappings is that they're essentially unbounded. Since we don't require
        // actually having tags present for a linked ancestor, you could just have however many ancestry links as you
        // want, and thus the unbounded aspect.
        //
        // Conceptually, every ancestry link ought to represent two entities that both have tags, otherwise it's kind of
        // useless to link them... but I'll need to think about how we scope down the flexibility so that we can
        // calculate a proper bound.
        //
        // For now, we'll use a reasonable guess of 1x the entity limit: since we generally only link container PIDs to
        // container IDs, we would expect to have no more links than entities with tags, and thus 1x the entity limit.

        builder
            .firm()
            // Active entities.
            .with_array::<EntityId>(self.entity_limit())
            // Entity hierarchy mappings.
            //
            // See TODO note about why this is an estimate.
            .with_map::<EntityId, EntityId>(self.entity_limit())
            // Low cardinality entity tags.
            .with_map::<EntityId, TagSet>(self.entity_limit())
            // High cardinality entity tags.
            .with_map::<EntityId, TagSet>(self.entity_limit())
            // Unified low cardinality entity tags.
            .with_map::<EntityId, TagSet>(self.entity_limit())
            // Unified high cardinality entity tags.
            .with_map::<EntityId, TagSet>(self.entity_limit());
    }
}

#[derive(Default)]
struct TagSnapshot {
    low_cardinality_entity_tags: FastHashMap<EntityId, TagSet>,
    high_cardinality_entity_tags: FastHashMap<EntityId, TagSet>,
}

/// A handle for querying entity tags from a `TagStore`.
#[derive(Clone)]
pub struct TagStoreQuerier {
    snapshot: Arc<ArcSwap<TagSnapshot>>,
}

impl TagStoreQuerier {
    /// Gets the tags for an entity at the given cardinality.
    ///
    /// If no tags can be found for the entity, or at the given cardinality, `None` is returned.
    pub fn get_entity_tags(&self, entity_id: &EntityId, cardinality: OriginTagCardinality) -> Option<TagSet> {
        let snapshot = self.snapshot.load();

        match cardinality {
            OriginTagCardinality::Low => snapshot.low_cardinality_entity_tags.get(entity_id).cloned(),
            OriginTagCardinality::Orchestrator => snapshot.high_cardinality_entity_tags.get(entity_id).cloned(),
            OriginTagCardinality::High => snapshot.high_cardinality_entity_tags.get(entity_id).cloned(),
        }
    }
}

// NOTE: All of the unit tests that deal with merging the "expected tags" by using `Extend` are designed/ordered to
// avoid creating tags with multiple values, since the logic for `TagSet` doesn't replace existing tags, but simply
// aggregates the values of existing tags.
#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use saluki_context::TagSet;
    use saluki_event::metric::OriginTagCardinality;

    use super::*;
    use crate::workload::helpers::OneOrMany;

    const DEFAULT_ENTITY_LIMIT: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(10) };

    fn link_ancestor(child: &EntityId, ancestor: &EntityId) -> MetadataOperation {
        MetadataOperation::link_ancestor(child.clone(), ancestor.clone())
    }

    macro_rules! low_cardinality {
		($entity_id:expr, tags => [$($key:literal => $value:literal),+]) => {{
			tag_values!($entity_id, OriginTagCardinality::Low, tags => [$($key => $value,)+])
		}};
	}

    macro_rules! high_cardinality {
		($entity_id:expr, tags => [$($key:literal => $value:literal),+]) => {{
			tag_values!($entity_id, OriginTagCardinality::High, tags => [$($key => $value,)+])
		}};
	}

    macro_rules! tag_values {
		($entity_id:expr, $cardinality:expr, tags => [$($key:literal => $value:literal),+ $(,)?]) => {{
			let mut expected_tags = TagSet::default();
			let mut operations = Vec::new();

			$(
				let tag = format!("{}:{}", $key, $value);
				expected_tags.insert_tag(tag.clone());

				operations.push(MetadataOperation {
					entity_id: $entity_id.clone(),
					actions: OneOrMany::One(MetadataAction::AddTag {
						cardinality: $cardinality,
						tag: tag.into(),
					}),
				});
			)+

			(expected_tags, operations)
		}};
	}

    #[test]
    fn basic_entity() {
        let entity_id = EntityId::Container("container-id".into());
        let (expected_tags, operations) = low_cardinality!(&entity_id, tags => ["service" => "foo"]);

        let mut store = TagStore::with_entity_limit(DEFAULT_ENTITY_LIMIT);
        for operation in operations {
            store.process_operation(operation);
        }

        let querier = store.querier();

        let unified_tags = querier.get_entity_tags(&entity_id, OriginTagCardinality::Low).unwrap();
        assert_eq!(unified_tags, expected_tags);
    }

    #[test]
    fn high_cardinality_is_superset() {
        let entity_id = EntityId::Container("container-id".into());
        let (low_card_expected_tags, low_card_operations) = low_cardinality!(&entity_id, tags => ["service" => "foo"]);
        let (mut high_card_expected_tags, high_card_operations) =
            high_cardinality!(&entity_id, tags => ["pod" => "foo-8xl-ah2z7"]);

        // Make sure to make our expected high cardinality tags a superset.
        high_card_expected_tags.extend(low_card_expected_tags.clone());

        let mut store = TagStore::with_entity_limit(DEFAULT_ENTITY_LIMIT);
        for operation in low_card_operations {
            store.process_operation(operation);
        }
        for operation in high_card_operations {
            store.process_operation(operation);
        }

        let querier = store.querier();

        let low_card_unified_tags = querier.get_entity_tags(&entity_id, OriginTagCardinality::Low).unwrap();
        assert_eq!(low_card_unified_tags.as_sorted(), low_card_expected_tags.as_sorted());

        let high_card_unified_tags = querier.get_entity_tags(&entity_id, OriginTagCardinality::High).unwrap();
        assert_eq!(high_card_unified_tags.as_sorted(), high_card_expected_tags.as_sorted());
    }

    #[test]
    fn global_tags() {
        let global_entity_id = EntityId::Global;
        let (global_expected_tags, global_operations) =
            low_cardinality!(&global_entity_id, tags => ["kube_cluster_name" => "saluki"]);

        let entity_id = EntityId::Container("container-id".into());
        let (mut expected_tags, operations) = low_cardinality!(&entity_id, tags => ["service" => "foo"]);

        expected_tags.extend(global_expected_tags.clone());

        let mut store = TagStore::with_entity_limit(DEFAULT_ENTITY_LIMIT);
        for operation in global_operations {
            store.process_operation(operation);
        }
        for operation in operations {
            store.process_operation(operation);
        }

        let querier = store.querier();

        let global_unified_tags = querier
            .get_entity_tags(&global_entity_id, OriginTagCardinality::Low)
            .unwrap();
        assert_eq!(global_unified_tags.as_sorted(), global_expected_tags.as_sorted());

        let unified_tags = querier.get_entity_tags(&entity_id, OriginTagCardinality::High).unwrap();
        assert_eq!(unified_tags.as_sorted(), expected_tags.as_sorted());
    }

    #[test]
    fn ancestors() {
        // We establish a three-level hierarchy -- pod -> container ID -> container PID -- which is indeed not a
        // real-world thing but we're just doing it to have more than one level of ancestry, to better exercise the
        // resolution logic.
        let pod_entity_id = EntityId::PodUid("datadog-agent-pod-uid".into());
        let (pod_expected_tags, pod_operations) =
            low_cardinality!(&pod_entity_id, tags => ["kube_pod_name" => "datadog-agent-z1ha3"]);

        let container_entity_id = EntityId::Container("process-agent-container-id".into());
        let (mut container_expected_tags, container_operations) =
            low_cardinality!(&container_entity_id, tags => ["service" => "foo"]);

        container_expected_tags.extend(pod_expected_tags.clone());

        let container_pid_entity_id = EntityId::ContainerPid(422);
        let (mut container_pid_expected_tags, container_pid_operations) =
            low_cardinality!(&container_pid_entity_id, tags => ["pid" => "422"]);

        container_pid_expected_tags.extend(container_expected_tags.clone());

        let mut store = TagStore::with_entity_limit(DEFAULT_ENTITY_LIMIT);
        for operation in pod_operations {
            store.process_operation(operation);
        }
        for operation in container_operations {
            store.process_operation(operation);
        }
        for operation in container_pid_operations {
            store.process_operation(operation);
        }

        // Crucially, we add two ancestry links: pod -> container ID, and container ID -> container PID.
        store.process_operation(link_ancestor(&container_entity_id, &pod_entity_id));
        store.process_operation(link_ancestor(&container_pid_entity_id, &container_entity_id));

        let querier = store.querier();

        let pod_unified_tags = querier
            .get_entity_tags(&pod_entity_id, OriginTagCardinality::Low)
            .unwrap();
        assert_eq!(pod_unified_tags.as_sorted(), pod_expected_tags.as_sorted());

        let container_unified_tags = querier
            .get_entity_tags(&container_entity_id, OriginTagCardinality::Low)
            .unwrap();
        assert_eq!(container_unified_tags.as_sorted(), container_expected_tags.as_sorted());

        let container_pid_unified_tags = querier
            .get_entity_tags(&container_pid_entity_id, OriginTagCardinality::Low)
            .unwrap();
        assert_eq!(
            container_pid_unified_tags.as_sorted(),
            container_pid_expected_tags.as_sorted()
        );
    }

    #[test]
    fn direct_resolve() {
        let entity_id = EntityId::Container("container-id".into());
        let (expected_tags, operations) = low_cardinality!(&entity_id, tags => ["service" => "foo"]);

        let mut store = TagStore::with_entity_limit(DEFAULT_ENTITY_LIMIT);
        for operation in operations {
            store.process_operation(operation);
        }

        let querier = store.querier();

        let unified_tags = querier.get_entity_tags(&entity_id, OriginTagCardinality::Low).unwrap();
        assert_eq!(unified_tags.as_sorted(), expected_tags.clone().as_sorted());

        // Create a new set of metadata entries to add an additional tag, and observe that processing the entry updates
        // the resolved tags for our entity.
        let (mut new_expected_tags, new_operations) = low_cardinality!(&entity_id, tags => ["app" => "bar"]);
        new_expected_tags.extend(expected_tags.clone());

        for operation in new_operations {
            store.process_operation(operation);
        }

        let querier = store.querier();

        let new_unified_tags = querier.get_entity_tags(&entity_id, OriginTagCardinality::Low).unwrap();
        assert_eq!(new_unified_tags.as_sorted(), new_expected_tags.as_sorted());
    }

    #[test]
    fn ancestor_resolve() {
        let pod_entity_id = EntityId::PodUid("datadog-agent-pod-uid".into());
        let (pod_expected_tags, pod_operations) =
            low_cardinality!(&pod_entity_id, tags => ["kube_pod_name" => "datadog-agent-z1ha3"]);

        let container_entity_id = EntityId::Container("process-agent-container-id".into());
        let (mut container_expected_tags, container_operations) =
            low_cardinality!(&container_entity_id, tags => ["service" => "foo"]);

        container_expected_tags.extend(pod_expected_tags.clone());

        let mut store = TagStore::with_entity_limit(DEFAULT_ENTITY_LIMIT);
        for operation in pod_operations {
            store.process_operation(operation);
        }
        for operation in container_operations {
            store.process_operation(operation);
        }

        store.process_operation(link_ancestor(&container_entity_id, &pod_entity_id));

        let querier = store.querier();

        let pod_unified_tags = querier
            .get_entity_tags(&pod_entity_id, OriginTagCardinality::Low)
            .unwrap();
        assert_eq!(pod_unified_tags.as_sorted(), pod_expected_tags.clone().as_sorted());

        let container_unified_tags = querier
            .get_entity_tags(&container_entity_id, OriginTagCardinality::Low)
            .unwrap();
        assert_eq!(
            container_unified_tags.as_sorted(),
            container_expected_tags.clone().as_sorted()
        );

        // Create a new set of metadata entries to add an additional tag to the pod, and observe that processing the
        // entry updates the resolved tags for both the pod entity as well as the container entity.
        let (mut new_pod_expected_tags, new_pod_operations) =
            low_cardinality!(&pod_entity_id, tags => ["app" => "bar"]);
        container_expected_tags.extend(new_pod_expected_tags.clone());
        new_pod_expected_tags.extend(pod_expected_tags.clone());

        for operation in new_pod_operations {
            store.process_operation(operation);
        }

        let querier = store.querier();

        let new_pod_unified_tags = querier
            .get_entity_tags(&pod_entity_id, OriginTagCardinality::Low)
            .unwrap();
        assert_eq!(new_pod_unified_tags.as_sorted(), new_pod_expected_tags.as_sorted());

        let container_unified_tags = querier
            .get_entity_tags(&container_entity_id, OriginTagCardinality::Low)
            .unwrap();
        assert_eq!(container_unified_tags.as_sorted(), container_expected_tags.as_sorted());
    }

    #[test]
    fn obeys_entity_limit() {
        // We create three entities that we'll try to process with an entity limit of two.
        let pod_entity_id = EntityId::PodUid("datadog-agent-pod-uid".into());
        let (_, pod_operations) = low_cardinality!(&pod_entity_id, tags => ["kube_pod_name" => "datadog-agent-z1ha3"]);

        let container1_entity_id = EntityId::Container("process-agent-container-id".into());
        let (_, container1_operations) = low_cardinality!(&container1_entity_id, tags => ["service" => "foo"]);

        let container2_entity_id = EntityId::Container("trace-agent-container-id".into());
        let (_, container2_operations) = low_cardinality!(&container2_entity_id, tags => ["service" => "foo"]);

        let entity_limit = NonZeroUsize::new(2).unwrap();
        let mut store = TagStore::with_entity_limit(entity_limit);
        for operation in pod_operations {
            store.process_operation(operation);
        }
        for operation in container1_operations {
            store.process_operation(operation);
        }
        for operation in container2_operations {
            store.process_operation(operation);
        }

        // At this point, we should have tags for the pod, and container #1, but not container #2.
        let querier = store.querier();

        assert!(querier
            .get_entity_tags(&pod_entity_id, OriginTagCardinality::Low)
            .is_some());
        assert!(querier
            .get_entity_tags(&container1_entity_id, OriginTagCardinality::Low)
            .is_some());
        assert!(querier
            .get_entity_tags(&container2_entity_id, OriginTagCardinality::Low)
            .is_none());

        // If we delete container #1, and then process the container #2 operations again, we should then have tags for
        // container #2 but not container #1.
        store.process_operation(MetadataOperation::delete(container1_entity_id.clone()));

        let (_, container2_operations) = low_cardinality!(&container2_entity_id, tags => ["service" => "foo"]);
        for operation in container2_operations {
            store.process_operation(operation);
        }

        let querier = store.querier();

        assert!(querier
            .get_entity_tags(&pod_entity_id, OriginTagCardinality::Low)
            .is_some());
        assert!(querier
            .get_entity_tags(&container1_entity_id, OriginTagCardinality::Low)
            .is_none());
        assert!(querier
            .get_entity_tags(&container2_entity_id, OriginTagCardinality::Low)
            .is_some());
    }
}
