use async_trait::async_trait;
use datadog_agent_commons::ipc::client::RemoteAgentClient;
use datadog_protos::agent::{EntityId as RemoteEntityId, EventType, TagCardinality as RemoteTagCardinality};
use futures::{StreamExt as _, TryStreamExt as _};
use saluki_config::GenericConfiguration;
use saluki_context::{
    origin::OriginTagCardinality,
    tags::{Tag, TagSet},
};
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::health::Health;
use saluki_env::workload::{collectors::MetadataCollector, EntityId, MetadataAction, MetadataOperation};
use saluki_error::GenericError;
use saluki_io::net::util::tonic::StatusError;
use saluki_metrics::{static_metrics, Counter};
use stringtheory::{
    interning::{GenericMapInterner, Interner as _},
    MetaString,
};
use tokio::{select, sync::mpsc};
use tracing::{debug, trace, warn};

#[static_metrics(prefix = remote_tagger_metadata_collector)]
#[derive(Clone)]
struct Telemetry {
    rpc_errors_total: Counter,
    intern_failed_total: Counter,
    events_added_total: Counter,
    events_modified_total: Counter,
    events_deleted_total: Counter,
}

/// A workload provider that uses the remote tagger API from a Datadog Agent to provide workload information.
pub struct RemoteAgentTaggerMetadataCollector {
    client: RemoteAgentClient,
    interner: GenericMapInterner,
    health: Health,
    telemetry: Telemetry,
}

impl RemoteAgentTaggerMetadataCollector {
    /// Creates a new `RemoteAgentTaggerMetadataCollector` from the given configuration.
    ///
    /// ## Errors
    ///
    /// If the Agent gRPC client can't be created (invalid API endpoint, missing authentication token, etc), or if the
    /// authentication token is invalid, an error will be returned.
    pub async fn from_configuration(
        config: &GenericConfiguration, health: Health, interner: GenericMapInterner,
    ) -> Result<Self, GenericError> {
        let client = RemoteAgentClient::from_configuration(config).await?;

        Ok(Self {
            client,
            interner,
            health,
            telemetry: Telemetry::new(),
        })
    }

    fn try_intern(&self, value: &str) -> Option<MetaString> {
        match self.interner.try_intern(value) {
            Some(interned) => Some(MetaString::from(interned)),
            None => {
                self.telemetry.intern_failed_total().increment(1);
                None
            }
        }
    }

    fn owned_tags_into_tagset(&self, tags: Vec<String>) -> Option<TagSet> {
        // We'll either inline the tags if they're short enough, otherwise we intern them.
        let mut new_tags = Vec::with_capacity(tags.len());
        for tag in tags {
            let new_tag = match MetaString::try_inline(&tag) {
                Some(s) => Tag::from(s),
                None => {
                    let interned = self.try_intern(&tag)?;
                    Tag::from(interned)
                }
            };

            new_tags.push(new_tag);
        }

        Some(TagSet::from_iter(new_tags))
    }

    fn track_event(&self, event_type: EventType) {
        match event_type {
            EventType::Added => {
                self.telemetry.events_added_total().increment(1);
            }
            EventType::Modified => {
                self.telemetry.events_modified_total().increment(1);
            }
            EventType::Deleted => {
                self.telemetry.events_deleted_total().increment(1);
            }
        }
    }
}

#[async_trait]
impl MetadataCollector for RemoteAgentTaggerMetadataCollector {
    fn name(&self) -> &'static str {
        "remote_agent_tags"
    }

    async fn watch(&mut self, operations_tx: &mut mpsc::Sender<MetadataOperation>) -> Result<(), GenericError> {
        self.health.mark_ready();

        let mut entity_stream = self
            .client
            .get_tagger_stream(RemoteTagCardinality::High)
            .map_err(StatusError::from);
        debug!("Established tagger entity stream.");

        loop {
            select! {
                _ = self.health.live() => {},
                maybe_response = entity_stream.next() => match maybe_response {
                    Some(Ok(response)) => {
                        trace!("Received tagger stream event.");

                        for event in response.events {
                            let event_type = match EventType::try_from(event.r#type) {
                                Ok(event_type) => event_type,
                                Err(_) => {
                                    debug!("Received tagger stream event with unknown type: {}", event.r#type);
                                    continue;
                                }
                            };

                            let entity = match event.entity {
                                Some(entity) => entity,
                                None => {
                                    debug!("Received tagger stream event with no entity.");
                                    continue;
                                }
                            };

                            let entity_id = match entity.id.and_then(remote_entity_id_to_entity_id) {
                                Some(entity_id) => entity_id,
                                None => {
                                    debug!("Received tagger stream event with missing or invalid entity ID.");
                                    continue;
                                }
                            };

                            self.track_event(event_type);

                            let maybe_operation = match event_type {
                                EventType::Added | EventType::Modified => {
                                    let entity_tags = [
                                        (OriginTagCardinality::Low, entity.low_cardinality_tags),
                                        (OriginTagCardinality::Orchestrator, entity.orchestrator_cardinality_tags),
                                        (OriginTagCardinality::High, entity.high_cardinality_tags),
                                    ];

                                    let mut actions = Vec::new();
                                    for (cardinality, tags) in entity_tags {
                                        if !tags.is_empty() {
                                            match self.owned_tags_into_tagset(tags) {
                                                Some(tags) => actions.push(MetadataAction::SetTags { cardinality, tags }),
                                                None => {
                                                    warn!(%entity_id, %cardinality, "Failed to intern tags for entity. Tags will not be present.");
                                                }
                                            }
                                        }
                                    }

                                    if actions.is_empty() {
                                        None
                                    } else {
                                        Some(MetadataOperation {
                                            entity_id,
                                            actions: actions.into(),
                                        })
                                    }
                                }
                                EventType::Deleted => Some(MetadataOperation::delete(entity_id)),
                            };

                            if let Some(operation) = maybe_operation {
                                if let Err(e) = operations_tx.send(operation).await {
                                    debug!(error = %e, "Failed to send metadata operation.");
                                }
                            }
                        }

                        trace!("Processed tagger stream event.");
                    },
                    Some(Err(e)) => {
                        self.telemetry.rpc_errors_total().increment(1);
                        return Err(e.into())
                    },
                    None => break,
                }
            }
        }

        self.health.mark_not_ready();

        Ok(())
    }
}

