use std::collections::{HashMap, VecDeque};

use saluki_event::metric::MetricTags;
use tracing::debug;

use super::{
    entity::EntityId,
    metadata::{MetadataAction, MetadataOperation, TagCardinality},
};

// TODO: This will be very slow if we deliver metadata operations one-by-one, especially when collectors will likely be
// generating many of these operations in a single go, and we could potentially defer incremental resolving until after
// processing a whole batch of operations.
//
// We at least will likely want to support taking a batch of operations, and maybe move the incremental resolving logic
// to `process_operationss` (theoretical name for method) so that we can give it a list of entity IDs to resolve, based
// on the operations we processed, and then adjust the resolving code to start from a list of IDs instead of a single
// one.

#[derive(Default)]
pub struct TagStore {
    entity_ancestor_mappings: HashMap<EntityId, EntityId>,

    low_cardinality_entity_tags: HashMap<EntityId, HashMap<String, String>>,
    high_cardinality_entity_tags: HashMap<EntityId, HashMap<String, String>>,

    resolved_low_cardinality_entity_tags: HashMap<EntityId, MetricTags>,
    resolved_high_cardinality_entity_tags: HashMap<EntityId, MetricTags>,
}

impl TagStore {
    pub fn process_operation(&mut self, operation: MetadataOperation) {
        debug!(?operation, "Processing metadata operation.");

        let entity_id = operation.entity_id;
        for action in operation.actions {
            match action {
                MetadataAction::Delete => self.delete_entity(entity_id.clone()),
                MetadataAction::LinkAncestor { ancestor_entity_id } => {
                    self.add_ancestor_mapping(entity_id.clone(), ancestor_entity_id)
                }
                MetadataAction::AddTag {
                    cardinality,
                    key,
                    value,
                } => self.add_entity_tags(entity_id.clone(), vec![(key, value)], cardinality),
                MetadataAction::AddTags { cardinality, tags } => {
                    self.add_entity_tags(entity_id.clone(), tags, cardinality)
                }
            }
        }
    }

    fn delete_entity(&mut self, entity_id: EntityId) {
        // Delete all of the tags for the entity, both raw and resolved.
        self.low_cardinality_entity_tags.remove(&entity_id);
        self.high_cardinality_entity_tags.remove(&entity_id);
        self.resolved_low_cardinality_entity_tags.remove(&entity_id);
        self.resolved_high_cardinality_entity_tags.remove(&entity_id);

        // Delete the ancestry mapping, if it exists, for the entity itself.
        self.entity_ancestor_mappings.remove(&entity_id);

        // Iterate over all ancestry mappings, and delete any which reference this entity as an ancestor. We'll keep a
        // list of the entity IDs which reference this entity as an ancestor, as we'll need to regenerate their tags
        // after breaking the ancestry link.
        let mut entity_ids_to_resolve = Vec::new();
        for (descendant_entity_id, ancestor_entity_id) in self.entity_ancestor_mappings.iter() {
            if ancestor_entity_id == &entity_id {
                entity_ids_to_resolve.push(descendant_entity_id.clone());
            }
        }

        for entity_id in entity_ids_to_resolve {
            self.entity_ancestor_mappings.remove(&entity_id);
            self.regenerate_entity_tags(entity_id);
        }
    }

    fn add_ancestor_mapping(&mut self, entity_id: EntityId, ancestor_entity_id: EntityId) {
        let _ = self
            .entity_ancestor_mappings
            .insert(entity_id.clone(), ancestor_entity_id);
        self.regenerate_entity_tags(entity_id);
    }

    fn add_entity_tags(&mut self, entity_id: EntityId, tags: Vec<(String, String)>, cardinality: TagCardinality) {
        let existing_tags = match cardinality {
            TagCardinality::Low => self.low_cardinality_entity_tags.entry(entity_id.clone()).or_default(),
            TagCardinality::High => self.high_cardinality_entity_tags.entry(entity_id.clone()).or_default(),
        };
        existing_tags.extend(tags.into_iter().map(Into::into));

        self.regenerate_entity_tags(entity_id);
    }

