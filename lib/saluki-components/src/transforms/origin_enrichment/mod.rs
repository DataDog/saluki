use async_trait::async_trait;

use saluki_core::{components::transforms::*, constants::datadog::*, topology::interconnect::EventBuffer};
use saluki_env::{EnvironmentProvider, TaggerProvider};
use saluki_event::{
    metric::{Metric, MetricOrigin},
    Event,
};
use tracing::warn;

/// Origin Enrichment synchronous transform.
///
/// Enriches metrics with tags based on the client origin of a metric. The "origin" referred to here is subtly different
/// than "origin metadata". Client origin refers to the specific source of the metric, including based on
/// client-provided identifiers, such as container ID. Origin metadata deals with a higher-level categorization of the
/// source of the metric, such as "dogstatsd" for all metrics received by DogStatsD, etc.
///
/// More specifically, client origin is used to drive tags which are added to the metric, whereas origin metadata is
/// out-of-band data sent along in the payload to downstream systems if supported.
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
    <E::Tagger as TaggerProvider>::Error: std::error::Error + Send + Sync,
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
    <E::Tagger as TaggerProvider>::Error: std::error::Error,
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
            } else {
                true
            }
        });

        // If this metric originates from a JMX check, update the metric's origin.
        if let Some(jmx_check_name) = maybe_jmx_check_name {
            metric.metadata.origin = Some(MetricOrigin::jmx_check(&jmx_check_name));
        }

        // Determine the client origin.
        let maybe_client_origin_entity_id = match maybe_entity_id {
            Some(entity_id) if entity_id != ENTITY_ID_IGNORE_VALUE => {
                Some(format!("kubernetes_pod_uid://{}", entity_id))
            }
            _ => maybe_container_id.map(|id| format!("container_id://{}", id)),
        };

        if let Some(client_origin_entity_id) = maybe_client_origin_entity_id {
            let entity_tags = match self.env_provider.tagger().get_tags_for_entity(&client_origin_entity_id) {
                Ok(tags) => tags,
                Err(e) => {
                    // TODO: Should this actually be a warning? :thinkies:
                    warn!("failed to get tags for entity {}: {}", client_origin_entity_id, e);
                    return;
                }
            };

            metric.context.tags.extend(entity_tags);
        }
    }
}

impl<E> SynchronousTransform for OriginEnrichment<E>
where
    E: EnvironmentProvider,
    <E::Tagger as TaggerProvider>::Error: std::error::Error,
{
    fn transform_buffer(&self, event_buffer: &mut EventBuffer) {
        for event in event_buffer {
            match event {
                Event::Metric(metric) => self.enrich_metric(metric),
            }
        }
    }
}
