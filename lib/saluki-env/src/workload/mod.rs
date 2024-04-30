mod aggregator;
mod collectors;
pub mod entity;
mod helpers;
pub mod metadata;
pub mod providers;
pub mod store;
mod tags;

use async_trait::async_trait;
use saluki_event::metric::MetricTags;

use self::{entity::EntityId, metadata::TagCardinality};

#[async_trait]
pub trait WorkloadProvider {
    type Error;

    fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: TagCardinality) -> Option<MetricTags>;
}
