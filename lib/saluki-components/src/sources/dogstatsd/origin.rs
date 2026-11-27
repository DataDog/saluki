use std::sync::Arc;

use saluki_core::data_model::{
    origin::{OriginTagCardinality, RawOrigin},
    tags::SharedTagSet,
};
use saluki_env::{
    workload::{origin::ResolvedOrigin, EntityId},
    WorkloadProvider,
};
use tracing::trace;

use super::{replay::CapturedTaggerHandle, tags::WellKnownTags};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ProcessOrigin {
    Pinned(Option<EntityId>),
    Unpinned(u32),
    Replay(u32),
}

impl ProcessOrigin {
    pub(super) fn container_entity_id(&self) -> Option<&EntityId> {
        match self {
            Self::Pinned(entity_id) => entity_id.as_ref(),
            Self::Unpinned(_) | Self::Replay(_) => None,
        }
    }
}

/// Origin enrichment configuration.
///
/// Origin enrichment controls the when and how of enriching metrics ingested via DogStatsD based on various sources of
/// "origin" information, such as specific metric tags or UDS socket credentials. Enrichment involves adding additional
/// metric tags that describe the origin of the metric, such as the Kubernetes pod or container.
#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub struct OriginEnrichmentConfiguration {
    /// Whether or not to enable origin detection.
    ///
    /// If disabled, no origin tags will be added to events even if the origin information is detected.
    pub enabled: bool,

    /// Whether or not a client-provided entity ID should take precedence over automatically detected origin metadata.
    ///
    /// When a client-provided entity ID is specified, and an origin process ID has automatically been detected, setting
    /// this to `true` will cause the origin process ID to be ignored. This only applies when
    /// `origin_detection_unified` is `false`.
    pub entity_id_precedence: bool,

    /// The cardinality of tags to enrich metrics with, when the payload doesn't specify one itself.
    pub tag_cardinality: OriginTagCardinality,

    /// Whether or not to use the unified origin detection behavior.
    ///
    /// When set to `true`, all detected entity IDs -- UDS Origin Detection, `dd.internal.entity_id`, container ID from
    /// DogStatsD payload -- will be used for querying tags to enrich with. When set to `false`, the original precedence
    /// behavior will be used, which enriches with the entity ID detected via Origin Detection first, and then
    /// potentially again with either the client-provided entity ID (`dd.internal.entity_id`) or the container ID from
    /// the DogStatsD payload, with the client-provided entity ID taking precedence. An entity ID detected via Origin
    /// Detection is only used when no client-provided entity ID was present, or when `entity_id_precedence` is
    /// `false`.
    pub origin_detection_unified: bool,

    /// Whether or not to opt out of origin detection for DogStatsD metrics.
    ///
    /// When set to `true`, and the metric explicitly denotes a cardinality of `"none"`, origin enrichment will be
    /// skipped. This is only applicable to DogStatsD metrics when unified origin detection behavior isn't enabled.
    pub origin_detection_optout: bool,

    /// Whether or not to parse client-provided origin fields from DogStatsD payloads.
    ///
    /// When enabled, the `c:` (Local Data), `e:` (External Data), and `card:` (Cardinality) protocol fields are
    /// parsed and used for origin enrichment.
    pub origin_detection_client: bool,
}

#[cfg(test)]
impl OriginEnrichmentConfiguration {
    /// Creates a fixture configuration with origin detection disabled, for tests that exercise enrichment behavior
    /// rather than configuration.
    pub(super) fn for_test() -> Self {
        Self {
            enabled: false,
            entity_id_precedence: false,
            tag_cardinality: OriginTagCardinality::Low,
            origin_detection_unified: false,
            origin_detection_optout: true,
            origin_detection_client: false,
        }
    }
}

#[derive(Clone)]
pub(super) struct DogStatsDOriginTagResolver {
    config: OriginEnrichmentConfiguration,
    workload_provider: Arc<dyn WorkloadProvider + Send + Sync>,
    captured_tagger: CapturedTaggerHandle,
}

