use std::sync::Arc;

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_context::{
    origin::{OriginKey, OriginTagCardinality, OriginTagsResolver, RawOrigin},
    tags::TagVisitor,
};
use saluki_core::{
    components::transforms::{SynchronousTransform, SynchronousTransformBuilder},
    topology::interconnect::FixedSizeEventBuffer,
};
use saluki_env::WorkloadProvider;
use saluki_error::GenericError;
use tracing::trace;

/// Origin Remapper transform.
///
/// This synchronous transform remaps the origin of metric events to a fixed origin, which allows uniformly updating
/// the origin, and thus the origin tags, of all metrics passing through the transform.
pub struct OriginRemapperConfiguration {
    workload_provider: Arc<dyn WorkloadProvider + Send + Sync>,
    tag_cardinality: OriginTagCardinality,
    raw_origin: RawOrigin<'static>,
}

impl OriginRemapperConfiguration {
    /// Creates a new `OriginRemapperConfiguration` from the given workload provider and tag cardinality.
    pub fn new<W>(workload_provider: W, tag_cardinality: OriginTagCardinality) -> Self
    where
        W: WorkloadProvider + Send + Sync + 'static,
    {
        let workload_provider = Arc::new(workload_provider);
        Self {
            workload_provider,
            tag_cardinality,
            raw_origin: RawOrigin::default(),
        }
    }

    /// Sets the self process ID as part of the origin.
    pub fn with_self_process_id(mut self) -> Self {
        self.raw_origin.set_process_id(std::process::id());
        self
    }
}

#[async_trait]
impl SynchronousTransformBuilder for OriginRemapperConfiguration {
    async fn build(&self) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        let maybe_origin_key = self.workload_provider.resolve_origin(self.raw_origin.clone());

        let origin_tags_resolver = Arc::new(FixedOriginTagsResolver {
            workload_provider: self.workload_provider.clone(),
            tag_cardinality: self.tag_cardinality,
        });

        Ok(Box::new(OriginRemapper {
            maybe_origin_key,
            origin_tags_resolver,
        }))
    }
}

impl MemoryBounds for OriginRemapperConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built.
            .with_single_value::<OriginRemapper>("component struct");
    }
}

pub struct OriginRemapper {
    maybe_origin_key: Option<OriginKey>,
    origin_tags_resolver: Arc<dyn OriginTagsResolver>,
}

impl SynchronousTransform for OriginRemapper {
    fn transform_buffer(&mut self, event_buffer: &mut FixedSizeEventBuffer) {
        if let Some(origin_key) = self.maybe_origin_key {
            for event in event_buffer {
                if let Some(metric) = event.try_as_metric_mut() {
                    let new_context = metric
                        .context()
                        .with_origin(origin_key, Arc::clone(&self.origin_tags_resolver));
                    *metric.context_mut() = new_context;
                }
            }
        }
    }
}

#[derive(Clone)]
struct FixedOriginTagsResolver {
    workload_provider: Arc<dyn WorkloadProvider + Send + Sync>,
    tag_cardinality: OriginTagCardinality,
}

impl OriginTagsResolver for FixedOriginTagsResolver {
    fn resolve_origin_key(&self, _origin: RawOrigin<'_>) -> Option<OriginKey> {
        unreachable!("origin keys should never be resolved as they're generated at build time")
    }

    fn visit_origin_tags(&self, origin_key: OriginKey, visitor: &mut dyn TagVisitor) {
        // If the cardinality is `None`, then we don't want to visit any tags.
        if self.tag_cardinality == OriginTagCardinality::None {
            trace!("Skipping origin enrichment for metric with cardinality 'none'.");
            return;
        }

        // Try resolving the origin for this key, and if we get one back, try to visit the tags for each entity ID that
        // is present in the origin.
        let origin = match self.workload_provider.get_resolved_origin_by_key(&origin_key) {
            Some(origin) => origin,
            None => {
                trace!("No origin found for key.");
                return;
            }
        };

        let maybe_entity_id = origin.pod_uid();
        let maybe_container_id = origin.container_id();
        let maybe_origin_pid = origin.process_id();
        let maybe_external_data_pod_uid = origin.resolved_external_data().map(|red| red.pod_entity_id());
        let maybe_external_data_container_id = origin.resolved_external_data().map(|red| red.container_entity_id());
        let maybe_entity_ids = &[
            maybe_origin_pid,
            maybe_container_id,
            maybe_entity_id,
            maybe_external_data_pod_uid,
            maybe_external_data_container_id,
        ];
        for entity_id in maybe_entity_ids.iter().flatten() {
            match self
                .workload_provider
                .get_tags_for_entity(entity_id, self.tag_cardinality)
            {
                Some(tags) => {
                    trace!(
                        ?entity_id,
                        tags_len = tags.len(),
                        "Found tags for entity during remapping."
                    );

                    for tag in &tags {
                        visitor.visit_tag(tag);
                    }
                }
                None => trace!(?entity_id, "No tags found for entity during remapping."),
            }
        }
    }
}