impl MemoryBounds for RemoteAgentTaggerMetadataCollector {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // TODO: Kind of a throwaway calculation because nothing about the gRPC client can really be bounded at the
        // moment.
        builder
            .firm()
            .with_fixed_amount("self struct", std::mem::size_of::<Self>());
    }
}

fn remote_entity_id_to_entity_id(remote_entity_id: RemoteEntityId) -> Option<EntityId> {
    // TODO: In the future, it would be nice to do zero-copy deserialization so that we could just intern them (or
    // inline them) directly instead of having to deal with the owned strings... but for now, we can transparently
    // convert the owned `String`s to `MetaString`s so it's not a huge deal.
    match remote_entity_id.prefix.as_str() {
        "container_id" => Some(EntityId::Container(remote_entity_id.uid.into())),
        "container_image_metadata" => Some(EntityId::ContainerImageMetadata(remote_entity_id.uid.into())),
        "ecs_task" => Some(EntityId::EcsTask(remote_entity_id.uid.into())),
        "deployment" => Some(EntityId::KubernetesDeployment(remote_entity_id.uid.into())),
        "kubernetes_metadata" => Some(EntityId::KubernetesMetadata(remote_entity_id.uid.into())),
        "kubernetes_node" => Some(EntityId::KubernetesNode(remote_entity_id.uid.into())),
        "kubernetes_pod_uid" => Some(EntityId::PodUid(remote_entity_id.uid.into())),
        "process" => Some(EntityId::Process(remote_entity_id.uid.into())),
        "internal" => match remote_entity_id.uid.as_str() {
            "global-entity-id" => Some(EntityId::Global),
            uid => {
                warn!("Unhandled internal entity ID: internal://{}", uid);
                None
            }
        },
        // Entities that the Agent's tagger publishes but that we have no use for.
        //
        // We ignore these explicitly so that the catch-all arm below stays a meaningful signal: it should only fire
        // for a prefix that the Agent has newly added and that we haven't yet made a decision about.
        //
        // - `gpu`: keyed by GPU device UUID, which never arrives as origin information.
        // - `kueue_workload`, `kubernetes_kueue_queue`, `kueue_resource_flavor`: the Agent folds these same Kueue tags
        //   into the pod and container entities, which we do handle, so consuming them here would be redundant.
        // - `crd`, `kubernetes_capabilities`: describe cluster-level objects rather than workloads, and carry no
        //   identifier that origin detection can resolve.
        // - `host`, `kubelet`: no tagger entity is ever published for these today, so they're listed only for
        //   completeness against the Agent's set of prefixes.
        "gpu"
        | "kueue_workload"
        | "kubernetes_kueue_queue"
        | "kueue_resource_flavor"
        | "crd"
        | "kubernetes_capabilities"
        | "host"
        | "kubelet" => None,
        prefix => {
            warn!("Unhandled entity ID prefix: {}://{}", prefix, remote_entity_id.uid);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_all_otlp_metric_entity_prefixes_from_the_remote_tagger() {
        let cases = [
            ("container_id", "container", EntityId::Container("container".into())),
            (
                "container_image_metadata",
                "sha256:image",
                EntityId::ContainerImageMetadata("sha256:image".into()),
            ),
            ("ecs_task", "task", EntityId::EcsTask("task".into())),
            (
                "deployment",
                "default/api",
                EntityId::KubernetesDeployment("default/api".into()),
            ),
            (
                "kubernetes_metadata",
                "/namespaces//default",
                EntityId::KubernetesMetadata("/namespaces//default".into()),
            ),
            ("kubernetes_node", "node", EntityId::KubernetesNode("node".into())),
            ("kubernetes_pod_uid", "pod", EntityId::PodUid("pod".into())),
            ("process", "42", EntityId::Process("42".into())),
        ];

        for (prefix, uid, expected) in cases {
            assert_eq!(
                remote_entity_id_to_entity_id(RemoteEntityId {
                    prefix: prefix.to_owned(),
                    uid: uid.to_owned(),
                }),
                Some(expected),
                "{prefix}://{uid}"
            );
        }
    }

    #[test]
    fn ignores_known_unused_entity_prefixes_from_the_remote_tagger() {
        let cases = [
            ("gpu", "GPU-00000000-0000-0000-0000-000000000000"),
            ("kueue_workload", "spark-jobs/spark-driver-0"),
            ("kubernetes_kueue_queue", "spark-jobs/local-queue"),
            ("kueue_resource_flavor", "default-flavor"),
            ("crd", "apps/v1/default/example"),
            ("kubernetes_capabilities", "kube_capabilities"),
            ("host", "host"),
            ("kubelet", "kubelet"),
        ];

        for (prefix, uid) in cases {
            assert_eq!(
                remote_entity_id_to_entity_id(RemoteEntityId {
                    prefix: prefix.to_owned(),
                    uid: uid.to_owned(),
                }),
                None,
                "{prefix}://{uid}"
            );
        }
    }
}