impl DogStatsDOriginTagResolver {
    pub fn new(
        config: OriginEnrichmentConfiguration, workload_provider: Arc<dyn WorkloadProvider + Send + Sync>,
        captured_tagger: CapturedTaggerHandle,
    ) -> Self {
        Self {
            config,
            workload_provider,
            captured_tagger,
        }
    }

    fn collect_origin_tags_with_process_entity(
        &self, origin: &ResolvedOrigin, process_entity_id: Option<&EntityId>,
    ) -> SharedTagSet {
        let mut collected_tags = SharedTagSet::default();

        if !self.config.enabled {
            return collected_tags;
        }

        // Examine the various possible entity ID values, and based on their state, use one or more of them to grab any
        // enriched tags attached to the entities. We evalulate a number of possible entity IDs:
        //
        // - sender entity (resolved from UDS socket credentials when the packet is received)
        // - Local Data-based container ID (extracted from special "container ID" extension in DogStatsD protocol; also known as Local Data)
        // - Local Data-based pod UID (extracted from `dd.internal.entity_id` tag)
        // - External Data-based container ID (derived from pod UID and container name in External Data)
        // - External Data-based pod UID (raw pod UID from External Data)
        let maybe_process_entity_id = process_entity_id;
        let maybe_local_container_id = origin.local_data();
        let maybe_local_pod_uid = origin.pod_uid();
        let maybe_external_container_id = origin.resolved_external_data().map(|red| red.container_entity_id());
        let maybe_external_pod_uid = origin.resolved_external_data().map(|red| red.pod_entity_id());

        let tag_cardinality = origin.cardinality().unwrap_or(self.config.tag_cardinality);

        if !self.config.origin_detection_unified {
            if self.config.origin_detection_optout && tag_cardinality == OriginTagCardinality::None {
                trace!("Skipping origin enrichment for DogStatsD metric with cardinality 'none'.");
                return collected_tags;
            }

            // If we discovered an entity ID from socket credentials, and no client-provided entity ID was provided (or
            // it was, but entity ID precedence is disabled), then try to get tags for the detected entity ID.
            if let Some(entity_id) = maybe_process_entity_id {
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

            // Evaluate all available entity IDs in order of priority: Local Data-based container ID, sender entity,
            // External Data-based container ID, Local Data-based pod UID, and External Data-based pod UID.
            //
            // As soon as the first set of tags for an entity ID is found, we skip the remaining entity IDs.
            let maybe_entity_ids = &[
                maybe_local_container_id,
                maybe_process_entity_id,
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

    fn collect_origin_tags(&self, origin: &ResolvedOrigin) -> SharedTagSet {
        self.collect_origin_tags_with_process_entity(origin, origin.process_id())
    }

    /// Resolves the origin tags of a packet, using the sender identity pinned when the packet was received.
    pub(super) fn resolve_origin_tags(
        &self, mut origin: RawOrigin<'_>, process_origin: Option<&ProcessOrigin>,
    ) -> SharedTagSet {
        match process_origin {
            Some(ProcessOrigin::Replay(process_id)) => self.resolve_replay_origin_tags(*process_id, &origin),
            Some(ProcessOrigin::Unpinned(process_id)) => {
                origin.set_process_id(*process_id);
                self.resolve_live_origin_tags(origin)
            }
            Some(ProcessOrigin::Pinned(process_entity_id)) => {
                let resolved_origin = self
                    .workload_provider
                    .get_resolved_origin(origin.clone())
                    .unwrap_or_else(|| ResolvedOrigin::from_parts(origin.cardinality(), None, None, None, None));
                self.collect_origin_tags_with_process_entity(&resolved_origin, process_entity_id.as_ref())
            }
            None => self.resolve_live_origin_tags(origin),
        }
    }

    /// Resolves the origin tags of replayed traffic from the captured tagger state.
    ///
    /// The captured state already holds fully resolved tags per entity, so this bypasses live enrichment entirely.
    fn resolve_replay_origin_tags(&self, captured_process_id: u32, origin: &RawOrigin<'_>) -> SharedTagSet {
        let Some(store) = self.captured_tagger.current() else {
            trace!(?origin, "Replayed origin but no captured tagger available.");
            return SharedTagSet::default();
        };
        let cardinality = origin.cardinality().unwrap_or(self.config.tag_cardinality);
        store
            .lookup(captured_process_id as i32, cardinality)
            .unwrap_or_default()
    }

    fn resolve_live_origin_tags(&self, origin: RawOrigin<'_>) -> SharedTagSet {
        match self.workload_provider.get_resolved_origin(origin.clone()) {
            Some(resolved_origin) => self.collect_origin_tags(&resolved_origin),
            None => {
                trace!(?origin, "No resolved origin found for origin.");
                SharedTagSet::default()
            }
        }
    }
}

/// Builds a `RawOrigin` from the origin fields of a DogStatsD packet and its well-known tags.
///
/// Metric, event, and service check packets all carry the same origin fields.
pub fn origin_from_packet<'a>(
    local_data: Option<&'a str>, external_data: Option<&'a str>, cardinality: Option<OriginTagCardinality>,
    well_known_tags: &WellKnownTags<'a>,
) -> RawOrigin<'a> {
    let cardinality = cardinality.or(well_known_tags.cardinality);

    let mut origin = RawOrigin::default();
    origin.set_pod_uid(well_known_tags.pod_uid);
    origin.set_local_data(local_data);
    origin.set_external_data(external_data);
    origin.set_cardinality(cardinality);
    origin
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use saluki_core::data_model::tags::{RawTags, TagSet};
    use saluki_env::workload::{origin::ResolvedExternalData, providers::TestWorkloadProvider, EntityId};
    use stringtheory::MetaString;

    use super::*;

    static EID_PID: EntityId = EntityId::ContainerPid(12345);
    static EID_LOCAL_CID: EntityId = EntityId::Container(MetaString::from_static("local-cid"));
    static EID_EXTERNAL_CID_VALID: EntityId = EntityId::Container(MetaString::from_static("external-cid"));
    static EID_EXTERNAL_CID_INVALID: EntityId = EntityId::Container(MetaString::from_static("invalid-external-cid"));
    static EID_LOCAL_POD: EntityId = EntityId::PodUid(MetaString::from_static("local-pod-uid"));
    static EID_EXTERNAL_POD: EntityId = EntityId::PodUid(MetaString::from_static("external-pod-uid"));

    /// The tag the default workload provider holds for each entity. `EID_EXTERNAL_CID_INVALID` has none.
    static ENTITY_TAGS: [(&EntityId, &str); 5] = [
        (&EID_PID, "tag_source:pid"),
        (&EID_LOCAL_CID, "tag_source:local-cid"),
        (&EID_EXTERNAL_CID_VALID, "tag_source:external-cid"),
        (&EID_LOCAL_POD, "tag_source:local-pod"),
        (&EID_EXTERNAL_POD, "tag_source:external-pod"),
    ];

    fn single_tag(tag: &str) -> SharedTagSet {
        let mut tag_set = TagSet::default();
        tag_set.insert_tag(tag);
        tag_set.into_shared()
    }

    fn tags_for_entity(entity_id: &EntityId) -> SharedTagSet {
        ENTITY_TAGS
            .iter()
            .find(|(id, _)| *id == entity_id)
            .map(|(_, tag)| single_tag(tag))
            .unwrap_or_default()
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
        let mut workload_provider = TestWorkloadProvider::new();
        for (entity_id, tag) in ENTITY_TAGS {
            workload_provider.add_entity_shared_tags(entity_id.clone(), single_tag(tag));
        }

        DogStatsDOriginTagResolver::new(config, Arc::new(workload_provider), CapturedTaggerHandle::new())
    }

    /// Builds a resolver with origin detection enabled at `tag_cardinality`.
    fn enabled_resolver(
        tag_cardinality: OriginTagCardinality, workload_provider: TestWorkloadProvider,
        captured_tagger: CapturedTaggerHandle,
    ) -> DogStatsDOriginTagResolver {
        let config = OriginEnrichmentConfiguration {
            enabled: true,
            tag_cardinality,
            ..OriginEnrichmentConfiguration::for_test()
        };
        DogStatsDOriginTagResolver::new(config, Arc::new(workload_provider), captured_tagger)
    }

    #[test]
    fn packet_cardinality_takes_precedence_over_cardinality_tag() {
        // The cardinality specified in a packet (`|card:high`, etc) takes precedence over the cardinality specified via
        // the deprecated `dd.internal.card` tag.
        let cases = [
            (
                "dd.internal.card:high",
                OriginTagCardinality::High,
                OriginTagCardinality::Low,
            ),
            (
                "dd.internal.card:low",
                OriginTagCardinality::Low,
                OriginTagCardinality::Orchestrator,
            ),
            (
                "dd.internal.card:orchestrator",
                OriginTagCardinality::Orchestrator,
                OriginTagCardinality::Low,
            ),
        ];

        for (tag, tag_cardinality, packet_cardinality) in cases {
            let raw_tags = RawTags::new(tag, usize::MAX, usize::MAX);
            let (well_known_tags, _) = WellKnownTags::split_raw_tags(&raw_tags);
            assert_eq!(well_known_tags.cardinality, Some(tag_cardinality), "{tag}");
            assert_ne!(packet_cardinality, tag_cardinality, "{tag}");

            assert_eq!(
                origin_from_packet(None, None, Some(packet_cardinality), &well_known_tags).cardinality(),
                Some(packet_cardinality),
                "{tag} with packet cardinality"
            );
            assert_eq!(
                origin_from_packet(None, None, None, &well_known_tags).cardinality(),
                Some(tag_cardinality),
                "{tag} without packet cardinality"
            );
        }
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
                enabled: true,
                entity_id_precedence,
                tag_cardinality: OriginTagCardinality::High,
                origin_detection_unified: false,
                origin_detection_optout: false,
                origin_detection_client: false,
            };

            let origin_tags_resolver = build_tags_resolver_with_default_tags(tag_resolver_config);

            let actual_tags = origin_tags_resolver.collect_origin_tags(&resolved_origin);
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
            enabled: true,
            entity_id_precedence: false,
            tag_cardinality: OriginTagCardinality::High,
            origin_detection_unified: true,
            origin_detection_optout: false,
            origin_detection_client: false,
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
            let actual_tags = origin_tags_resolver.collect_origin_tags(&resolved_origin);
            assert_eq!(
                actual_tags, expected_tags,
                "failed to resolve the expected tags for origin {:?}",
                resolved_origin
            );
        }
    }

