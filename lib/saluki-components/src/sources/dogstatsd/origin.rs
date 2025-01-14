use papaya::HashMap;
use saluki_context::{
    origin::{OriginEnricher, OriginInfo, OriginKey, OriginTagCardinality},
    tags::{SharedTagSet, TagSet},
    Resolvable, TagVisitor,
};
use saluki_env::{workload::EntityId, WorkloadProvider};
use saluki_io::deser::codec::dogstatsd::MetricPacket;
use serde::Deserialize;
use tracing::trace;

const fn default_tag_cardinality() -> OriginTagCardinality {
    OriginTagCardinality::Low
}

const fn default_origin_detection_optout() -> bool {
    true
}

/// Origin enrichment configuration.
///
/// Origin enrichment controls the when and how of enriching metrics ingested via DogStatsD based on various sources of
/// "origin" information, such as specific metric tags or UDS socket credentials. Enrichment involves adding additional
/// metric tags that describe the origin of the metric, such as the Kubernetes pod or container.
#[derive(Clone, Debug, Deserialize)]
pub struct OriginEnrichmentConfiguration<W = ()> {
    #[serde(skip)]
    workload_provider: W,

    /// Whether or not a client-provided entity ID should take precedence over automatically detected origin metadata.
    ///
    /// When a client-provided entity ID is specified, and an origin process ID has automatically been detected, setting
    /// this to `true` will cause the origin process ID to be ignored.
    #[serde(rename = "dogstatsd_entity_id_precedence", default)]
    entity_id_precedence: bool,

    /// The default cardinality of tags to enrich metrics with.
    #[serde(rename = "dogstatsd_tag_cardinality", default = "default_tag_cardinality")]
    tag_cardinality: OriginTagCardinality,

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

    /// Whether or not to opt out of origin detection for DogStatsD metrics.
    ///
    /// When set to `true`, and the metric explicitly denotes a cardinality of "none", origin enrichment will be
    /// skipped. This is only applicable to DogStatsD metrics when unified origin detection behavior is not enabled.
    ///
    /// Defaults to `true`.
    #[serde(
        rename = "dogstatsd_origin_optout_enabled",
        default = "default_origin_detection_optout"
    )]
    origin_detection_optout: bool,
}

impl<W> OriginEnrichmentConfiguration<W> {
    /// Sets the workload provider for the configuration.
    pub fn with_workload_provider<W2>(self, workload_provider: W2) -> OriginEnrichmentConfiguration<W2> {
        OriginEnrichmentConfiguration {
            workload_provider,
            entity_id_precedence: self.entity_id_precedence,
            tag_cardinality: self.tag_cardinality,
            origin_detection_unified: self.origin_detection_unified,
            origin_detection_optout: self.origin_detection_optout,
        }
    }
}

impl<W> OriginEnrichmentConfiguration<W>
where
    W: Clone,
{
    pub fn build(&self) -> DogStatsDOriginEnricher<W> {
        DogStatsDOriginEnricher {
            workload_provider: self.workload_provider.clone(),
            entity_id_precedence: self.entity_id_precedence,
            tag_cardinality: self.tag_cardinality,
            origin_detection_unified: self.origin_detection_unified,
            origin_detection_optout: self.origin_detection_optout,
            origin_cache: HashMap::default(),
        }
    }
}

pub struct DogStatsDOriginEnricher<W> {
    workload_provider: W,
    entity_id_precedence: bool,
    tag_cardinality: OriginTagCardinality,
    origin_detection_unified: bool,
    origin_detection_optout: bool,
    origin_cache: HashMap<OriginKey, SharedTagSet>,
}

