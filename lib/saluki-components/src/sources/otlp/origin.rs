use std::sync::Arc;

use saluki_context::{
    origin::{OriginTagCardinality, OriginTagsResolver, RawOrigin},
    tags::SharedTagSet,
};
use saluki_env::WorkloadProvider;
use tracing::trace;

#[derive(Clone)]
pub(super) struct OtlpOriginTagResolver {
    workload_provider: Arc<dyn WorkloadProvider + Send + Sync>,
}

impl OtlpOriginTagResolver {
    pub fn new(workload_provider: Arc<dyn WorkloadProvider + Send + Sync>) -> Self {
        Self { workload_provider }
    }

    fn collect_origin_tags(&self, resolved_origin: &saluki_env::workload::origin::ResolvedOrigin) -> SharedTagSet {
        let mut collected_tags = SharedTagSet::default();
        let tag_cardinality = resolved_origin.cardinality().unwrap_or(OriginTagCardinality::Low);

        // TODO: Add additional entity IDs that are specific to OTLP.
        // https://github.com/DataDog/datadog-agent/blob/main/comp/otelcol/otlp/components/processor/infraattributesprocessor/common.go#L158
        let entity_ids = [
            resolved_origin.container_id(),
            resolved_origin.pod_uid(),
            resolved_origin.process_id(),
        ];

        for entity_id in entity_ids.iter().flatten() {
            if let Some(tags) = self.workload_provider.get_tags_for_entity(entity_id, tag_cardinality) {
                if !tags.is_empty() {
                    collected_tags.extend_from_shared(&tags);
                    return collected_tags;
                }
            } else {
                trace!(
                    ?entity_id,
                    cardinality = tag_cardinality.as_str(),
                    "No tags found for entity."
                );
            }
        }
        collected_tags
    }
}

impl OriginTagsResolver for OtlpOriginTagResolver {
    fn resolve_origin_tags(&self, origin: RawOrigin<'_>) -> SharedTagSet {
        match self.workload_provider.get_resolved_origin(origin.clone()) {
            Some(resolved_origin) => self.collect_origin_tags(&resolved_origin),
            None => {
                trace!(?origin, "No resolved origin found for raw origin.");
                SharedTagSet::default()
            }
        }
    }
}
