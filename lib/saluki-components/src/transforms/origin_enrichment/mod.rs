use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_core::{components::transforms::*, constants::datadog::*, topology::interconnect::EventBuffer};
use saluki_env::{
    workload::{entity::EntityId, metadata::TagCardinality},
    EnvironmentProvider, WorkloadProvider,
};
use saluki_error::GenericError;
use saluki_event::{
    metric::{Metric, MetricOrigin},
    Event,
};
use serde::Deserialize;
use tracing::trace;

const fn default_tag_cardinality() -> TagCardinality {
    TagCardinality::Low
}

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
#[derive(Deserialize)]
pub struct OriginEnrichmentConfiguration<E = ()> {
    #[serde(skip)]
    env_provider: E,

    /// Whether or not a client-provided entity ID should take precedence over automatically detected origin metadata.
    ///
    /// When a client-provided entity ID is specified, and an origin process ID has automatically been detected, setting
    /// this to `true` will cause the origin process ID to be ignored.
    #[serde(rename = "dogstatsd_entity_id_precedence", default)]
    entity_id_precedence: bool,

    /// The default cardinality of tags to enrich metrics with.
    #[serde(rename = "dogstatsd_tag_cardinality", default = "default_tag_cardinality")]
    tag_cardinality: TagCardinality,

    /// Whether or not to use the unified origin detection behavior.
    ///
    /// When set to `true`, all detected entity IDs -- UDS Origin Detection, `dd.internal.entity_id`, container ID from
    /// DogStatsD payload -- will be used for querying tags to enrich with. When set to `false`, the original precedence
    /// behavior will be used, which enriches with the entity ID detected via Origin Detection first [1], and then
    /// potentially again with either the client-provided entity ID (`dd.internal.entity_id`) or the container ID from
    /// the DogStatsD payload, with the client-provided entity ID taking precedence.
    ///
    /// Defaults to `false`.
    ///
    /// [1]: if an entity ID was detected via Origin Detection, it is only used if either no client-provided entity ID
    ///      was present or if `entity_id_precedence` is set to `false`.
    #[serde(rename = "dogstatsd_origin_detection_unified", default)]
    origin_detection_unified: bool,
}

impl OriginEnrichmentConfiguration<()> {
    /// Creates a new `OriginEnrichmentConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }
}

impl<E> OriginEnrichmentConfiguration<E> {
    /// Sets the environment provider for the configuration.
    pub fn with_environment_provider<E2>(self, env_provider: E2) -> OriginEnrichmentConfiguration<E2> {
        OriginEnrichmentConfiguration {
            env_provider,
            entity_id_precedence: self.entity_id_precedence,
            tag_cardinality: self.tag_cardinality,
            origin_detection_unified: self.origin_detection_unified,
        }
    }
}

#[async_trait]
impl<E> SynchronousTransformBuilder for OriginEnrichmentConfiguration<E>
where
    E: EnvironmentProvider + Clone + Send + Sync + 'static,
    <E::Workload as WorkloadProvider>::Error: std::error::Error + Send + Sync,
{
    async fn build(&self) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        Ok(Box::new(OriginEnrichment {
            env_provider: self.env_provider.clone(),
            entity_id_precedence: self.entity_id_precedence,
            origin_detection_unified: self.origin_detection_unified,
            tag_cardinality: self.tag_cardinality,
        }))
    }
}