    #[test]
    fn origin_detection_disabled() {
        // When origin detection is disabled, no tags should be resolved even if we do have mapped tags for the given
        // resolved origin.
        let tag_resolver_config = OriginEnrichmentConfiguration::for_test();
        assert!(!tag_resolver_config.enabled);

        let origin_tags_resolver = build_tags_resolver_with_default_tags(tag_resolver_config);

        let resolved_origin = origin(Some(&EID_PID), None, None, None);
        let actual_tags = origin_tags_resolver.collect_origin_tags(&resolved_origin);
        assert!(actual_tags.is_empty());
    }

    #[test]
    fn replayed_origin_tags_come_from_the_captured_store() {
        use datadog_protos::agent::{Entity as ProtoEntity, TaggerState};

        // Build a captured store with a single entity keyed by PID 7777.
        let mut entities = HashMap::new();
        entities.insert(
            "container_id://captured-container".to_string(),
            ProtoEntity {
                low_cardinality_tags: vec!["env:captured".into(), "service:replayed".into()],
                ..Default::default()
            },
        );
        let mut pid_map = HashMap::new();
        pid_map.insert(7777, "container_id://captured-container".to_string());
        let state = TaggerState {
            state: entities,
            pid_map,
            duration: 0,
        };
        let captured_tagger = CapturedTaggerHandle::new();
        captured_tagger.set_current(Some(super::super::replay::CapturedTaggerStore::from_tagger_state(
            state,
        )));

        // Build a resolver whose live workload provider is empty; if the replay path is wired wrong, we'd get
        // no tags. If it's wired right, we get the captured tags.
        let resolver = enabled_resolver(OriginTagCardinality::Low, TestWorkloadProvider::new(), captured_tagger);

        let mut origin = RawOrigin::default();
        origin.set_cardinality(OriginTagCardinality::Low);

        let tags = resolver.resolve_origin_tags(origin, Some(&ProcessOrigin::Replay(7777)));
        let tag_strs: Vec<String> = tags.into_iter().map(|t| t.as_str().to_string()).collect();
        assert!(tag_strs.contains(&"env:captured".to_string()));
        assert!(tag_strs.contains(&"service:replayed".to_string()));
    }

