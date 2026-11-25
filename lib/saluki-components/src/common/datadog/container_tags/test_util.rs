//! A workload provider whose container tags change during a test.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
};

use saluki_core::data_model::{
    origin::{OriginTagCardinality, RawOrigin},
    tags::{SharedTagSet, Tag, TagSet},
};
use saluki_env::{
    workload::{origin::ResolvedOrigin, EntityId},
    WorkloadProvider,
};
use stringtheory::MetaString;

/// A provider that knows the containers a test sets, and counts its lookups.
#[derive(Default)]
pub(crate) struct ScriptedProvider {
    containers: Mutex<HashMap<MetaString, SharedTagSet>>,
    lookups: AtomicUsize,
    last_cardinality: Mutex<Option<OriginTagCardinality>>,
}

impl ScriptedProvider {
    /// Sets the tags of a container, which makes the provider know the container.
    pub fn set_tags(&self, container_id: &str, tags: &[&str]) {
        let tags = TagSet::from_iter(tags.iter().copied().map(Tag::from)).into_shared();
        self.containers
            .lock()
            .unwrap()
            .insert(MetaString::from(container_id), tags);
    }

    /// Returns the cardinality of the most recent lookup.
    pub fn last_cardinality(&self) -> Option<OriginTagCardinality> {
        *self.last_cardinality.lock().unwrap()
    }

    /// Returns how many times a consumer looked a container up.
    pub fn lookups(&self) -> usize {
        self.lookups.load(Ordering::Relaxed)
    }
}

impl WorkloadProvider for ScriptedProvider {
    fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: OriginTagCardinality) -> Option<SharedTagSet> {
        self.lookups.fetch_add(1, Ordering::Relaxed);
        *self.last_cardinality.lock().unwrap() = Some(cardinality);
        match entity_id {
            EntityId::Container(id) => self.containers.lock().unwrap().get(id).cloned(),
            _ => None,
        }
    }

    fn get_resolved_origin(&self, _: RawOrigin<'_>) -> Option<ResolvedOrigin> {
        None
    }
}
