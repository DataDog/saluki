use saluki_context::{origin::OriginTagCardinality, tags::SharedTagSet};

use crate::{
    workload::{EntityId, ResolvedExternalData},
    WorkloadProvider,
};

/// A no-op workload provider that does not provide any workload information.
#[derive(Default)]
pub struct NoopWorkloadProvider;

impl WorkloadProvider for NoopWorkloadProvider {
    fn get_tags_for_entity(&self, _: &EntityId, _alive_: OriginTagCardinality) -> Option<SharedTagSet> {
        None
    }

    fn resolve_external_data(&self, _: &str) -> Option<ResolvedExternalData> {
        None
    }
}