    fn regenerate_entity_tags(&mut self, entity_id: EntityId) {
        // We want to incrementally resolve the tags for the given entity, which is directional: we have to regenerate
        // the tags for both the entity itself _and_ any descendants of the entity, based on our ancestry mapping.
        //
        // We'll regenerate tags for the entity itself first, and then for any descendants.
        let new_low_cardinality_entity_tags = self.resolve_entity_tags(&entity_id, TagCardinality::Low);
        let new_high_cardinality_entity_tags = self.resolve_entity_tags(&entity_id, TagCardinality::High);

        match self.resolved_low_cardinality_entity_tags.get_mut(&entity_id) {
            Some(existing_tags) => *existing_tags = new_low_cardinality_entity_tags,
            None => {
                self.resolved_low_cardinality_entity_tags
                    .insert(entity_id.clone(), new_low_cardinality_entity_tags);
            }
        }

        match self.resolved_high_cardinality_entity_tags.get_mut(&entity_id) {
            Some(existing_tags) => *existing_tags = new_high_cardinality_entity_tags,
            None => {
                self.resolved_high_cardinality_entity_tags
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
        for (descendant_entity_id, ancestor_entity_id) in self.entity_ancestor_mappings.iter() {
            if ancestor_entity_id == &entity_id {
                entity_stack.push_back(descendant_entity_id.clone());
            }
        }

        while let Some(sub_entity_id) = entity_stack.pop_front() {
            let new_low_cardinality_entity_tags = self.resolve_entity_tags(&sub_entity_id, TagCardinality::Low);
            let new_high_cardinality_entity_tags = self.resolve_entity_tags(&sub_entity_id, TagCardinality::High);

            match self.resolved_low_cardinality_entity_tags.get_mut(&sub_entity_id) {
                Some(existing_tags) => *existing_tags = new_low_cardinality_entity_tags,
                None => {
                    self.resolved_low_cardinality_entity_tags
                        .insert(sub_entity_id.clone(), new_low_cardinality_entity_tags);
                }
            }

            match self.resolved_high_cardinality_entity_tags.get_mut(&sub_entity_id) {
                Some(existing_tags) => *existing_tags = new_high_cardinality_entity_tags,
                None => {
                    self.resolved_high_cardinality_entity_tags
                        .insert(sub_entity_id.clone(), new_high_cardinality_entity_tags);
                }
            }

            // Add any descendants of the current entity to the stack.
            for (descendant_entity_id, ancestor_entity_id) in self.entity_ancestor_mappings.iter() {
                if ancestor_entity_id == &sub_entity_id {
                    entity_stack.push_back(descendant_entity_id.clone());
                }
            }
        }
    }

    fn get_raw_entity_tags(
        &self, entity_id: &EntityId, cardinality: TagCardinality,
    ) -> Option<HashMap<String, String>> {
        match cardinality {
            TagCardinality::Low => self.low_cardinality_entity_tags.get(entity_id).cloned(),
            TagCardinality::High => {
                // In high cardinality mode, we merge together both the low and high cardinality tags, so that "high
                // cardinality" ends up as a superset instead of being disjoint.
                self.low_cardinality_entity_tags
                    .get(entity_id)
                    .cloned()
                    .map(|mut tags| {
                        if let Some(high_cardinality_tags) = self.high_cardinality_entity_tags.get(entity_id) {
                            tags.extend(high_cardinality_tags.clone());
                        }
                        tags
                    })
                    .or_else(|| self.high_cardinality_entity_tags.get(entity_id).cloned())
            }
        }
    }

    fn resolve_entity_tags(&self, entity_id: &EntityId, cardinality: TagCardinality) -> MetricTags {
        // Start with any global tags, which are always used if present.
        let mut resolved_tags = self
            .get_raw_entity_tags(&EntityId::Global, cardinality)
            .unwrap_or_default();

        // Build the ancestry chain for the entity, starting with the entity itself.
        let mut entity_chain = VecDeque::new();
        entity_chain.push_back(entity_id);

        loop {
            let current = entity_chain.front().expect("entity chain can never be empty");
            if let Some(ancestor) = self.entity_ancestor_mappings.get(*current) {
                entity_chain.push_front(ancestor);
            } else {
                break;
            }
        }

        // For each entity in the chain, grab their tags, and merge them into the resolved tags.
        for ancestor_entity_id in entity_chain {
            if let Some(tags) = self.get_raw_entity_tags(ancestor_entity_id, cardinality) {
                for (tag_key, tag_value) in tags {
                    match resolved_tags.get_mut(&tag_key) {
                        Some(existing_value) => {
                            // If the tag already exists, we need to merge the values.
                            *existing_value = tag_value;
                        }
                        None => {
                            // Otherwise, we can just insert the tag.
                            resolved_tags.insert(tag_key, tag_value);
                        }
                    }
                }
            }
        }

        let mut metric_tags = MetricTags::default();
        for (tag_key, tag_value) in resolved_tags {
            metric_tags.insert_tag((tag_key, tag_value));
        }

        metric_tags
    }

    pub fn snapshot(&self) -> TagSnapshot {
        TagSnapshot {
            low_cardinality_entity_tags: self.resolved_low_cardinality_entity_tags.clone(),
            high_cardinality_entity_tags: self.resolved_high_cardinality_entity_tags.clone(),
        }
    }
}

#[derive(Debug, Default)]
pub struct TagSnapshot {
    low_cardinality_entity_tags: HashMap<EntityId, MetricTags>,
    high_cardinality_entity_tags: HashMap<EntityId, MetricTags>,
}

impl TagSnapshot {
    pub fn get_entity_tags(&self, entity_id: &EntityId, cardinality: TagCardinality) -> Option<MetricTags> {
        match cardinality {
            TagCardinality::Low => self.low_cardinality_entity_tags.get(entity_id).cloned(),
            TagCardinality::High => self.high_cardinality_entity_tags.get(entity_id).cloned(),
        }
    }
}

// NOTE: All of the unit tests that deal with merging the "expected tags" by using `Extend` are designed/ordered to
// avoid creating tags with multiple values, since the logic for `MetricTags` doesn't replace existing tags, but simply
// aggregates the values of existing tags.
#[cfg(test)]
mod tests {
    use saluki_event::metric::MetricTags;

    use crate::workload::{
        entity::EntityId,
        helpers::OneOrMany,
        metadata::{MetadataAction, MetadataOperation, TagCardinality},
        store::TagStore,
    };

    fn link_ancestor(child: &EntityId, ancestor: &EntityId) -> MetadataOperation {
        MetadataOperation::link_ancestor(child.clone(), ancestor.clone())
    }

    macro_rules! low_cardinality {
		($entity_id:expr, tags => [$($key:literal => $value:literal),+]) => {{
			tag_values!($entity_id, TagCardinality::Low, tags => [$($key => $value,)+])
		}};
	}

    macro_rules! high_cardinality {
		($entity_id:expr, tags => [$($key:literal => $value:literal),+]) => {{
			tag_values!($entity_id, TagCardinality::High, tags => [$($key => $value,)+])
		}};
	}

    macro_rules! tag_values {
		($entity_id:expr, $cardinality:expr, tags => [$($key:literal => $value:literal),+ $(,)?]) => {{
			let mut expected_tags = MetricTags::default();
			let mut operations = Vec::new();

			$(
				let tag = format!("{}:{}", $key, $value);
				expected_tags.insert_tag(tag);

				operations.push(MetadataOperation {
					entity_id: $entity_id.clone(),
					actions: OneOrMany::One(MetadataAction::AddTag {
						cardinality: $cardinality,
						key: $key.into(),
						value: $value.into(),
					}),
				});
			)+

			(expected_tags, operations)
		}};
	}

    #[test]
    fn basic_entity() {
        let entity_id = EntityId::Container("container-id".to_string());
        let (expected_tags, operations) = low_cardinality!(&entity_id, tags => ["service" => "foo"]);

        let mut store = TagStore::default();
        for operation in operations {
            store.process_operation(operation);
        }

        let snapshot = store.snapshot();

        let resolved_tags = snapshot.get_entity_tags(&entity_id, TagCardinality::Low).unwrap();
        assert_eq!(resolved_tags, expected_tags);
    }

    #[test]
    fn high_cardinality_is_superset() {
        let entity_id = EntityId::Container("container-id".to_string());
        let (low_card_expected_tags, low_card_operations) = low_cardinality!(&entity_id, tags => ["service" => "foo"]);
        let (mut high_card_expected_tags, high_card_operations) =
            high_cardinality!(&entity_id, tags => ["pod" => "foo-8xl-ah2z7"]);

        // Make sure to make our expected high cardinality tags a superset.
        high_card_expected_tags.extend(low_card_expected_tags.clone());

        let mut store = TagStore::default();
        for operation in low_card_operations {
            store.process_operation(operation);
        }
        for operation in high_card_operations {
            store.process_operation(operation);
        }

        let snapshot = store.snapshot();

        let low_card_resolved_tags = snapshot.get_entity_tags(&entity_id, TagCardinality::Low).unwrap();
        assert_eq!(low_card_resolved_tags.sorted(), low_card_expected_tags.sorted());

        let high_card_resolved_tags = snapshot.get_entity_tags(&entity_id, TagCardinality::High).unwrap();
        assert_eq!(high_card_resolved_tags.sorted(), high_card_expected_tags.sorted());
    }

    #[test]
    fn global_tags() {
        let global_entity_id = EntityId::Global;
        let (global_expected_tags, global_operations) =
            low_cardinality!(&global_entity_id, tags => ["kube_cluster_name" => "saluki"]);

        let entity_id = EntityId::Container("container-id".to_string());
        let (mut expected_tags, operations) = low_cardinality!(&entity_id, tags => ["service" => "foo"]);

        expected_tags.extend(global_expected_tags.clone());

        let mut store = TagStore::default();
        for operation in global_operations {
            store.process_operation(operation);
        }
        for operation in operations {
            store.process_operation(operation);
        }

        let snapshot = store.snapshot();

        let global_resolved_tags = snapshot
            .get_entity_tags(&global_entity_id, TagCardinality::Low)
            .unwrap();
        assert_eq!(global_resolved_tags.sorted(), global_expected_tags.sorted());

        let resolved_tags = snapshot.get_entity_tags(&entity_id, TagCardinality::High).unwrap();
        assert_eq!(resolved_tags.sorted(), expected_tags.sorted());
    }

    #[test]
    fn ancestors() {
        // We establish a three-level hierarchy -- pod -> container ID -> container PID -- which is indeed not a
        // real-world thing but we're just doing it to have more than one level of ancestry, to better exercise the
        // resolution logic.
        let pod_entity_id = EntityId::PodUid("datadog-agent-pod-uid".to_string());
        let (pod_expected_tags, pod_operations) =
            low_cardinality!(&pod_entity_id, tags => ["kube_pod_name" => "datadog-agent-z1ha3"]);

        let container_entity_id = EntityId::Container("process-agent-container-id".to_string());
        let (mut container_expected_tags, container_operations) =
            low_cardinality!(&container_entity_id, tags => ["service" => "foo"]);

        container_expected_tags.extend(pod_expected_tags.clone());

        let container_pid_entity_id = EntityId::ContainerPid(422);
        let (mut container_pid_expected_tags, container_pid_operations) =
            low_cardinality!(&container_pid_entity_id, tags => ["pid" => "422"]);

        container_pid_expected_tags.extend(container_expected_tags.clone());

        let mut store = TagStore::default();
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

        let snapshot = store.snapshot();

        let pod_resolved_tags = snapshot.get_entity_tags(&pod_entity_id, TagCardinality::Low).unwrap();
        assert_eq!(pod_resolved_tags.sorted(), pod_expected_tags.sorted());

        let container_resolved_tags = snapshot
            .get_entity_tags(&container_entity_id, TagCardinality::Low)
            .unwrap();
        assert_eq!(container_resolved_tags.sorted(), container_expected_tags.sorted());

        let container_pid_resolved_tags = snapshot
            .get_entity_tags(&container_pid_entity_id, TagCardinality::Low)
            .unwrap();
        assert_eq!(
            container_pid_resolved_tags.sorted(),
            container_pid_expected_tags.sorted()
        );
    }

    #[test]
    fn direct_resolve() {
        let entity_id = EntityId::Container("container-id".to_string());
        let (expected_tags, operations) = low_cardinality!(&entity_id, tags => ["service" => "foo"]);

        let mut store = TagStore::default();
        for operation in operations {
            store.process_operation(operation);
        }

        let snapshot = store.snapshot();

        let resolved_tags = snapshot.get_entity_tags(&entity_id, TagCardinality::Low).unwrap();
        assert_eq!(resolved_tags.sorted(), expected_tags.clone().sorted());

        // Create a new set of metadata entries to add an additional tag, and observe that processing the entry updates
        // the resolved tags for our entity.
        let (mut new_expected_tags, new_operations) = low_cardinality!(&entity_id, tags => ["app" => "bar"]);
        new_expected_tags.extend(expected_tags.clone());

        for operation in new_operations {
            store.process_operation(operation);
        }

        let snapshot = store.snapshot();

        let new_resolved_tags = snapshot.get_entity_tags(&entity_id, TagCardinality::Low).unwrap();
        assert_eq!(new_resolved_tags.sorted(), new_expected_tags.sorted());
    }

    #[test]
    fn ancestor_resolve() {
        let pod_entity_id = EntityId::PodUid("datadog-agent-pod-uid".to_string());
        let (pod_expected_tags, pod_operations) =
            low_cardinality!(&pod_entity_id, tags => ["kube_pod_name" => "datadog-agent-z1ha3"]);

        let container_entity_id = EntityId::Container("process-agent-container-id".to_string());
        let (mut container_expected_tags, container_operations) =
            low_cardinality!(&container_entity_id, tags => ["service" => "foo"]);

        container_expected_tags.extend(pod_expected_tags.clone());

        let mut store = TagStore::default();
        for operation in pod_operations {
            store.process_operation(operation);
        }
        for operation in container_operations {
            store.process_operation(operation);
        }

        store.process_operation(link_ancestor(&container_entity_id, &pod_entity_id));

        let snapshot = store.snapshot();

        let pod_resolved_tags = snapshot.get_entity_tags(&pod_entity_id, TagCardinality::Low).unwrap();
        assert_eq!(pod_resolved_tags.sorted(), pod_expected_tags.clone().sorted());

        let container_resolved_tags = snapshot
            .get_entity_tags(&container_entity_id, TagCardinality::Low)
            .unwrap();
        assert_eq!(
            container_resolved_tags.sorted(),
            container_expected_tags.clone().sorted()
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

        let snapshot = store.snapshot();

        let new_pod_resolved_tags = snapshot.get_entity_tags(&pod_entity_id, TagCardinality::Low).unwrap();
        assert_eq!(new_pod_resolved_tags.sorted(), new_pod_expected_tags.sorted());

        let container_resolved_tags = snapshot
            .get_entity_tags(&container_entity_id, TagCardinality::Low)
            .unwrap();
        assert_eq!(container_resolved_tags.sorted(), container_expected_tags.sorted());
    }
}