impl<W> DogStatsDOriginEnricher<W>
where
    W: WorkloadProvider + Send + Sync,
{
    fn resolve_origin(&self, origin_info: &OriginInfo<'_>) -> Option<OriginKey> {
        // Calculate the key for this origin information, and see if we've already previously resolved it.
        let origin_key = OriginKey::from_opaque(origin_info);
        if let Some(tags) = self.origin_cache.pin().get(&origin_key) {
            trace!(origin = %origin_info, tags_len = tags.len(), "Found existing origin during resolving.");
            return Some(origin_key);
        }

        // We couldn't find the origin information in the cache, so we'll try to resolve it now.
        let mut had_entity_matches = false;
        let mut enriched_tags = TagSet::default();

        // Examine the various possible entity ID values, and based on their state, use one or more of them to grab any
        // enriched tags attached to the entities. Below is a description of each entity ID we may have extracted:
        //
        // - entity ID (extracted from `dd.internal.entity_id` tag; non-prefixed pod UID)
        // - container ID (extracted from `saluki.internal.container_id` tag, which comes from special "container ID"
        //   extension in DogStatsD protocol; non-prefixed container ID)
        // - origin PID (extracted via UDS socket credentials)
        let maybe_entity_id = origin_info.pod_uid().and_then(EntityId::from_pod_uid);
        let maybe_container_id = origin_info.container_id().and_then(EntityId::from_raw_container_id);
        let maybe_origin_pid = origin_info.process_id().map(EntityId::ContainerPid);

        let tag_cardinality = origin_info.cardinality().unwrap_or(self.tag_cardinality);

        if !self.origin_detection_unified {
            if self.origin_detection_optout && tag_cardinality == OriginTagCardinality::None {
                trace!("Skipping origin enrichment for DogStatsD metric with cardinality 'none'.");
                return None;
            }

            // If we discovered an entity ID via origin detection, and no client-provided entity ID was provided (or it was,
            // but entity ID precedence is disabled), then try to get tags for the detected entity ID.
            if let Some(origin_pid) = maybe_origin_pid {
                if maybe_entity_id.is_none() || !self.entity_id_precedence {
                    match self.workload_provider.get_tags_for_entity(&origin_pid, tag_cardinality) {
                        Some(tags) => {
                            trace!(entity_id = ?origin_pid, tags_len = tags.len(), "Found tags for entity.");

                            had_entity_matches = true;
                            enriched_tags.merge_missing_shared(&tags);
                        }
                        None => trace!(entity_id = ?origin_pid, "No tags found for entity."),
                    }
                }
            }

            // If we have a client-provided pod UID or a container ID, try to get tags for the entity based on those. A
            // client-provided entity ID takes precedence over the container ID.
            let maybe_client_entity_id = maybe_entity_id.or(maybe_container_id);
            if let Some(entity_id) = maybe_client_entity_id {
                match self.workload_provider.get_tags_for_entity(&entity_id, tag_cardinality) {
                    Some(tags) => {
                        trace!(?entity_id, tags_len = tags.len(), "Found tags for entity.");

                        had_entity_matches = true;
                        enriched_tags.merge_missing_shared(&tags);
                    }
                    None => trace!(?entity_id, "No tags found for entity."),
                }
            }
        } else {
            if tag_cardinality == OriginTagCardinality::None {
                trace!("Skipping origin enrichment for metric with cardinality 'none'.");
                return None;
            }

            // Try all possible detected entity IDs, enriching in the following order of precedence: origin PID,
            // then container ID, and finally the client-provided entity ID.
            let maybe_entity_ids = &[maybe_origin_pid, maybe_container_id, maybe_entity_id];
            for entity_id in maybe_entity_ids.iter().flatten() {
                match self.workload_provider.get_tags_for_entity(entity_id, tag_cardinality) {
                    Some(tags) => {
                        trace!(?entity_id, tags_len = tags.len(), "Found tags for entity.");

                        had_entity_matches = true;
                        enriched_tags.merge_missing_shared(&tags);
                    }
                    None => trace!(?entity_id, "No tags found for entity."),
                }
            }

            // If the metric has External Data attached, try to resolve an entity ID from it and enrich the metric with
            // any tags attached to that entity ID.
            if let Some(external_data) = origin_info.external_data() {
                if let Some(entity_id) = self
                    .workload_provider
                    .resolve_entity_id_from_external_data(external_data)
                {
                    match self.workload_provider.get_tags_for_entity(&entity_id, tag_cardinality) {
                        Some(tags) => {
                            trace!(?entity_id, tags_len = tags.len(), "Found tags for entity.");

                            had_entity_matches = true;
                            enriched_tags.merge_missing_shared(&tags);
                        }
                        None => trace!(?entity_id, "No tags found for entity."),
                    }
                }
            }
        }

        // If we had any entity matches, even if we have no tags for those entities, we'll consider this a "hit" and
        // cache whatever we got. This allows us to avoid checking _every single time_ if there isn't anything that
        // matches the given origin information. The flip side is that if we have no entries at all in the workload
        // provider related to the origin information we're resolving, then we likely just haven't been _given_ the tags
        // yet, and so we treat that as a "miss" and avoid caching it.
        if had_entity_matches {
            let tags_len = enriched_tags.len();
            self.origin_cache.pin().insert(origin_key, enriched_tags.into_shared());

            trace!(origin = %origin_info, tags_len, "Caching tags for origin.");
            Some(origin_key)
        } else {
            None
        }
    }
}

impl<W> OriginEnricher for DogStatsDOriginEnricher<W>
where
    W: WorkloadProvider + Send + Sync,
{
    fn resolve_origin_key(&self, origin_info: &OriginInfo<'_>) -> Option<OriginKey> {
        self.resolve_origin(origin_info)
    }

    fn collect_origin_tags(&self, origin_key: OriginKey, tags: &mut TagSet) {
        if let Some(origin_tags) = self.origin_cache.pin().get(&origin_key) {
            tags.merge_missing_shared(origin_tags);
        }
    }
}

impl<W> Default for OriginEnrichmentConfiguration<W>
where
    W: Default,
{
    fn default() -> Self {
        Self {
            workload_provider: Default::default(),
            entity_id_precedence: false,
            tag_cardinality: default_tag_cardinality(),
            origin_detection_unified: false,
            origin_detection_optout: default_origin_detection_optout(),
        }
    }
}

/// A resolvable metric packet.
///
/// Allows resolving a context based on a given borrowed metric packet and its origin information.
pub struct ResolvableMetricPacket<'a> {
    packet: &'a MetricPacket<'a>,
    origin_info: OriginInfo<'a>,
}

impl<'a> ResolvableMetricPacket<'a> {
    /// Creates a new `ResolvableMetricPacket` with the given metric packet and origin information.
    pub fn new(packet: &'a MetricPacket<'a>, origin_info: OriginInfo<'a>) -> Self {
        Self { packet, origin_info }
    }
}

impl<'a> TagVisitor for ResolvableMetricPacket<'a> {
    fn visit_tags<F>(&self, mut visitor: F)
    where
        F: FnMut(&str),
    {
        for tag in self.packet.tags.clone() {
            visitor(tag);
        }
    }
}

impl<'a> Resolvable for ResolvableMetricPacket<'a> {
    fn name(&self) -> &str {
        self.packet.metric_name
    }

    fn origin_info(&self) -> Option<&OriginInfo<'a>> {
        Some(&self.origin_info)
    }
}

/// Builds an `OriginInfo` object from the given metric packet.
pub fn origin_info_from_metric_packet<'packet>(packet: &MetricPacket<'packet>) -> OriginInfo<'packet> {
    let mut origin_info = OriginInfo::default();
    origin_info.set_pod_uid(packet.pod_uid);
    origin_info.set_container_id(packet.container_id);
    origin_info.set_external_data(packet.external_data);
    origin_info.set_cardinality(packet.cardinality);
    origin_info
}
