//! Workload provider.
//!
//! This modules provides the `WorkloadProvider` trait, which deals with providing information about workloads running on
//! the process host.
//!
//! A number of building blocks are included -- generic entity identifiers, tag storage, metadata collection and
//! aggregation -- along with a default workload provider implementation based on the Datadog Agent.

use saluki_context::{
    origin::{OriginTagCardinality, RawOrigin},
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
    /// Returns `Some(SharedTagSet)` if the entity has tags, or `None` if the entity does not have any tags or if the
    /// entity was not found.
    fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: OriginTagCardinality) -> Option<SharedTagSet>;

    /// Resolves a raw origin.
    ///
    ///  If the origin is empty, `None` is returned. Otherwise, `Some(ResolvedOrigin)` will be returned, which contains
    ///  fully resolved versions of the raw origin components.
    fn get_resolved_origin(&self, origin: RawOrigin<'_>) -> Option<ResolvedOrigin>;
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

    fn get_resolved_origin(&self, origin: RawOrigin<'_>) -> Option<ResolvedOrigin> {
        match self.as_ref() {
            Some(provider) => provider.get_resolved_origin(origin),
            None => None,
        }
    }
}
