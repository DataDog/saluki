use std::sync::Arc;

use saluki_context::{
    origin::{OriginTagCardinality, OriginTagsResolver, RawOrigin},
    tags::SharedTagSet,
};
use saluki_env::{workload::origin::ResolvedOrigin, WorkloadProvider};
use saluki_io::deser::codec::dogstatsd::{EventPacket, MetricPacket, ServiceCheckPacket};
use serde::Deserialize;
use tracing::{info, trace};

use super::tags::WellKnownTags;

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
    #[serde(rename = "origin_detection_unified", default)]
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
        if config.origin_detection_unified {
            info!("Initializing origin detection for DogStatsD source in unified mode.");
        } else {
            info!("Initializing origin detection for DogStatsD source in legacy mode.");
        }

        Self {
            config,
            workload_provider,
        }
    }

    fn collect_origin_tags(&self, origin: ResolvedOrigin) -> SharedTagSet {
        let mut collected_tags = SharedTagSet::default();

        // Examine the various possible entity ID values, and based on their state, use one or more of them to grab any
        // enriched tags attached to the entities. We evalulate a number of possible entity IDs:
        //
        // - process ID (extracted via UDS socket credentials and mapped to a container ID)
        // - Local Data-based container ID (extracted from special "container ID" extension in DogStatsD protocol; either container ID or cgroup controller inode for the container)
        // - Local Data-based pod UID (extracted from `dd.internal.entity_id` tag)
        // - External Data-based container ID (derived from pod UID and container name in External Data)
        // - External Data-based pod UID (raw pod UID from External Data)
        let maybe_process_id = origin.process_id();
        let maybe_local_container_id = origin.container_id();
        let maybe_local_pod_uid = origin.pod_uid();
        let maybe_external_container_id = origin.resolved_external_data().map(|red| red.container_entity_id());
        let maybe_external_pod_uid = origin.resolved_external_data().map(|red| red.pod_entity_id());

        let tag_cardinality = origin.cardinality().unwrap_or(self.config.tag_cardinality);

        if !self.config.origin_detection_unified {
            if self.config.origin_detection_optout && tag_cardinality == OriginTagCardinality::None {
                trace!("Skipping origin enrichment for DogStatsD metric with cardinality 'none'.");
                return collected_tags;
            }

            // If we discovered an entity ID via origin detection, and no client-provided entity ID was provided (or it
            // was, but entity ID precedence is disabled), then try to get tags for the detected entity ID.
            if let Some(entity_id) = maybe_process_id {
                if maybe_local_pod_uid.is_none() || !self.config.entity_id_precedence {
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

            // If we have a client-provided entity ID, try to get tags for the entity based on those. A client-provided
            // entity ID takes precedence over the container ID.
            let maybe_entity_id = maybe_local_pod_uid.or(maybe_local_container_id);
            if let Some(entity_id) = maybe_entity_id {
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

            // Evaluate all available entity IDs in order of priority: Local Data-based container ID, process ID,
            // External Data-based container ID, Local Data-based pod UID, and External Data-based pod UID.
            //
            // As soon as the first set of tags for an entity ID is found, we skip the remaining entity IDs.
            let maybe_entity_ids = &[
                maybe_local_container_id,
                maybe_process_id,
                maybe_external_container_id,
                maybe_local_pod_uid,
                maybe_external_pod_uid,
            ];
            for entity_id in maybe_entity_ids.iter().flatten() {
                if let Some(tags) = self.workload_provider.get_tags_for_entity(entity_id, tag_cardinality) {
                    if !tags.is_empty() {
                        collected_tags.extend_from_shared(&tags);
                        break;
                    }
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
pub fn origin_from_metric_packet<'packet>(
    packet: &MetricPacket<'packet>, well_known_tags: &WellKnownTags<'packet>,
) -> RawOrigin<'packet> {
    let cardinality = packet.cardinality.or(well_known_tags.cardinality);

    let mut origin = RawOrigin::default();
    origin.set_pod_uid(well_known_tags.pod_uid);
    origin.set_container_id(packet.container_id);
    origin.set_external_data(packet.external_data);
    origin.set_cardinality(cardinality);
    origin
}

/// Builds an `RawOrigin` object from the given event packet.
pub fn origin_from_event_packet<'packet>(
    packet: &EventPacket<'packet>, well_known_tags: &WellKnownTags<'packet>,
) -> RawOrigin<'packet> {
    let cardinality = packet.cardinality.or(well_known_tags.cardinality);

    let mut origin = RawOrigin::default();
    origin.set_pod_uid(well_known_tags.pod_uid);
    origin.set_container_id(packet.container_id);
    origin.set_external_data(packet.external_data);
    origin.set_cardinality(cardinality);
    origin
}

/// Builds an `RawOrigin` object from the given service check packet.
pub fn origin_from_service_check_packet<'packet>(
    packet: &ServiceCheckPacket<'packet>, well_known_tags: &WellKnownTags<'packet>,
) -> RawOrigin<'packet> {
    let cardinality = packet.cardinality.or(well_known_tags.cardinality);

    let mut origin = RawOrigin::default();
    origin.set_pod_uid(well_known_tags.pod_uid);
    origin.set_container_id(packet.container_id);
    origin.set_external_data(packet.external_data);
    origin.set_cardinality(cardinality);
    origin
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use saluki_context::tags::{RawTags, TagSet};
    use saluki_core::data_model::event::{metric::MetricValues, service_check::CheckStatus};
    use saluki_env::workload::{origin::ResolvedExternalData, EntityId};
    use stringtheory::MetaString;

    use super::*;

    static EID_PID: EntityId = EntityId::ContainerPid(12345);
    static EID_LOCAL_CID: EntityId = EntityId::Container(MetaString::from_static("local-cid"));
    static EID_EXTERNAL_CID_VALID: EntityId = EntityId::Container(MetaString::from_static("external-cid"));
    static EID_EXTERNAL_CID_INVALID: EntityId = EntityId::Container(MetaString::from_static("invalid-external-cid"));
    static EID_LOCAL_POD: EntityId = EntityId::PodUid(MetaString::from_static("local-pod-uid"));
    static EID_EXTERNAL_POD: EntityId = EntityId::PodUid(MetaString::from_static("external-pod-uid"));

    #[derive(Default)]
    struct MockWorkloadProvider {
        tags: HashMap<EntityId, SharedTagSet>,
    }

    impl MockWorkloadProvider {
        fn add_tags(&mut self, entity_id: EntityId, tags: SharedTagSet) {
            self.tags.insert(entity_id, tags);
        }
    }

    impl WorkloadProvider for MockWorkloadProvider {
        fn get_tags_for_entity(&self, entity_id: &EntityId, _: OriginTagCardinality) -> Option<SharedTagSet> {
            self.tags.get(entity_id).cloned()
        }

        fn get_resolved_origin(&self, _: RawOrigin<'_>) -> Option<ResolvedOrigin> {
            // We don't use this for our tests.
            todo!()
        }
    }

    fn single_tag(tag: &str) -> SharedTagSet {
        let mut tag_set = TagSet::default();
        tag_set.insert_tag(tag);
        tag_set.into_shared()
    }

    fn tags_for_entity(entity_id: &EntityId) -> SharedTagSet {
        if entity_id == &EID_PID {
            single_tag("tag_source:pid")
        } else if entity_id == &EID_LOCAL_CID {
            single_tag("tag_source:local-cid")
        } else if entity_id == &EID_EXTERNAL_CID_VALID {
            single_tag("tag_source:external-cid")
        } else if entity_id == &EID_LOCAL_POD {
            single_tag("tag_source:local-pod")
        } else if entity_id == &EID_EXTERNAL_POD {
            single_tag("tag_source:external-pod")
        } else {
            SharedTagSet::default()
        }
    }

    fn origin(
        maybe_process_id: Option<&EntityId>, maybe_local_container_id: Option<&EntityId>,
        maybe_local_pod_uid: Option<&EntityId>, maybe_external_data: Option<&ResolvedExternalData>,
    ) -> ResolvedOrigin {
        ResolvedOrigin::from_parts(
            None,
            maybe_process_id.cloned(),
            maybe_local_container_id.cloned(),
            maybe_local_pod_uid.cloned(),
            maybe_external_data.cloned(),
        )
    }

    fn build_tags_resolver_with_default_tags(config: OriginEnrichmentConfiguration) -> DogStatsDOriginTagResolver {
        let mut workload_provider = MockWorkloadProvider::default();
        workload_provider.add_tags(EID_PID.clone(), tags_for_entity(&EID_PID));
        workload_provider.add_tags(EID_LOCAL_CID.clone(), tags_for_entity(&EID_LOCAL_CID));
        workload_provider.add_tags(EID_EXTERNAL_CID_VALID.clone(), tags_for_entity(&EID_EXTERNAL_CID_VALID));
        workload_provider.add_tags(EID_LOCAL_POD.clone(), tags_for_entity(&EID_LOCAL_POD));
        workload_provider.add_tags(EID_EXTERNAL_POD.clone(), tags_for_entity(&EID_EXTERNAL_POD));

        let erased_workload_provider = Arc::new(workload_provider);

        DogStatsDOriginTagResolver::new(config, erased_workload_provider)
    }

    #[test]
    fn metric_cardinality_precedence() {
        // Tests that the cardinality specified in a metric packet (`|card:high`, etc) takes precedence over the cardinality
        // specified via the deprecated `dd.internal.card` tag.
        let raw_tags_input = "dd.internal.card:high";
        let raw_tags = RawTags::new(raw_tags_input, usize::MAX, usize::MAX);

        let well_known_tags = WellKnownTags::from_raw_tags(raw_tags.clone());
        assert_eq!(well_known_tags.cardinality, Some(OriginTagCardinality::High));

        let packet_with_card = MetricPacket {
            metric_name: "test_metric",
            tags: raw_tags.clone(),
            values: MetricValues::counter(1.0),
            num_points: 1,
            timestamp: None,
            container_id: None,
            external_data: None,
            cardinality: Some(OriginTagCardinality::Low),
        };

        let packet_without_card = MetricPacket {
            metric_name: "test_metric",
            tags: raw_tags.clone(),
            values: MetricValues::counter(1.0),
            num_points: 1,
            timestamp: None,
            container_id: None,
            external_data: None,
            cardinality: None,
        };

        let with_card_origin = origin_from_metric_packet(&packet_with_card, &well_known_tags);
        assert_ne!(packet_with_card.cardinality, well_known_tags.cardinality);
        assert_eq!(with_card_origin.cardinality(), packet_with_card.cardinality);

        let without_card_origin = origin_from_metric_packet(&packet_without_card, &well_known_tags);
        assert_ne!(packet_without_card.cardinality, well_known_tags.cardinality);
        assert_eq!(without_card_origin.cardinality(), well_known_tags.cardinality);
    }

    #[test]
    fn event_cardinality_precedence() {
        // Tests that the cardinality specified in an event packet (`|card:high`, etc) takes precedence over the cardinality
        // specified via the deprecated `dd.internal.card` tag.
        let raw_tags_input = "dd.internal.card:low";
        let raw_tags = RawTags::new(raw_tags_input, usize::MAX, usize::MAX);

        let well_known_tags = WellKnownTags::from_raw_tags(raw_tags.clone());
        assert_eq!(well_known_tags.cardinality, Some(OriginTagCardinality::Low));

        let packet_with_card = EventPacket {
            title: MetaString::empty(),
            text: MetaString::empty(),
            timestamp: None,
            hostname: None,
            aggregation_key: None,
            priority: None,
            alert_type: None,
            source_type_name: None,
            tags: raw_tags.clone(),
            container_id: None,
            external_data: None,
            cardinality: Some(OriginTagCardinality::Orchestrator),
        };

        let packet_without_card = EventPacket {
            title: MetaString::empty(),
            text: MetaString::empty(),
            timestamp: None,
            hostname: None,
            aggregation_key: None,
            priority: None,
            alert_type: None,
            source_type_name: None,
            tags: raw_tags.clone(),
            container_id: None,
            external_data: None,
            cardinality: None,
        };

        let with_card_origin = origin_from_event_packet(&packet_with_card, &well_known_tags);
        assert_ne!(packet_with_card.cardinality, well_known_tags.cardinality);
        assert_eq!(with_card_origin.cardinality(), packet_with_card.cardinality);

        let without_card_origin = origin_from_event_packet(&packet_without_card, &well_known_tags);
        assert_ne!(packet_without_card.cardinality, well_known_tags.cardinality);
        assert_eq!(without_card_origin.cardinality(), well_known_tags.cardinality);
    }

    #[test]
    fn service_check_cardinality_precedence() {
        // Tests that the cardinality specified in an event packet (`|card:high`, etc) takes precedence over the cardinality
        // specified via the deprecated `dd.internal.card` tag.
        let raw_tags_input = "dd.internal.card:orchestrator";
        let raw_tags = RawTags::new(raw_tags_input, usize::MAX, usize::MAX);

        let well_known_tags = WellKnownTags::from_raw_tags(raw_tags.clone());
        assert_eq!(well_known_tags.cardinality, Some(OriginTagCardinality::Orchestrator));

        let packet_with_card = ServiceCheckPacket {
            name: MetaString::empty(),
            status: CheckStatus::Ok,
            timestamp: None,
            hostname: None,
            message: None,
            tags: raw_tags.clone(),
            container_id: None,
            external_data: None,
            cardinality: Some(OriginTagCardinality::Low),
        };

        let packet_without_card = ServiceCheckPacket {
            name: MetaString::empty(),
            status: CheckStatus::Ok,
            timestamp: None,
            hostname: None,
            message: None,
            tags: raw_tags.clone(),
            container_id: None,
            external_data: None,
            cardinality: None,
        };

        let with_card_origin = origin_from_service_check_packet(&packet_with_card, &well_known_tags);
        assert_ne!(packet_with_card.cardinality, well_known_tags.cardinality);
        assert_eq!(with_card_origin.cardinality(), packet_with_card.cardinality);

        let without_card_origin = origin_from_service_check_packet(&packet_without_card, &well_known_tags);
        assert_ne!(packet_without_card.cardinality, well_known_tags.cardinality);
        assert_eq!(without_card_origin.cardinality(), well_known_tags.cardinality);
    }

    #[test]
    fn origin_detection_legacy_precedence() {
        let mut pid_plus_local_pod_tags = tags_for_entity(&EID_PID);
        pid_plus_local_pod_tags.extend_from_shared(&tags_for_entity(&EID_LOCAL_POD));

        let mut pid_plus_local_cid_tags = tags_for_entity(&EID_PID);
        pid_plus_local_cid_tags.extend_from_shared(&tags_for_entity(&EID_LOCAL_CID));

        let cases = [
            // We only have the process ID, so entity ID precedence should be irrelevant.
            (
                false,
                origin(Some(&EID_PID), None, None, None),
                tags_for_entity(&EID_PID),
            ),
            (
                true,
                origin(Some(&EID_PID), None, None, None),
                tags_for_entity(&EID_PID),
            ),
            // We have both the process ID and local pod UID, but entity ID precedence is disabled, so we get the
            // process ID and local pod UID tags.
            (
                false,
                origin(Some(&EID_PID), None, Some(&EID_LOCAL_POD), None),
                pid_plus_local_pod_tags.clone(),
            ),
            // We have both the process ID and local pod UID, but entity ID precedence is enabled, so we should only get
            // the local pod UID tags.
            (
                true,
                origin(Some(&EID_PID), None, Some(&EID_LOCAL_POD), None),
                tags_for_entity(&EID_LOCAL_POD),
            ),
            // We have the process ID, local container ID, and local pod UID, but entity ID precedence is disabled, so
            // we should get the process ID and local pod UID tags.
            (
                false,
                origin(Some(&EID_PID), Some(&EID_LOCAL_CID), Some(&EID_LOCAL_POD), None),
                pid_plus_local_pod_tags,
            ),
            // We have the process ID, local container ID, and local pod UID, but entity ID precedence is enabled, so we
            // should only get the local pod UID tags.
            (
                true,
                origin(Some(&EID_PID), Some(&EID_LOCAL_CID), Some(&EID_LOCAL_POD), None),
                tags_for_entity(&EID_LOCAL_POD),
            ),
            // We only have the process ID and local container ID, so entity ID precedence should be irrelevant, and so
            // we should get the process ID and local container ID tags.
            (
                false,
                origin(Some(&EID_PID), Some(&EID_LOCAL_CID), None, None),
                pid_plus_local_cid_tags.clone(),
            ),
            (
                true,
                origin(Some(&EID_PID), Some(&EID_LOCAL_CID), None, None),
                pid_plus_local_cid_tags,
            ),
        ];

        for (entity_id_precedence, resolved_origin, expected_tags) in cases {
            let tag_resolver_config = OriginEnrichmentConfiguration {
                entity_id_precedence,
                tag_cardinality: OriginTagCardinality::High,
                origin_detection_unified: false,
                origin_detection_optout: false,
            };

            let origin_tags_resolver = build_tags_resolver_with_default_tags(tag_resolver_config);

            let actual_tags = origin_tags_resolver.collect_origin_tags(resolved_origin.clone());
            assert_eq!(
                actual_tags, expected_tags,
                "failed to resolve the expected tags for origin {:?}",
                resolved_origin
            );
        }
    }

    #[test]
    fn origin_detection_unified_precedence() {
        // We craft a "valid" and "invalid" variant for External Data, where the invalid one has a container ID with no tags
        // assigned to it, which lets us exercise the tags resolver logic for when we have a pod UID through External Data,
        // but not a container ID (or a container ID with no tags attached).
        //
        // We have to do it this way because we pass both External Data-based entity IDs through `ResolvedExternalData` when
        // creating `ResolvedOrigin`, so we can't pass them separately.
        let ext_data_valid = ResolvedExternalData::new(EID_EXTERNAL_POD.clone(), EID_EXTERNAL_CID_VALID.clone());
        let ext_data_invalid = ResolvedExternalData::new(EID_EXTERNAL_POD.clone(), EID_EXTERNAL_CID_INVALID.clone());

        let tag_resolver_config = OriginEnrichmentConfiguration {
            entity_id_precedence: false,
            tag_cardinality: OriginTagCardinality::High,
            origin_detection_unified: true,
            origin_detection_optout: false,
        };

        let origin_tags_resolver = build_tags_resolver_with_default_tags(tag_resolver_config);

        // We craft our test cases to ensure that we always take the tags of the highest precedence entity ID available,
        // and don't take any other tags.
        let cases = [
            // Cases where we're only setting a single entity ID. This is the happy path.
            (origin(Some(&EID_PID), None, None, None), tags_for_entity(&EID_PID)),
            (
                origin(None, Some(&EID_LOCAL_CID), None, None),
                tags_for_entity(&EID_LOCAL_CID),
            ),
            (
                origin(None, None, Some(&EID_LOCAL_POD), None),
                tags_for_entity(&EID_LOCAL_POD),
            ),
            (
                origin(None, None, None, Some(&ext_data_valid)),
                tags_for_entity(&EID_EXTERNAL_CID_VALID),
            ),
            (
                origin(None, None, None, Some(&ext_data_invalid)),
                tags_for_entity(&EID_EXTERNAL_POD),
            ),
            // Cases where we have multiple entity IDs to choose from. We work our way backwards here.
            (
                origin(
                    Some(&EID_PID),
                    Some(&EID_LOCAL_CID),
                    Some(&EID_LOCAL_POD),
                    Some(&ext_data_valid),
                ),
                tags_for_entity(&EID_LOCAL_CID),
            ),
            (
                origin(Some(&EID_PID), None, Some(&EID_LOCAL_POD), Some(&ext_data_valid)),
                tags_for_entity(&EID_PID),
            ),
            (
                origin(None, None, Some(&EID_LOCAL_POD), Some(&ext_data_valid)),
                tags_for_entity(&EID_EXTERNAL_CID_VALID),
            ),
            (
                origin(None, None, Some(&EID_LOCAL_POD), Some(&ext_data_invalid)),
                tags_for_entity(&EID_LOCAL_POD),
            ),
            (
                origin(None, None, None, Some(&ext_data_invalid)),
                tags_for_entity(&EID_EXTERNAL_POD),
            ),
        ];

        for (resolved_origin, expected_tags) in cases {
            let actual_tags = origin_tags_resolver.collect_origin_tags(resolved_origin.clone());
            assert_eq!(
                actual_tags, expected_tags,
                "failed to resolve the expected tags for origin {:?}",
                resolved_origin
            );
        }
    }
}
