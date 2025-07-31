use async_trait::async_trait;
use datadog_protos::agent::{EntityId as RemoteEntityId, EventType, TagCardinality as RemoteTagCardinality};
use futures::StreamExt as _;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_context::{
    origin::OriginTagCardinality,
    tags::{Tag, TagSet},
};
use saluki_error::GenericError;
use saluki_health::Health;
use stringtheory::{
    interning::{GenericMapInterner, Interner as _},
    MetaString,
};
use tokio::{select, sync::mpsc};
use tracing::{debug, trace, warn};

use crate::{
    helpers::remote_agent::RemoteAgentClient,
    workload::{
        collectors::MetadataCollector,
        metadata::{MetadataAction, MetadataOperation},
        EntityId,
    },
};

/// A workload provider that uses the remote tagger API from a Datadog Agent to provide workload information.
pub struct RemoteAgentTaggerMetadataCollector {
    client: RemoteAgentClient,
    tag_interner: GenericMapInterner,
    health: Health,
}

impl RemoteAgentTaggerMetadataCollector {
    /// Creates a new `RemoteAgentTaggerMetadataCollector` from the given configuration.
    ///
    /// ## Errors
    ///
    /// If the Agent gRPC client cannot be created (invalid API endpoint, missing authentication token, etc), or if the
    /// authentication token is invalid, an error will be returned.
    pub async fn from_configuration(
        config: &GenericConfiguration, health: Health, tag_interner: GenericMapInterner,
    ) -> Result<Self, GenericError> {
        let client = RemoteAgentClient::from_configuration(config).await?;

        Ok(Self {
            client,
            tag_interner,
            health,
        })
    }

    fn owned_tags_into_tagset(&self, tags: Vec<String>) -> Option<TagSet> {
        // We'll either inline the tags if they're short enough, otherwise we intern them.
        let mut new_tags = Vec::with_capacity(tags.len());
        for tag in tags {
            let new_tag = match MetaString::try_inline(&tag) {
                Some(s) => Tag::from(s),
                None => {
                    let interned = self.tag_interner.try_intern(&tag)?;
                    Tag::from(interned)
                }
            };

            new_tags.push(new_tag);
        }

        Some(TagSet::from_iter(new_tags))
    }
}

#[async_trait]
impl MetadataCollector for RemoteAgentTaggerMetadataCollector {
    fn name(&self) -> &'static str {
        "remote-agent-tags"
    }

    async fn watch(&mut self, operations_tx: &mut mpsc::Sender<MetadataOperation>) -> Result<(), GenericError> {
        self.health.mark_ready();

        let mut entity_stream = self.client.get_tagger_stream(RemoteTagCardinality::High);
        debug!("Established tagger entity stream.");

        loop {
            select! {
                _ = self.health.live() => {},
                result = entity_stream.next() => match result {
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
                    Some(Err(e)) => return Err(e.into()),
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
        "kubernetes_pod_uid" => Some(EntityId::PodUid(remote_entity_id.uid.into())),
        "internal" => match remote_entity_id.uid.as_str() {
            "global-entity-id" => Some(EntityId::Global),
            uid => {
                warn!("Unhandled internal entity ID: internal://{}", uid);
                None
            }
        },
        // We don't care about these, so we just ignore them.
        "container_image_metadata" => None,
        prefix => {
            warn!("Unhandled entity ID prefix: {}://{}", prefix, remote_entity_id.uid);
            None
        }
    }
}
