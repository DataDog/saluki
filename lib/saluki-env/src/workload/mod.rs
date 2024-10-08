//! Workload provider.
//!
//! This modules provides the `WorkloadProvider` trait, which dels with providing information about workloads running on
//! the process host.
//!
//! A number of building blocks are included -- generic entity identifiers, tag storage, metadata collection and
//! aggregation -- along with a default workload provider implementation based on the Datadog Agent.
mod aggregator;
mod collectors;

mod entity;
pub use self::entity::EntityId;

mod helpers;

mod metadata;
pub use self::metadata::{MetadataAction, MetadataOperation};

pub mod providers;

mod store;
use async_trait::async_trait;
use saluki_context::TagSet;
use saluki_event::metric::OriginTagCardinality;

pub use self::store::{TagSnapshot, TagStore};

/// Provides information about workloads running on the process host.
#[async_trait]
pub trait WorkloadProvider {
    /// Gets the tags for an entity.
    ///
    /// Entities are workload resources running on the process host, such as containers or pods. The cardinality of the
    /// tags to get can be controlled via `cardinality`.
    ///
    /// If no tags can be found for the entity, or at the given cardinality, `None` is returned.
    fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: OriginTagCardinality) -> Option<TagSet>;
}

impl<T> WorkloadProvider for Option<T>
where
    T: WorkloadProvider,
{
    fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: OriginTagCardinality) -> Option<TagSet> {
        match self.as_ref() {
            Some(provider) => provider.get_tags_for_entity(entity_id, cardinality),
            None => None,
        }
    }
}
