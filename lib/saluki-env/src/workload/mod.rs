//! Workload provider.
//!
//! This modules provides the `WorkloadProvider` trait, which deals with providing information about workloads running on
//! the process host.
//!
//! A number of building blocks are included -- generic entity identifiers, tag storage, metadata collection and
//! aggregation -- along with a default workload provider implementation based on the Datadog Agent.

use saluki_context::{
    origin::{OriginKey, OriginTagCardinality, RawOrigin},
    tags::TagVisitor,
};

mod aggregator;
mod collectors;

mod entity;
pub use self::entity::EntityId;

mod helpers;
mod metadata;
pub use self::metadata::{MetadataAction, MetadataOperation};

#[cfg(target_os = "linux")]
mod on_demand_pid;

pub mod origin;
use self::origin::ResolvedOrigin;

pub mod providers;

mod stores;

/// Provides information about workloads running on the process host.
pub trait WorkloadProvider {
    /// Visits the tags for an entity.
    ///
    /// Entities are workload resources running on the process host, such as containers or pods. The cardinality of the
    /// tags to get can be controlled via `cardinality`.
    ///
    /// All tags found for the entity at the given cardinality will be passed to the `tag_visitor` in an unspecified
    /// order. Tags may or may not be duplicated, so it is the caller's responsibility to handle duplicates.
    ///
    /// Returns `false` if the entity does not exist at all, `true` otherwise.
    fn visit_tags_for_entity(
        &self, entity_id: &EntityId, cardinality: OriginTagCardinality, tag_visitor: &mut dyn TagVisitor,
    ) -> bool;

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
    fn visit_tags_for_entity(
        &self, entity_id: &EntityId, cardinality: OriginTagCardinality, tag_visitor: &mut dyn TagVisitor,
    ) -> bool {
        match self.as_ref() {
            Some(provider) => provider.visit_tags_for_entity(entity_id, cardinality, tag_visitor),
            None => false,
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
