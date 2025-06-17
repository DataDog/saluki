use std::sync::Arc;

use saluki_context::{
    origin::{OriginTagCardinality, OriginTagsResolver, RawOrigin},
    tags::SharedTagSet,
};
use saluki_env::{workload::origin::ResolvedOrigin, WorkloadProvider};
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
#[derive(Clone, Deserialize)]
pub struct OriginEnrichmentConfiguration {
    /// Whether or not a client-provided entity ID should take precedence over automatically detected origin metadata.
    ///
    /// When a client-provided entity ID is specified, and an origin process ID has automatically been detected, setting
    /// this to `true` will cause the origin process ID to be ignored.
    ///
    /// Defaults to `false`.
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

impl Default for OriginEnrichmentConfiguration {
    fn default() -> Self {
        Self {
            entity_id_precedence: false,
            tag_cardinality: default_tag_cardinality(),
            origin_detection_unified: false,
            origin_detection_optout: default_origin_detection_optout(),
        }
    }
}

#[derive(Clone)]
pub(super) struct DogStatsDOriginTagResolver {
    config: OriginEnrichmentConfiguration,
    workload_provider: Arc<dyn WorkloadProvider + Send + Sync>,
}

impl DogStatsDOriginTagResolver {
    pub fn new(
        config: OriginEnrichmentConfiguration, workload_provider: Arc<dyn WorkloadProvider + Send + Sync>,
    ) -> Self {
        Self {
            config,
            workload_provider,
        }
    }

    fn collect_origin_tags(&self, origin: ResolvedOrigin) -> SharedTagSet {
        let mut collected_tags = SharedTagSet::default();

        // Examine the various possible entity ID values, and based on their state, use one or more of them to grab any
        // enriched tags attached to the entities. Below is a description of each entity ID we may have extracted:
        //
        // - entity ID (extracted from `dd.internal.entity_id` tag; non-prefixed pod UID)
        // - container ID (extracted from special "container ID" extension in DogStatsD protocol; non-prefixed container ID)
        // - container ID via origin PID (extracted via UDS socket credentials)
        let maybe_process_id = origin.process_id();
        let maybe_entity_id = origin.pod_uid();
        let maybe_container_id = origin.container_id();

        let tag_cardinality = origin.cardinality().unwrap_or(self.config.tag_cardinality);

        if !self.config.origin_detection_unified {
            if self.config.origin_detection_optout && tag_cardinality == OriginTagCardinality::None {
                trace!("Skipping origin enrichment for DogStatsD metric with cardinality 'none'.");
                return collected_tags;
            }

            // If we discovered an entity ID via origin detection, and no client-provided entity ID was provided (or it was,
            // but entity ID precedence is disabled), then try to get tags for the detected entity ID.
            if let Some(entity_id) = maybe_process_id {
                if maybe_entity_id.is_none() || !self.config.entity_id_precedence {
                    if let Some(tags) = self.workload_provider.get_tags_for_entity(entity_id, tag_cardinality) {
                        collected_tags.extend_from_shared(&tags);
                    } else {
                        trace!(
                            ?entity_id,
                            cardinality = tag_cardinality.as_str(),
                            "No tags found for entity."
                        );
                    }
                }
            }

            // If we have a client-provided entity ID, try to get tags for the entity based on those. A
            // client-provided entity ID takes precedence over the container ID.
            let maybe_client_entity_id = maybe_entity_id.or(maybe_container_id);
            if let Some(entity_id) = maybe_client_entity_id {
                if let Some(tags) = self.workload_provider.get_tags_for_entity(entity_id, tag_cardinality) {
                    collected_tags.extend_from_shared(&tags);
                } else {
                    trace!(
                        ?entity_id,
                        cardinality = tag_cardinality.as_str(),
                        "No tags found for entity."
                    );
                }
            }
        } else {
            if tag_cardinality == OriginTagCardinality::None {
                trace!("Skipping origin enrichment for metric with cardinality 'none'.");
                return collected_tags;
            }

            // Try all possible detected entity IDs, enriching in the following order of precedence: local process ID,
            // local container ID, client-provided entity ID, External Data-based pod ID, and External Data-based
            // container ID.
            let maybe_external_data_pod_uid = origin.resolved_external_data().map(|red| red.pod_entity_id());
            let maybe_external_data_container_id = origin.resolved_external_data().map(|red| red.container_entity_id());
            let maybe_entity_ids = &[
                maybe_process_id,
                maybe_container_id,
                maybe_entity_id,
                maybe_external_data_pod_uid,
                maybe_external_data_container_id,
            ];
            for entity_id in maybe_entity_ids.iter().flatten() {
                if let Some(tags) = self.workload_provider.get_tags_for_entity(entity_id, tag_cardinality) {
                    collected_tags.extend_from_shared(&tags);
                } else {
                    trace!(
                        ?entity_id,
                        cardinality = tag_cardinality.as_str(),
                        "No tags found for entity."
                    );
                }
            }
        }

        collected_tags
    }
}

impl OriginTagsResolver for DogStatsDOriginTagResolver {
    fn resolve_origin_tags(&self, origin: RawOrigin<'_>) -> SharedTagSet {
        match self.workload_provider.get_resolved_origin(origin.clone()) {
            Some(resolved_origin) => self.collect_origin_tags(resolved_origin),
            None => {
                trace!(?origin, "No resolved origin found for origin.");
                SharedTagSet::default()
            }
        }
    }
}

/// Builds an `RawOrigin` object from the given metric packet.
pub fn origin_from_metric_packet<'packet>(packet: &MetricPacket<'packet>) -> RawOrigin<'packet> {
    let mut origin = RawOrigin::default();
    origin.set_pod_uid(packet.pod_uid);
    origin.set_container_id(packet.container_id);
    origin.set_external_data(packet.external_data);
    origin.set_cardinality(packet.cardinality);
    origin
}
