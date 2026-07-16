//! A configurable, in-memory workload provider for tests.
//!
//! [`NoopWorkloadProvider`][super::NoopWorkloadProvider] can't be seeded with per-entity tags, so downstream test
//! suites (notably the DogStatsD source's origin and replay tests) previously hand-rolled their own
//! [`WorkloadProvider`] mocks. This provider replaces those copies with a single configurable implementation, exposed
//! behind the `test-util` feature so other crates can pull it in as a dev-dependency.

use std::collections::HashMap;

use saluki_context::{
    origin::{OriginTagCardinality, RawOrigin},
    tags::{SharedTagSet, Tag, TagSet},
};

use crate::{
    workload::{origin::ResolvedOrigin, EntityId},
    WorkloadProvider,
};

/// Per-entity tags, split by cardinality level.
///
/// Tags are stored non-cumulatively -- each level holds only the tags that belong to *that* level -- but looked up
/// cumulatively, mirroring the live workload provider: `Low` returns `low`, `Orchestrator` returns
/// `low + orchestrator`, and `High` returns `low + orchestrator + high`.
#[derive(Default)]
struct EntityTags {
    low: SharedTagSet,
    orchestrator: SharedTagSet,
    high: SharedTagSet,
}

impl EntityTags {
    fn for_cardinality(&self, cardinality: OriginTagCardinality) -> Option<SharedTagSet> {
        let levels: &[&SharedTagSet] = match cardinality {
            OriginTagCardinality::None => return None,
            OriginTagCardinality::Low => &[&self.low],
            OriginTagCardinality::Orchestrator => &[&self.low, &self.orchestrator],
            OriginTagCardinality::High => &[&self.low, &self.orchestrator, &self.high],
        };

        let mut merged = SharedTagSet::default();
        for level in levels {
            merged.extend_from_shared(level);
        }
        (!merged.is_empty()).then_some(merged)
    }
}

/// A configurable, in-memory [`WorkloadProvider`] for use in tests.
///
/// Entities are seeded with per-cardinality tags and looked up cumulatively, matching the behavior of the live
/// workload provider. [`get_resolved_origin`][WorkloadProvider::get_resolved_origin] resolves the raw origin's process
/// ID, Local Data, and pod UID into entity IDs using the same rules as the production resolver; External Data is not
/// modeled (it requires the live external-data store) and always resolves to `None`.
#[derive(Default)]
pub struct TestWorkloadProvider {
    entities: HashMap<EntityId, EntityTags>,
}

impl TestWorkloadProvider {
    /// Creates an empty provider.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a provider with a single entity carrying the given low-cardinality tags.
    pub fn with_entity(entity_id: EntityId, low_tags: &[&str]) -> Self {
        let mut provider = Self::new();
        provider.add_entity(entity_id, low_tags);
        provider
    }

    /// Creates a provider with a single entity carrying the given per-cardinality tags.
    pub fn with_entity_cardinalities(entity_id: EntityId, low: &[&str], orchestrator: &[&str], high: &[&str]) -> Self {
        let mut provider = Self::new();
        provider.add_entity_cardinalities(entity_id, low, orchestrator, high);
        provider
    }

    /// Adds an entity carrying the given low-cardinality tags.
    pub fn add_entity(&mut self, entity_id: EntityId, low_tags: &[&str]) -> &mut Self {
        self.add_entity_cardinalities(entity_id, low_tags, &[], &[])
    }

    /// Adds an entity carrying the given per-cardinality tags.
    pub fn add_entity_cardinalities(
        &mut self, entity_id: EntityId, low: &[&str], orchestrator: &[&str], high: &[&str],
    ) -> &mut Self {
        self.entities.insert(
            entity_id,
            EntityTags {
                low: shared_tags(low),
                orchestrator: shared_tags(orchestrator),
                high: shared_tags(high),
            },
        );
        self
    }

    /// Adds an entity carrying the given prebuilt low-cardinality tag set.
    pub fn add_entity_shared_tags(&mut self, entity_id: EntityId, low_tags: SharedTagSet) -> &mut Self {
        self.entities.insert(
            entity_id,
            EntityTags {
                low: low_tags,
                ..Default::default()
            },
        );
        self
    }
}

impl WorkloadProvider for TestWorkloadProvider {
    fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: OriginTagCardinality) -> Option<SharedTagSet> {
        self.entities
            .get(entity_id)
            .and_then(|tags| tags.for_cardinality(cardinality))
    }

    fn get_resolved_origin(&self, origin: RawOrigin<'_>) -> Option<ResolvedOrigin> {
        // Mirror the production resolver: an empty origin resolves to nothing, and each populated component maps to its
        // corresponding entity ID. External Data resolution needs the live external-data store, so it's left unset.
        if origin.is_empty() {
            return None;
        }

        Some(ResolvedOrigin::from_parts(
            origin.cardinality(),
            origin.process_id().map(EntityId::ContainerPid),
            origin.local_data().and_then(EntityId::from_local_data),
            origin.pod_uid().and_then(EntityId::from_pod_uid),
            None,
        ))
    }
}

fn shared_tags(tags: &[&str]) -> SharedTagSet {
    TagSet::from_iter(tags.iter().copied().map(Tag::from)).into_shared()
}
