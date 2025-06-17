use saluki_context::{
    origin::{OriginTagCardinality, RawOrigin},
    tags::SharedTagSet,
};

use crate::{
    workload::{origin::ResolvedOrigin, EntityId},
    WorkloadProvider,
};

/// A no-op workload provider that does not provide any workload information.
#[derive(Default)]
pub struct NoopWorkloadProvider;

impl WorkloadProvider for NoopWorkloadProvider {
    fn get_tags_for_entity(&self, _: &EntityId, _alive_: OriginTagCardinality) -> Option<SharedTagSet> {
        None
    }

    fn get_resolved_origin(&self, _: RawOrigin<'_>) -> Option<ResolvedOrigin> {
        None
    }
}
