use async_trait::async_trait;
use datadog_agent_commons::ipc::client::RemoteAgentClient;
use datadog_protos::agent::{EventType, TagCardinality};
use futures::StreamExt as _;
use saluki_context::{origin::OriginTagCardinality, tags::TagSet};
use saluki_env::workload::{collectors::MetadataCollector, MetadataOperation};
use saluki_error::{generic_error, GenericError};
use tokio::sync::mpsc;

use super::entity_id_from_tagger;

/// Collects tagger entity tags from the Datadog Agent stream.
#[derive(Clone)]
pub struct TaggerMetadataCollector {
    client: RemoteAgentClient,
    cardinality: TagCardinality,
    name: &'static str,
}

impl TaggerMetadataCollector {
    /// Creates a tagger collector for the requested cardinality stream.
    pub fn new(client: RemoteAgentClient, cardinality: TagCardinality) -> Self {
        let name = match cardinality {
            TagCardinality::Low => "workload/tagger-low",
            TagCardinality::Orchestrator => "workload/tagger-orchestrator",
            TagCardinality::High => "workload/tagger-high",
        };
        Self {
            client,
            cardinality,
            name,
        }
    }
}

#[async_trait]
impl MetadataCollector for TaggerMetadataCollector {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn watch(&mut self, operations_tx: &mut mpsc::Sender<MetadataOperation>) -> Result<(), GenericError> {
        let mut client = self.client.clone();
        let mut stream = client.get_tagger_stream(self.cardinality);

        while let Some(result) = stream.next().await {
            let response = result?;
            for event in response.events {
                let Some(entity) = event.entity else {
                    continue;
                };
                let Some(entity_id) = entity.id.and_then(entity_id_from_tagger) else {
                    continue;
                };

                match EventType::try_from(event.r#type).unwrap_or(EventType::Modified) {
                    EventType::Added | EventType::Modified => {
                        send_tags(
                            operations_tx,
                            entity_id.clone(),
                            OriginTagCardinality::Low,
                            entity.low_cardinality_tags,
                        )
                        .await?;
                        send_tags(
                            operations_tx,
                            entity_id.clone(),
                            OriginTagCardinality::Orchestrator,
                            entity.orchestrator_cardinality_tags,
                        )
                        .await?;
                        send_tags(
                            operations_tx,
                            entity_id,
                            OriginTagCardinality::High,
                            entity.high_cardinality_tags,
                        )
                        .await?;
                    }
                    EventType::Deleted => {
                        operations_tx.send(MetadataOperation::delete(entity_id)).await?;
                    }
                }
            }
        }

        Err(generic_error!("Datadog Agent tagger stream ended."))
    }
}

async fn send_tags(
    operations_tx: &mut mpsc::Sender<MetadataOperation>, entity_id: saluki_env::workload::EntityId,
    cardinality: OriginTagCardinality, tags: Vec<String>,
) -> Result<(), GenericError> {
    let mut tag_set = TagSet::default();
    for tag in tags {
        tag_set.insert_tag(tag);
    }
    operations_tx
        .send(MetadataOperation::set_tags(entity_id, cardinality, tag_set))
        .await?;
    Ok(())
}
