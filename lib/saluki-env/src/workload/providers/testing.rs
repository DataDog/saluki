//! A configurable, in-memory workload provider for tests.
//!
//! [`NoopWorkloadProvider`][super::NoopWorkloadProvider] can't be seeded with per-entity tags, so downstream test
//! suites (notably the DogStatsD source's origin and replay tests) previously hand-rolled their own
//! [`WorkloadProvider`] mocks. This provider replaces those copies with a single configurable implementation, exposed
//! behind the `test-util` feature so other crates can pull it in as a dev-dependency.

use std::collections::HashMap;

use saluki_common::collections::FastIndexMap;
use saluki_core::data_model::{
    origin::{ExternalData, OriginTagCardinality, RawOrigin},
    tags::{SharedTagSet, Tag, TagSet},
};
use stringtheory::MetaString;

use crate::{
    workload::{
        origin::{ResolvedExternalData, ResolvedOrigin},
        CaptureEntityResolver, EntityId,
    },
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
/// workload provider. Entity aliases work as in the live tag store: a tag lookup for an aliased entity reads the tags of
/// its target, and an alias from a cgroup inode to a container resolves through
/// [`get_container_entity_for_inode`][WorkloadProvider::get_container_entity_for_inode].
///
/// [`get_resolved_origin`][WorkloadProvider::get_resolved_origin] resolves the raw origin's process ID, Local Data, and
/// pod UID into entity IDs with the rules of the production resolver. It resolves External Data from the entries that
/// [`add_external_data`][Self::add_external_data] adds.
///
/// The provider also implements [`CaptureEntityResolver`], for the process IDs that
/// [`add_live_process`][Self::add_live_process] adds.
#[derive(Default)]
pub struct TestWorkloadProvider {
    entities: HashMap<EntityId, EntityTags>,
    aliases: HashMap<EntityId, EntityId>,
    external_data: FastIndexMap<ExternalData, ResolvedExternalData>,
    live_processes: HashMap<u32, EntityId>,
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

    /// Adds an alias from `source` to `target`.
    pub fn add_entity_alias(&mut self, source: EntityId, target: EntityId) -> &mut Self {
        self.aliases.insert(source, target);
        self
    }

    /// Adds an External Data entry that resolves to the container `container_entity_id`.
    pub fn add_external_data(
        &mut self, pod_uid: &str, container_name: &str, init_container: bool, container_entity_id: EntityId,
    ) -> &mut Self {
        let external_data = ExternalData::new(
            MetaString::from(pod_uid),
            MetaString::from(container_name),
            init_container,
        );
        let resolved = ResolvedExternalData::new(EntityId::PodUid(pod_uid.into()), container_entity_id);
        self.external_data.insert(external_data, resolved);
        self
    }

    /// Adds a live process that runs in the container `container_entity_id`.
    pub fn add_live_process(&mut self, process_id: u32, container_entity_id: EntityId) -> &mut Self {
        self.live_processes.insert(process_id, container_entity_id);
        self
    }
}

impl WorkloadProvider for TestWorkloadProvider {
    fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: OriginTagCardinality) -> Option<SharedTagSet> {
        let entity_id = self.aliases.get(entity_id).unwrap_or(entity_id);
        self.entities
            .get(entity_id)
            .and_then(|tags| tags.for_cardinality(cardinality))
    }

    fn get_resolved_origin(&self, origin: RawOrigin<'_>) -> Option<ResolvedOrigin> {
        // Mirror the production resolver: an empty origin resolves to nothing, and each populated component maps to its
        // corresponding entity ID.
        if origin.is_empty() {
            return None;
        }

        Some(ResolvedOrigin::from_parts(
            origin.cardinality(),
            origin.process_id().map(EntityId::ContainerPid),
            origin.local_data().and_then(EntityId::from_local_data),
            origin.pod_uid().and_then(EntityId::from_pod_uid),
            origin
                .external_data()
                .and_then(|raw| self.external_data.get(raw).cloned()),
        ))
    }

    fn get_container_entity_for_inode(&self, inode: u64) -> Option<EntityId> {
        self.aliases
            .get(&EntityId::ContainerInode(inode))
            .filter(|entity_id| matches!(entity_id, EntityId::Container(_)))
            .cloned()
    }
}

impl CaptureEntityResolver for TestWorkloadProvider {
    fn resolve_container_entity_for_live_pid(&self, process_id: u32) -> Option<EntityId> {
        self.live_processes.get(&process_id).cloned()
    }
}

fn shared_tags(tags: &[&str]) -> SharedTagSet {
    TagSet::from_iter(tags.iter().copied().map(Tag::from)).into_shared()
}