impl<E> MemoryBounds for OriginEnrichmentConfiguration<E> {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

pub struct OriginEnrichment<E> {
    env_provider: E,
    entity_id_precedence: bool,
    origin_detection_unified: bool,
    tag_cardinality: TagCardinality,
}

impl<E> OriginEnrichment<E>
where
    E: EnvironmentProvider,
    <E::Workload as WorkloadProvider>::Error: std::error::Error + Send + Sync,
{
    fn enrich_metric(&self, metric: &mut Metric) {
        // TODO: This code below worked before when we could just mutate our context willy-nilly, but not so much when
        // we're using a resolved handle.
        //
        // This code differs from some other usages where this is a case where we really _do_ want to attach a bunch of
        // new tags to a metric. This would involve rebuilding the context, which isn't _horrible_ but also wouldn't be
        // super great, just in terms of allocations... unless we also wire in the interning stuff there.
        //
        // My thought is that eventually we might have `TagSet` be able to chain itself, so that we could basically
        // merge together discrete sets of tags without having to re-allocate a vector that holds all of them together
        // contiguously.
        //
        // Just a thought, and doesn't really address having to eventually need to allocate a vector to hold them all
        // contiguously as `Vec<protobuf::Chars>` for when we build our request payloads, but could be incremental
        // progress.
        //todo!()

        /*
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
        let mut tag_cardinality = self.tag_cardinality;
        let mut maybe_jmx_check_name = None;
        let mut maybe_origin_pid = None;

        metric.context.tags.retain(|tag| {
            if tag.key() == ENTITY_ID_TAG_KEY {
                maybe_entity_id = tag
                    .as_value_string()
                    .filter(|s| s != ENTITY_ID_IGNORE_VALUE)
                    .map(EntityId::PodUid);
                false
            } else if tag.key() == CARDINALITY_TAG_KEY {
                if let Some(cardinality) = tag.as_value_string().and_then(TagCardinality::parse) {
                    tag_cardinality = cardinality;
                }
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
                maybe_container_id = tag.as_value_string().and_then(EntityId::from_raw_container_id);
                false
            } else if tag.key() == ORIGIN_PID_TAG_KEY {
                maybe_origin_pid = tag
                    .as_value_string()
                    .and_then(|s| s.parse::<u32>().ok())
                    .map(EntityId::ContainerPid);
                false
            } else {
                true
            }
        });

        // If this metric originates from a JMX check, update the metric's origin.
        if let Some(jmx_check_name) = maybe_jmx_check_name {
            metric.metadata.origin = Some(MetricOrigin::jmx_check(&jmx_check_name));
        }

        // Examine the various possible entity ID values, and based on their state, use one or more of them to enrich
        // the tags for the given metric. Below is a description of each entity ID we may have extracted:
        //
        // - entity ID (extracted from `dd.internal.entity_id` tag; non-prefixed pod UID)
        // - container ID (extracted from `saluki.internal.container_id` tag, which comes from special "container ID"
        //   extension in DogStatsD protocol; non-prefixed container ID)
        // - origin PID (extracted via UDS socket credentials)
        //
        // NOTE: There's the possibility that a metric is enriched multiple times, regardless of whether or not we're in
        // unified mode. We're currently using an extend approach, which would lead to duplicate tag values if we extend
        // with a tag key that already exists. Need to figure to figure out if the Datadog Agent's approach is based on
        // an override strategy (i.e. if a tag key already exists, it's overwritten) or an extend strategy, like we
        // have. Perhaps even further, does the Datadog Agent ignore duplicate _values_ for a given tag key?

        if !self.origin_detection_unified {
            // If we discovered an entity ID via origin detection, and no client-provided entity ID was provided (or it was,
            // but entity ID precedence is disabled), then try to get tags for the detected entity ID.
            if let Some(origin_pid) = maybe_origin_pid {
                if maybe_entity_id.is_none() || !self.entity_id_precedence {
                    match self
                        .env_provider
                        .workload()
                        .get_tags_for_entity(&origin_pid, tag_cardinality)
                    {
                        Some(tags) => {
                            trace!(entity_id = ?origin_pid, tags_len = tags.len(), "Found tags for entity.");
                            metric.context.tags.extend(tags);
                        }
                        None => trace!(entity_id = ?origin_pid, "No tags found for entity."),
                    }
                }
            }

            // If we have a client-provided entity ID or a container ID, try to get tags for the entity based on those. A
            // client-provided entity ID takes precedence over the container ID.
            let maybe_client_entity_id = maybe_entity_id.or(maybe_container_id);
            if let Some(entity_id) = maybe_client_entity_id {
                match self
                    .env_provider
                    .workload()
                    .get_tags_for_entity(&entity_id, tag_cardinality)
                {
                    Some(tags) => {
                        trace!(?entity_id, tags_len = tags.len(), "Found tags for entity.");
                        metric.context.tags.extend(tags);
                    }
                    None => trace!(?entity_id, "No tags found for entity."),
                }
            }
        } else {
            // Try all possible detected entity IDs, enriching in the following order of precedence: origin PID,
            // then container ID, and finally the client-provided entity ID.
            let maybe_entity_ids = &[maybe_origin_pid, maybe_container_id, maybe_entity_id];
            for entity_id in maybe_entity_ids.iter().flatten() {
                match self
                    .env_provider
                    .workload()
                    .get_tags_for_entity(entity_id, tag_cardinality)
                {
                    Some(tags) => {
                        trace!(?entity_id, tags_len = tags.len(), "Found tags for entity.");
                        metric.context.tags.extend(tags);
                    }
                    None => trace!(?entity_id, "No tags found for entity."),
                }
            }
        }
        */
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