    #[test]
    fn replayed_origin_tags_are_empty_without_a_captured_store() {
        // With no captured store current, replayed traffic gets no tags rather than falling back to the live tagger.
        let resolver = enabled_resolver(
            OriginTagCardinality::Low,
            TestWorkloadProvider::new(),
            CapturedTaggerHandle::new(),
        );

        let tags = resolver.resolve_origin_tags(RawOrigin::default(), Some(&ProcessOrigin::Replay(7777)));
        assert!(
            tags.is_empty(),
            "replay path with no captured store must return empty tags"
        );
    }

    #[test]
    fn resolve_origin_tags_live_path_resolves_tags_via_workload_provider() {
        // The live path asks the workload provider to resolve the raw origin, then walks the resolved entity IDs to
        // collect tags.
        let mut workload_provider = TestWorkloadProvider::new();
        workload_provider.add_entity_shared_tags(EntityId::ContainerPid(4242), single_tag("tag_source:live-pid"));
        let resolver = enabled_resolver(
            OriginTagCardinality::High,
            workload_provider,
            CapturedTaggerHandle::new(),
        );

        // An unpinned process ID resolves to `EntityId::ContainerPid(4242)` via the live path.
        let tags = resolver.resolve_origin_tags(RawOrigin::default(), Some(&ProcessOrigin::Unpinned(4242)));
        assert_eq!(tags, single_tag("tag_source:live-pid"));
    }

    #[test]
    fn resolve_origin_tags_uses_pinned_process_entity() {
        let pinned_entity = EntityId::Container(MetaString::from_static("original-container"));
        let workload_provider =
            TestWorkloadProvider::with_entity(pinned_entity.clone(), &["tag_source:pinned-container"]);
        let resolver = enabled_resolver(
            OriginTagCardinality::High,
            workload_provider,
            CapturedTaggerHandle::new(),
        );
        let process_origin = ProcessOrigin::Pinned(Some(pinned_entity));

        let tags = resolver.resolve_origin_tags(RawOrigin::default(), Some(&process_origin));

        assert_eq!(tags, single_tag("tag_source:pinned-container"));
    }

    #[test]
    fn resolve_origin_tags_live_path_returns_empty_when_origin_unresolved() {
        // When the workload provider can't resolve the origin (here, an empty origin resolves to `None`), the live
        // path returns an empty tag set rather than falling through to anything else.
        let resolver = enabled_resolver(
            OriginTagCardinality::High,
            TestWorkloadProvider::new(),
            CapturedTaggerHandle::new(),
        );

        let tags = resolver.resolve_origin_tags(RawOrigin::default(), None);
        assert!(tags.is_empty(), "unresolved origin must produce no tags");
    }
}
