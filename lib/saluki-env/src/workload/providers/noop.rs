use saluki_context::{origin::OriginTagCardinality, tags::SharedTagSet};

use crate::{workload::EntityId, WorkloadProvider};

/// A no-op workload provider that does not provide any workload information.
#[derive(Default)]
pub struct NoopWorkloadProvider;

impl WorkloadProvider for NoopWorkloadProvider {
    fn get_tags_for_entity(&self, _entity_id: &EntityId, _cardinality: OriginTagCardinality) -> Option<SharedTagSet> {
        None
    }

    fn resolve_entity_id_from_external_data(&self, _external_data: &str) -> Option<EntityId> {
        None
    }
}
