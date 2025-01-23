use std::sync::Arc;

use papaya::HashMap;
use saluki_context::origin::{OriginInfo, OriginKey, OriginTagCardinality, OriginTagVisitor};
use serde::Deserialize;
use tracing::trace;

use super::stores::{ExternalDataStoreResolver, TagStoreQuerier};
use crate::workload::{external_data::ResolvedExternalData, EntityId};

/// A handle for querying the tags associated with an origin.
#[derive(Clone)]
pub struct OriginTagsQuerier {
    config: OriginEnrichmentConfiguration,
    tag_querier: TagStoreQuerier,
    ed_resolver: ExternalDataStoreResolver,
    key_mappings: Arc<HashMap<OriginKey, OwnedOriginInfo>>,
}

impl OriginTagsQuerier {
    /// Creates a new `OriginTagsQuerier`.
    pub fn new(
        config: OriginEnrichmentConfiguration, tag_querier: TagStoreQuerier, ed_resolver: ExternalDataStoreResolver,
    ) -> Self {
        Self {
            config,
            tag_querier,
            ed_resolver,
            key_mappings: Arc::new(HashMap::default()),
        }
    }

    /// Resolves an origin key from the provided origin information.
    ///
    /// If the origin information is empty, this method will return `None`. Otherwise, it will hash the origin
    pub fn resolve_origin_key_from_info(&self, origin_info: OriginInfo<'_>) -> Option<OriginKey> {
        // If there's no origin information at all, then there's nothing to key off of.
        if origin_info.is_empty() {
            return None;
        }

        // We quickly hash the origin information to its key form, and see if we're already tracking it.
        //
        // If we haven't seen this origin yet, we generate an owned representation of its information and store it in
        // the state, mapped to the resulting key. We'll utilize these mappings later on when actually resolving the
        // necessary origin tags.
        let origin_key = OriginKey::from_opaque(&origin_info);

        // TODO: This is a slow leak because we never remove entries and have no signal to know when to do so.
        //
        // We should likely just use `quick-cache` here, but it's not a huge deal for now.
        let _ = self.key_mappings.pin().get_or_insert_with(origin_key, || {
            OwnedOriginInfo::from_borrowed_info(origin_info, &self.ed_resolver)
        });

        Some(origin_key)
    }

    /// Visits the origin tags associated with the given origin key.
    pub fn visit_origin_tags(&self, origin_key: OriginKey, visitor: &mut dyn OriginTagVisitor) {
        // TODO: I know our only usage of the origin tags resolver is for DSD, so baking this in like this is _fine_,
        // but conceptually this could stand to be configurable.
        let dsd_resolver = DogStatsDOriginTagResolver {
            config: &self.config,
            querier: &self.tag_querier,
        };

        match self.key_mappings.pin().get(&origin_key) {
            Some(origin) => {
                dsd_resolver.visit_origin_tags(origin, visitor);
            }
            None => {
                trace!(?origin_key, "No origin information found for key.");
            }
        }
    }
}

/// An owned and resolved representation of `OriginInfo<'a>`
///
/// This representation is used to store the pre-calculated entity IDs derived from a borrowed `OriginInfo<'a>` in order
/// to speed the lookup of origin tags attached to each individual entity ID that comprises an origin.
#[derive(Hash)]
struct OwnedOriginInfo {
    cardinality: Option<OriginTagCardinality>,
    process_id: Option<EntityId>,
    container_id: Option<EntityId>,
    pod_uid: Option<EntityId>,
    resolved_external_data: Option<ResolvedExternalData>,
}

impl OwnedOriginInfo {
    fn from_borrowed_info(origin_info: OriginInfo<'_>, ed_resolver: &ExternalDataStoreResolver) -> Self {
        // We have to do a little song-and-dance to get the resolved External Data out, because the interface provided
        // can't return a reference to the resolved External Data directly.
        let resolved_external_data = match origin_info.external_data() {
            Some(raw_external_data) => {
                let mut maybe_resolved_external_data = None;
                ed_resolver.resolve_external_data(raw_external_data, |maybe_resolved| {
                    maybe_resolved_external_data = maybe_resolved.cloned();
                });
                maybe_resolved_external_data
            }
            None => None,
        };

        Self {
            cardinality: origin_info.cardinality(),
            process_id: origin_info.process_id().map(EntityId::ContainerPid),
            container_id: origin_info.container_id().and_then(EntityId::from_raw_container_id),
            pod_uid: origin_info.pod_uid().and_then(EntityId::from_pod_uid),
            resolved_external_data,
        }
    }
}

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

