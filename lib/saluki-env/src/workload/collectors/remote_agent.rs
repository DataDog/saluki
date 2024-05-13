use std::time::Duration;

use async_trait::async_trait;
use datadog_protos::agent_secure::{
    AgentSecureClient, EntityId as RemoteEntityId, EventType, StreamTagsRequest, TagCardinality as RemoteTagCardinality,
};
use saluki_config::GenericConfiguration;
use tokio::sync::mpsc;
use tonic::{
    service::interceptor::InterceptedService,
    transport::{Channel, Endpoint},
};
use tracing::{debug, warn};

use crate::workload::{
    helpers::tonic::{build_self_signed_https_connector, BearerAuthInterceptor},
    metadata::{MetadataAction, MetadataOperation, TagCardinality},
    EntityId,
};

use super::MetadataCollector;

const DEFAULT_AGENT_IPC_ENDPOINT: &str = "https://127.0.0.1:5001";
const DEFAULT_AGENT_AUTH_TOKEN_FILE_PATH: &str = "/etc/datadog-agent/auth/token";

/// A workload provider that uses the remote tagger API from a Datadog Agent, or Datadog Cluster
/// Agent, to provide workload information.
pub struct RemoteAgentMetadataCollector {
    agent_client: AgentSecureClient<InterceptedService<Channel, BearerAuthInterceptor>>,
}

impl RemoteAgentMetadataCollector {
    /// Creates a new `RemoteAgentMetadataCollector` from the given configuration.
    ///
    /// ## Errors
    ///
    /// If the collector fails to connect to the tagger API, an error will be returined.
    pub async fn from_configuration(
        config: &GenericConfiguration,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let api_endpoint = config
            .get_typed::<String>("agent_ipc_endpoint")?
            .unwrap_or_else(|| DEFAULT_AGENT_IPC_ENDPOINT.to_string());

        let auth_token_file_path = config
            .get_typed::<String>("auth_token_file_path")?
            .unwrap_or_else(|| DEFAULT_AGENT_AUTH_TOKEN_FILE_PATH.to_string());

        let bearer_token_interceptor = BearerAuthInterceptor::from_file(auth_token_file_path).await?;

        let channel = Endpoint::from_shared(api_endpoint)?
            .connect_timeout(Duration::from_secs(2))
            .connect_with_connector(build_self_signed_https_connector())
            .await?;

        let agent_client = AgentSecureClient::with_interceptor(channel, bearer_token_interceptor);

        Ok(Self { agent_client })
    }
}

#[async_trait]
impl MetadataCollector for RemoteAgentMetadataCollector {
    fn name(&self) -> &'static str {
        "remote-agent"
    }

    async fn watch(
        &self, operations_tx: &mut mpsc::Sender<MetadataOperation>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut client = self.agent_client.clone();

        let request = StreamTagsRequest {
            cardinality: RemoteTagCardinality::High.into(),
            ..Default::default()
        };

        let mut stream = client.tagger_stream_entities(request).await?.into_inner();
        loop {
            match stream.message().await {
                Ok(Some(response)) => {
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

                        let operation = match event_type {
                            EventType::Added | EventType::Modified => {
                                let mut actions = Vec::new();
                                if !entity.low_cardinality_tags.is_empty() {
                                    actions.push(MetadataAction::SetTags {
                                        cardinality: TagCardinality::Low,
                                        tags: entity.low_cardinality_tags.into_iter().map(Into::into).collect(),
                                    });
                                }

                                if !entity.high_cardinality_tags.is_empty() {
                                    actions.push(MetadataAction::SetTags {
                                        cardinality: TagCardinality::High,
                                        tags: entity.high_cardinality_tags.into_iter().map(Into::into).collect(),
                                    });
                                }

                                MetadataOperation {
                                    entity_id,
                                    actions: actions.into(),
                                }
                            }
                            EventType::Deleted => MetadataOperation::delete(entity_id),
                        };

                        if let Err(e) = operations_tx.send(operation).await {
                            debug!(error = %e, "Failed to send metadata operation.");
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => return Err(Box::new(e)),
            }
        }

        Ok(())
    }
}

fn remote_entity_id_to_entity_id(remote_entity_id: RemoteEntityId) -> Option<EntityId> {
    match remote_entity_id.prefix.as_str() {
        "container_id" => Some(EntityId::Container(remote_entity_id.uid)),
        "kubernetes_pod_uid" => Some(EntityId::PodUid(remote_entity_id.uid)),
        other => {
            warn!("Unhandled entity ID prefix: {}", other);
            None
        }
    }
}
