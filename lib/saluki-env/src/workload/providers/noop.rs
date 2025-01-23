use saluki_context::{origin::OriginTagCardinality, tags::SharedTagSet};

use crate::{workload::EntityId, WorkloadProvider};

/// A no-op workload provider that does not provide any workload information.
#[derive(Default)]
pub struct NoopWorkloadProvider;

impl WorkloadProvider for NoopWorkloadProvider {
    fn get_tags_for_entity(&self, _: &EntityId, _: OriginTagCardinality) -> Option<SharedTagSet> {
        None
    }
}
