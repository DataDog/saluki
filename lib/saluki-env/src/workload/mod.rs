//! Workload provider.
//!
//! This modules provides the `WorkloadProvider` trait, which deals with providing information about workloads running on
//! the process host.
//!
//! A number of building blocks are included -- generic entity identifiers, tag storage, metadata collection and
//! aggregation -- along with a default workload provider implementation based on the Datadog Agent.

use async_trait::async_trait;
use saluki_context::{origin::OriginTagCardinality, tags::SharedTagSet};

mod aggregator;
mod collectors;

mod entity;
pub use self::entity::EntityId;

mod external_data;

mod helpers;
mod metadata;
pub use self::metadata::{MetadataAction, MetadataOperation};

mod origin;

pub mod providers;

mod stores;

/// Provides information about workloads running on the process host.
#[async_trait]
pub trait WorkloadProvider {
    /// Gets the tags for an entity.
    ///
    /// Entities are workload resources running on the process host, such as containers or pods. The cardinality of the
    /// tags to get can be controlled via `cardinality`.
    ///
    /// If no tags can be found for the entity, or at the given cardinality, `None` is returned.
    fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: OriginTagCardinality) -> Option<SharedTagSet>;
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
}
