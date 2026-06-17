use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_context::{
    origin::{OriginTagCardinality, RawOrigin},
    tags::SharedTagSet,
};
use saluki_env::workload::{origin::ResolvedOrigin, CaptureEntityResolver, EntityId, WorkloadProvider};

/// Health-check name prefix used by workload provider workers.
pub const WORKLOAD_HEALTH_PREFIX: &str = "workload/";

/// Placeholder workload provider for typed attachment wiring.
#[derive(Clone)]
pub struct RemoteAgentWorkloadProvider;

impl WorkloadProvider for RemoteAgentWorkloadProvider {
    fn get_tags_for_entity(&self, _entity_id: &EntityId, _cardinality: OriginTagCardinality) -> Option<SharedTagSet> {
        None
    }

    fn get_resolved_origin(&self, _origin: RawOrigin<'_>) -> Option<ResolvedOrigin> {
        None
    }
}

impl CaptureEntityResolver for RemoteAgentWorkloadProvider {
    fn resolve_container_entity_for_live_pid(&self, _process_id: u32) -> Option<EntityId> {
        None
    }
}

impl MemoryBounds for RemoteAgentWorkloadProvider {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<Self>("component struct");
    }
}
