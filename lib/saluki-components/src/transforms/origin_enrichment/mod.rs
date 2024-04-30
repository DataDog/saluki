use async_trait::async_trait;

use saluki_core::{
    components::transforms::*,
    constants::{datadog::*, internal::*},
    topology::interconnect::EventBuffer,
};
use saluki_env::{
    workload::{entity::EntityId, metadata::TagCardinality},
    EnvironmentProvider, WorkloadProvider,
};
use saluki_event::{
    metric::{Metric, MetricOrigin},
    Event,
};
use tracing::trace;

/// Origin Enrichment synchronous transform.
///
/// Enriches metrics with tags based on the client origin of a metric. The "origin" referred to here is subtly different
/// than "origin metadata". Client origin refers to the specific source of the metric, including based on
/// client-provided identifiers, such as container ID. Origin metadata deals with a higher-level categorization of the
/// source of the metric, such as "dogstatsd" for all metrics received by DogStatsD, etc.
///
/// More specifically, client origin is used to drive tags which are added to the metric, whereas origin metadata is
/// out-of-band data sent along in the payload to downstream systems if supported.
///
/// ## Missing
///
/// - full alignment with entity ID/client origin handling in terms of which one we use for getting enrichment tags
pub struct OriginEnrichmentConfiguration<E> {
    env_provider: E,
}

impl<E> OriginEnrichmentConfiguration<E> {
    /// Creates a new `OriginEnrichmentConfiguration` with the given environment provider.
    pub fn from_environment_provider(env_provider: E) -> Self {
        Self { env_provider }
    }
}

#[async_trait]
impl<E> SynchronousTransformBuilder for OriginEnrichmentConfiguration<E>
where
    E: EnvironmentProvider + Clone + Send + Sync + 'static,
    <E::Workload as WorkloadProvider>::Error: std::error::Error + Send + Sync,
{
    async fn build(&self) -> Result<Box<dyn SynchronousTransform + Send>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Box::new(OriginEnrichment {
            env_provider: self.env_provider.clone(),
        }))
    }
}

pub struct OriginEnrichment<E> {
    env_provider: E,
}

impl<E> OriginEnrichment<E>
where
    E: EnvironmentProvider,
    <E::Workload as WorkloadProvider>::Error: std::error::Error + Send + Sync,
{
    fn enrich_metric(&self, metric: &mut Metric) {
        // Try to collect various pieces of client origin information from the metric tags. For any tags that we collect
        // information from, we remove them from the original set of metric tags, as they're only used for driving
        // enrichment logic.
        //
        // This may also result in changing the origin metadata in certain cases, such as if we're dealing with a JMX
        // check metric.
        //
        // TODO: Follow the approach that the Agent takes where the tags are iterated directly by index, and then
        // removed from the vector by simply overwriting the slot with the next non-consumed tag. This would let us let
        // us avoid having to shift all elements after the tag to remove.
        let mut maybe_entity_id = None;
        let mut maybe_container_id = None;
        let mut maybe_cardinality = None;
        let mut maybe_jmx_check_name = None;
        let mut maybe_origin_pid = None;

        metric.context.tags.retain(|tag| {
            if tag.key() == ENTITY_ID_TAG_KEY {
                maybe_entity_id = tag.as_value_string();
                false
            } else if tag.key() == CARDINALITY_TAG_KEY {
                maybe_cardinality = tag.as_value_string();
                false
            } else if tag.key() == JMX_CHECK_NAME_TAG_KEY {
                maybe_jmx_check_name = tag.as_value_string();
                false
            } else if tag.key() == CONTAINER_ID_TAG_KEY {
                // NOTE: This isn't _actually_ set as a tag on the metric, but directly by the DogStatsD decoder since
                // we want to keep the metric data model a little cleaner.
                //
                // This is a bit of a hack, but it's a bit cleaner than adding a new field to `Metric` that never gets
                // used after enrichment.
                maybe_container_id = tag.as_value_string();
                false
            } else if tag.key() == ORIGIN_PID_TAG_KEY {
                maybe_origin_pid = tag.as_value_string().and_then(|s| s.parse::<u32>().ok());
                false
            } else {
                true
            }
        });

        // If this metric originates from a JMX check, update the metric's origin.
        if let Some(jmx_check_name) = maybe_jmx_check_name {
            metric.metadata.origin = Some(MetricOrigin::jmx_check(&jmx_check_name));
        }

        // Determine the correct entity ID using the following precedence mapping:
        //
        // - entity ID (extracted from `dd.internal.entity_id` tag; non-prefixed pod UID)
        // - container ID (extracted from `saluki.internal.container_id` tag, which comes from special "container ID"
        //   extension in DogStatsD protocol; non-prefixed container ID)
        // - origin PID (extracted via UDS socket credentials)
        //
        // NOTE: In the Datadog Agent, there's the possibility that a metric is enriched twice: once using the origin
        // PID if the entity ID was not specified, and again if container ID was specified. I haven't fully untangled
        // this yet, but I'm applying priority to the container ID because that enrichment step comes after the UDS one,
        // and so we only do one enrichment step here.
        let maybe_client_origin_entity_id = maybe_entity_id
            .and_then(|entity_id| {
                if entity_id != ENTITY_ID_IGNORE_VALUE {
                    Some(EntityId::PodUid(entity_id))
                } else {
                    None
                }
            })
            .or_else(|| maybe_container_id.and_then(EntityId::from_raw_container_id))
            .or_else(|| maybe_origin_pid.map(EntityId::ContainerPid));

        if let Some(entity_id) = maybe_client_origin_entity_id {
            // TODO: Just hardcoding this to high cardinality for now.
            let cardinality = maybe_cardinality
                .and_then(TagCardinality::parse)
                .unwrap_or(TagCardinality::High);

            match self
                .env_provider
                .workload()
                .get_tags_for_entity(&entity_id, cardinality)
            {
                Some(tags) => {
                    trace!(?entity_id, tags_len = tags.len(), "Found tags for entity.");
                    metric.context.tags.extend(tags);
                }
                None => trace!(entity_id = entity_id.to_string(), "No tags found for entity."),
            }
        }
    }
}

impl<E> SynchronousTransform for OriginEnrichment<E>
where
    E: EnvironmentProvider,
    <E::Workload as WorkloadProvider>::Error: std::error::Error + Send + Sync,
{
    fn transform_buffer(&self, event_buffer: &mut EventBuffer) {
        for event in event_buffer {
            match event {
                Event::Metric(metric) => self.enrich_metric(metric),
            }
        }
    }
}
