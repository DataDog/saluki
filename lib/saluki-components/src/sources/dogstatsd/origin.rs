use std::num::NonZeroU32;

use saluki_context::tags::ChainedTags;
use saluki_env::{workload::EntityId, WorkloadProvider};
use saluki_event::metric::OriginTagCardinality;
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
    W: WorkloadProvider,
{
    pub fn gather_origin_tags(&self, origin_entity_meta: &OriginEntityMetadata<'_>, tags_buf: &mut ChainedTags<'_>) {
        // Examine the various possible entity ID values, and based on their state, use one or more of them to enrich
        // the tags for the given metric. Below is a description of each entity ID we may have extracted:
        //
        // - entity ID (extracted from `dd.internal.entity_id` tag; non-prefixed pod UID)
        // - container ID (extracted from `saluki.internal.container_id` tag, which comes from special "container ID"
        //   extension in DogStatsD protocol; non-prefixed container ID)
        // - origin PID (extracted via UDS socket credentials)

        let maybe_entity_id = origin_entity_meta.pod_uid().and_then(EntityId::from_pod_uid);
        let maybe_container_id = origin_entity_meta
            .container_id()
            .and_then(EntityId::from_raw_container_id);
        let maybe_origin_pid = origin_entity_meta.process_id().map(EntityId::ContainerPid);

        let tag_cardinality = origin_entity_meta.cardinality().unwrap_or(self.tag_cardinality);

        if !self.origin_detection_unified {
            if self.origin_detection_optout && tag_cardinality == OriginTagCardinality::None {
                trace!("Skipping origin enrichment for DogStatsD metric with cardinality 'none'.");
                return;
            }

            // If we discovered an entity ID via origin detection, and no client-provided entity ID was provided (or it was,
            // but entity ID precedence is disabled), then try to get tags for the detected entity ID.
            if let Some(origin_pid) = maybe_origin_pid {
                if maybe_entity_id.is_none() || !self.entity_id_precedence {
                    match self.workload_provider.get_tags_for_entity(&origin_pid, tag_cardinality) {
                        Some(tags) => {
                            trace!(entity_id = ?origin_pid, tags_len = tags.len(), "Found tags for entity.");
                            tags_buf.push_tags(tags);
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
                        tags_buf.push_tags(tags);
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
            // then container ID, and finally the client-provided entity ID.
            let maybe_entity_ids = &[maybe_origin_pid, maybe_container_id, maybe_entity_id];
            for entity_id in maybe_entity_ids.iter().flatten() {
                match self.workload_provider.get_tags_for_entity(entity_id, tag_cardinality) {
                    Some(tags) => {
                        trace!(?entity_id, tags_len = tags.len(), "Found tags for entity.");
                        tags_buf.push_tags(tags);
                    }
                    None => trace!(?entity_id, "No tags found for entity."),
                }
            }

            // If the metric has External Data attached, try to resolve an entity ID from it and enrich the metric with
            // any tags attached to that entity ID.
            if let Some(external_data) = origin_entity_meta.external_data() {
                if let Some(entity_id) = self
                    .workload_provider
                    .resolve_entity_id_from_external_data(external_data)
                {
                    match self.workload_provider.get_tags_for_entity(&entity_id, tag_cardinality) {
                        Some(tags) => {
                            trace!(?entity_id, tags_len = tags.len(), "Found tags for entity.");
                            tags_buf.push_tags(tags);
                        }
                        None => trace!(?entity_id, "No tags found for entity."),
                    }
                }
            }
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

/// Information about the entity where a metric originated from.
///
/// Metrics contain metadata about their origin, in terms of the metric's _reason_ for existing: the metric was ingested
/// via DogStatsD, or was generated by an integration, and so on. However, there is also the concept of a metric
/// originating from a particular _entity_, such as a specific Kubernetes container. This relates directly to the
/// specific sender of the metric, which is used to enrich the metric with additional tags describing the origin entity.
///
/// The origin entity will generally be the process ID of the metric sender, or the container ID, both of which are then
/// generally mapped to the relevant information for the metric, such as the orchestrator-level tags for the
/// container/pod/deployment.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OriginEntityMetadata<'a> {
    /// Process ID of the sender.
    process_id: Option<NonZeroU32>,

    /// Container ID of the sender.
    ///
    /// This will generally be the typical long hexadecimal string that is used by container runtimes like `containerd`,
    /// but may sometimes also be a different form, such as the container's cgroups inode.
    container_id: Option<&'a str>,

    /// Pod UID of the sender.
    ///
    /// This is generally only used in Kubernetes environments to uniquely identify the pod. UIDs are equivalent to UUIDs.
    pod_uid: Option<&'a str>,

    /// Desired cardinality of any tags associated with the entity.
    ///
    /// This controls the cardinality of the tags added to this metric when enriching based on the available entity IDs.
    cardinality: Option<OriginTagCardinality>,

    /// External Data of the sender.
    ///
    /// See [`ExternalData`][saluki_env::workload::ExternalData] for more information.
    external_data: Option<&'a str>,
}

impl<'a> OriginEntityMetadata<'a> {
    /// Creates a new `OriginEntityMetadata` from a `MetricPacket`.
    pub fn from_metric_packet(packet: &MetricPacket<'a>) -> Self {
        Self {
            process_id: None,
            container_id: packet.container_id,
            pod_uid: packet.pod_uid,
            cardinality: packet.cardinality,
            external_data: packet.external_data,
        }
    }

    /// Sets the process ID of the sender.
    ///
    /// Must be a non-zero value. If the value is zero, it is silently ignored.
    pub fn set_process_id(&mut self, process_id: u32) {
        self.process_id = NonZeroU32::new(process_id);
    }

    /// Gets the process ID of the sender.
    pub fn process_id(&self) -> Option<u32> {
        self.process_id.map(NonZeroU32::get)
    }

    /// Gets the container ID of the sender.
    pub fn container_id(&self) -> Option<&str> {
        self.container_id
    }

    /// Gets the pod UID of the sender.
    pub fn pod_uid(&self) -> Option<&str> {
        self.pod_uid
    }

    /// Gets the desired cardinality of any tags associated with the entity.
    pub fn cardinality(&self) -> Option<OriginTagCardinality> {
        self.cardinality.as_ref().copied()
    }

    /// Gets the external data of the sender.
    pub fn external_data(&self) -> Option<&str> {
        self.external_data
    }
}
