use async_trait::async_trait;
use datadog_agent_commons::ipc::{client::RemoteAgentClient, config::RemoteAgentClientConfiguration};
use datadog_protos::agent::{EntityId as RemoteEntityId, EventType, TagCardinality as RemoteTagCardinality};
use futures::{StreamExt as _, TryStreamExt as _};
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

/// Entity ID prefixes that we subscribe to and convert into an [`EntityId`].
///
/// This is the set of prefixes we ask the Agent's tagger for, so an entity whose prefix isn't listed here never
/// reaches us at all. Adding a prefix here requires a matching arm in [`remote_entity_id_to_entity_id`].
const HANDLED_ENTITY_ID_PREFIXES: &[&str] = &[
    "container_id",
    "container_image_metadata",
    "ecs_task",
    "deployment",
    "kubernetes_metadata",
    "kubernetes_node",
    "kubernetes_pod_uid",
    "process",
    "internal",
];

/// Entity ID prefixes that the Agent's tagger publishes but that we have reviewed and deliberately don't consume.
///
/// We don't subscribe to these, so they cost us nothing at runtime. Recording them here is what lets us tell a prefix
/// we've decided against apart from one the Agent has newly added, which is the difference the
/// `agent_entity_id_prefixes_are_all_classified` test checks for.
///
/// - `gpu`: keyed by GPU device UUID, which never arrives as origin information.
/// - `kueue_workload`, `kubernetes_kueue_queue`, `kueue_resource_flavor`: the Agent folds these same Kueue tags into
///   the pod and container entities, which we do handle, so consuming them here would be redundant.
/// - `crd`, `kubernetes_capabilities`: describe cluster-level objects rather than workloads, and carry no identifier
///   that origin detection can resolve.
/// - `host`, `kubelet`: no tagger entity is ever published for these today, so they're listed only for completeness
///   against the Agent's set of prefixes.
const IGNORED_ENTITY_ID_PREFIXES: &[&str] = &[
    "gpu",
    "kueue_workload",
    "kubernetes_kueue_queue",
    "kueue_resource_flavor",
    "crd",
    "kubernetes_capabilities",
    "host",
    "kubelet",
];

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
    pub async fn new(
        client_config: &RemoteAgentClientConfiguration, health: Health, interner: GenericMapInterner,
    ) -> Result<Self, GenericError> {
        let client = RemoteAgentClient::connect(client_config).await?;

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

        let prefixes = HANDLED_ENTITY_ID_PREFIXES
            .iter()
            .map(|prefix| prefix.to_string())
            .collect();
        let mut entity_stream = self
            .client
            .get_tagger_stream(RemoteTagCardinality::High, Some(prefixes))
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
        // Entities we've reviewed and don't consume. See `IGNORED_ENTITY_ID_PREFIXES` for why each one is here.
        //
        // We don't subscribe to these, so in practice they never arrive at all.
        prefix if IGNORED_ENTITY_ID_PREFIXES.contains(&prefix) => None,
        // Anything we didn't ask for. The Agent filters the stream by the prefixes we subscribe to, so reaching this
        // arm means either the Agent ignored our filter or it's too old to support one.
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

    /// The Agent's set of tagger entity ID prefixes, regenerated nightly by
    /// `.github/workflows/update-agent-tagger-prefixes.yml`.
    const AGENT_ENTITY_ID_PREFIXES: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/agent_tagger_entity_id_prefixes.txt"
    ));

    fn agent_entity_id_prefixes() -> Vec<&'static str> {
        AGENT_ENTITY_ID_PREFIXES
            .lines()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect()
    }

    #[test]
    fn agent_entity_id_prefix_fixture_is_intact() {
        // A truncated or empty fixture would make `agent_entity_id_prefixes_are_all_classified` pass without checking
        // anything, so assert that it still looks like the Agent's list before we rely on it. The generating workflow
        // performs the same check, but the fixture can also be edited by hand.
        let prefixes = agent_entity_id_prefixes();

        assert!(
            prefixes.len() >= HANDLED_ENTITY_ID_PREFIXES.len(),
            "fixture lists only {} prefixes, which is fewer than the {} we handle: it is likely truncated",
            prefixes.len(),
            HANDLED_ENTITY_ID_PREFIXES.len(),
        );

        for anchor in ["container_id", "kubernetes_pod_uid", "internal"] {
            assert!(
                prefixes.contains(&anchor),
                "fixture is missing the long-standing `{anchor}` prefix, so it is likely truncated or malformed"
            );
        }
    }

    #[test]
    fn agent_entity_id_prefixes_are_all_classified() {
        // Note that we deliberately don't assert the other direction. ADP runs alongside a range of Agent versions, so
        // a prefix that the Agent has since removed may still need handling for older Agents.
        for prefix in agent_entity_id_prefixes() {
            assert!(
                HANDLED_ENTITY_ID_PREFIXES.contains(&prefix) || IGNORED_ENTITY_ID_PREFIXES.contains(&prefix),
                "the Agent's tagger publishes `{prefix}://` entities but ADP neither handles nor ignores them. Either \
                 add `{prefix}` to `HANDLED_ENTITY_ID_PREFIXES` along with an arm in \
                 `remote_entity_id_to_entity_id` that converts it, or add it to `IGNORED_ENTITY_ID_PREFIXES` with a \
                 comment explaining why we don't need it."
            );
        }
    }

    #[test]
    fn prefix_lists_agree_with_conversion() {
        // The prefixes we subscribe to determine what the Agent sends us, so a prefix listed as handled but missing an
        // arm here (or vice versa) would silently drop entities we asked for.
        //
        // `internal` only converts for a single UID, so use that UID throughout: every other prefix treats it as an
        // opaque value.
        for prefix in HANDLED_ENTITY_ID_PREFIXES {
            assert!(
                remote_entity_id_to_entity_id(RemoteEntityId {
                    prefix: (*prefix).to_owned(),
                    uid: "global-entity-id".to_owned(),
                })
                .is_some(),
                "`{prefix}` is subscribed to but isn't converted by `remote_entity_id_to_entity_id`"
            );

            assert!(
                !IGNORED_ENTITY_ID_PREFIXES.contains(prefix),
                "`{prefix}` is listed as both handled and ignored"
            );
        }

        for prefix in IGNORED_ENTITY_ID_PREFIXES {
            assert_eq!(
                remote_entity_id_to_entity_id(RemoteEntityId {
                    prefix: (*prefix).to_owned(),
                    uid: "global-entity-id".to_owned(),
                }),
                None,
                "`{prefix}` is listed as ignored but is converted by `remote_entity_id_to_entity_id`"
            );
        }
    }
}
