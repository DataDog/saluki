//! Workload provider.
//!
//! This modules provides the `WorkloadProvider` trait, which deals with providing information about workloads running on
//! the process host.
//!
//! A number of building blocks are included -- generic entity identifiers, tag storage, metadata collection and
//! aggregation -- along with a default workload provider implementation based on the Datadog Agent.

use saluki_context::{
    origin::{OriginKey, OriginTagCardinality, RawOrigin},
    tags::SharedTagSet,
};

mod aggregator;
mod collectors;

mod entity;
pub use self::entity::EntityId;

mod helpers;
mod metadata;
pub use self::metadata::{MetadataAction, MetadataOperation};

mod on_demand_pid;

pub mod origin;
use self::origin::ResolvedOrigin;

pub mod providers;

mod stores;

/// Provides information about workloads running on the process host.
pub trait WorkloadProvider {
    /// Gets the tags for an entity.
    ///
    /// Entities are workload resources running on the process host, such as containers or pods. The cardinality of the
    /// tags to get can be controlled via `cardinality`.
    ///
    /// If no tags can be found for the entity, or at the given cardinality, `None` is returned.
    fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: OriginTagCardinality) -> Option<SharedTagSet>;

    /// Resolves an origin, mapping it to a unique, opaque key.
    ///
    /// If the origin is empty, `None` is returned. Otherwise, `Some(OriginKey)` will be returned, which is a unique,
    /// but opaque, key that can be used to retrieve the origin at a later time. The returned key will always be equal
    /// to the key returned for the same origin in the same process.
    fn resolve_origin(&self, origin: RawOrigin<'_>) -> Option<OriginKey>;

    /// Gets a resolved origin by its key.
    fn get_resolved_origin_by_key(&self, origin_key: &OriginKey) -> Option<ResolvedOrigin>;
}

impl<T> WorkloadProvider for Option<T>
where
    T: WorkloadProvider,
{
    fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: OriginTagCardinality) -> Option<SharedTagSet> {
        match self.as_ref() {
            Some(provider) => provider.get_tags_for_entity(entity_id, cardinality),
            None => None,
        }
    }

    fn resolve_origin(&self, origin: RawOrigin<'_>) -> Option<OriginKey> {
        match self.as_ref() {
            Some(provider) => provider.resolve_origin(origin),
            None => None,
        }
    }

    fn get_resolved_origin_by_key(&self, origin_key: &OriginKey) -> Option<ResolvedOrigin> {
        self.as_ref()
            .and_then(|provider| provider.get_resolved_origin_by_key(origin_key))
    }
}