struct DogStatsDOriginTagResolver<'a> {
    config: &'a OriginEnrichmentConfiguration,
    querier: &'a TagStoreQuerier,
}

impl<'a> DogStatsDOriginTagResolver<'a> {
    fn visit_origin_tags(&self, origin_info: &OwnedOriginInfo, visitor: &mut dyn OriginTagVisitor) {
        // Examine the various possible entity ID values, and based on their state, use one or more of them to grab any
        // enriched tags attached to the entities. Below is a description of each entity ID we may have extracted:
        //
        // - entity ID (extracted from `dd.internal.entity_id` tag; non-prefixed pod UID)
        // - container ID (extracted from `saluki.internal.container_id` tag, which comes from special "container ID"
        //   extension in DogStatsD protocol; non-prefixed container ID)
        // - origin PID (extracted via UDS socket credentials)
        let maybe_entity_id = origin_info.pod_uid.as_ref();
        let maybe_container_id = origin_info.container_id.as_ref();
        let maybe_origin_pid = origin_info.process_id.as_ref();

        let tag_cardinality = origin_info.cardinality.unwrap_or(self.config.tag_cardinality);

        if !self.config.origin_detection_unified {
            if self.config.origin_detection_optout && tag_cardinality == OriginTagCardinality::None {
                trace!("Skipping origin enrichment for DogStatsD metric with cardinality 'none'.");
                return;
            }

            // If we discovered an entity ID via origin detection, and no client-provided entity ID was provided (or it was,
            // but entity ID precedence is disabled), then try to get tags for the detected entity ID.
            if let Some(origin_pid) = maybe_origin_pid {
                if maybe_entity_id.is_none() || !self.config.entity_id_precedence {
                    match self.querier.get_entity_tags(origin_pid, tag_cardinality) {
                        Some(tags) => {
                            trace!(entity_id = ?origin_pid, tags_len = tags.len(), "Found tags for entity.");

                            for tag in &tags {
                                visitor.visit_tag(tag);
                            }
                        }
                        None => trace!(entity_id = ?origin_pid, "No tags found for entity."),
                    }
                }
            }

            // If we have a client-provided pod UID or a container ID, try to get tags for the entity based on those. A
            // client-provided entity ID takes precedence over the container ID.
            let maybe_client_entity_id = maybe_entity_id.or(maybe_container_id);
            if let Some(entity_id) = maybe_client_entity_id {
                match self.querier.get_entity_tags(entity_id, tag_cardinality) {
                    Some(tags) => {
                        trace!(?entity_id, tags_len = tags.len(), "Found tags for entity.");

                        for tag in &tags {
                            visitor.visit_tag(tag);
                        }
                    }
                    None => trace!(?entity_id, "No tags found for entity."),
                }
            }
        } else {
            if tag_cardinality == OriginTagCardinality::None {
                trace!("Skipping origin enrichment for metric with cardinality 'none'.");
                return;
            }

            // Try all possible detected entity IDs, enriching in the following order of precedence: origin PID,
            // local container ID, client-provided entity ID, External Data-based pod ID, and External Data-based
            // container ID.
            let maybe_external_data_pod_uid = origin_info
                .resolved_external_data
                .as_ref()
                .map(|red| red.pod_entity_id());
            let maybe_external_data_container_id = origin_info
                .resolved_external_data
                .as_ref()
                .map(|red| red.container_entity_id());
            let maybe_entity_ids = &[
                maybe_origin_pid,
                maybe_container_id,
                maybe_entity_id,
                maybe_external_data_pod_uid,
                maybe_external_data_container_id,
            ];
            for entity_id in maybe_entity_ids.iter().flatten() {
                match self.querier.get_entity_tags(entity_id, tag_cardinality) {
                    Some(tags) => {
                        trace!(?entity_id, tags_len = tags.len(), "Found tags for entity.");

                        for tag in &tags {
                            visitor.visit_tag(tag);
                        }
                    }
                    None => trace!(?entity_id, "No tags found for entity."),
                }
            }
        }
    }
}
