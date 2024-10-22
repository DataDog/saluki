use std::time::Duration;

use async_trait::async_trait;
use datadog_protos::agent::{
    AgentSecureClient, EntityId as RemoteEntityId, EventType, FetchEntityRequest, StreamTagsRequest,
    TagCardinality as RemoteTagCardinality,
};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_context::{Tag, TagSet};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_event::metric::OriginTagCardinality;
use saluki_health::Health;
use stringtheory::interning::GenericMapInterner;
use tokio::{pin, select, sync::mpsc};
use tonic::{
    service::interceptor::InterceptedService,
    transport::{Channel, Endpoint, Uri},
    Code,
};
use tracing::{debug, trace, warn};

use super::MetadataCollector;
use crate::workload::{
    helpers::tonic::{build_self_signed_https_connector, BearerAuthInterceptor},
    metadata::{MetadataAction, MetadataOperation},
    EntityId,
};

const DEFAULT_AGENT_IPC_ENDPOINT: &str = "https://127.0.0.1:5001";
const DEFAULT_AGENT_AUTH_TOKEN_FILE_PATHS: &[&str] =
    &["/etc/datadog-agent/auth/token", "/etc/datadog-agent/auth_token"];

/// A workload provider that uses the remote tagger API from a Datadog Agent, or Datadog Cluster
/// Agent, to provide workload information.
pub struct RemoteAgentMetadataCollector {
    agent_client: AgentSecureClient<InterceptedService<Channel, BearerAuthInterceptor>>,
    tag_interner: GenericMapInterner,
    health: Health,
}

impl RemoteAgentMetadataCollector {
    /// Creates a new `RemoteAgentMetadataCollector` from the given configuration.
    ///
    /// ## Errors
    ///
    /// If the Agent gRPC client cannot be created (invalid API endpoint, missing authentication token, etc), or if the
    /// authentication token is invalid, an error will be returned.
    pub async fn from_configuration(
        config: &GenericConfiguration, health: Health, tag_interner: GenericMapInterner,
    ) -> Result<Self, GenericError> {
        let raw_ipc_endpoint = config
            .try_get_typed::<String>("agent_ipc_endpoint")?
            .unwrap_or_else(|| DEFAULT_AGENT_IPC_ENDPOINT.to_string());

        let ipc_endpoint = match Uri::from_maybe_shared(raw_ipc_endpoint.clone()) {
            Ok(uri) => uri,
            Err(_) => {
                return Err(generic_error!(
                    "Failed to parse configured IPC endpoint for Datadog Agent: {}",
                    raw_ipc_endpoint
                ))
            }
        };

        // Improved code to support multiple default file paths, testing each for existence
        let token_path = match config.try_get_typed::<String>("auth_token_file_path")? {
            Some(path) => path,
            None => DEFAULT_AGENT_AUTH_TOKEN_FILE_PATHS
                .iter()
                .find(|path| std::fs::metadata(path).is_ok())
                .map(|s| s.to_string())
                .ok_or_else(|| {
                    generic_error!(
                        "None of the default auth token file paths exist: {:?}",
                        DEFAULT_AGENT_AUTH_TOKEN_FILE_PATHS
                    )
                })?,
        };

        let bearer_token_interceptor =
            BearerAuthInterceptor::from_file(&token_path)
                .await
                .with_error_context(|| {
                    format!(
                        "Failed to read Datadog Agent authentication token from '{}'",
                        token_path
                    )
                })?;

        // TODO: We need to write a Tower middleware service that allows applying a backoff between failed calls,
        // specifically so that we can throttle reconnection attempts.
        //
        // When the remote Agent endpoint is not available -- Agent isn't running, etc -- the gRPC client will
        // essentially freewheel, trying to reconnect as quickly as possible, which spams the logs, wastes resources, so
        // on and so forth. We would want to essentially apply a backoff like any other client would for the RPC calls
        // themselves, but use it with the _connector_ instead.
        //
        // We could potentially just use a retry middleware, but Tonic does have its own reconnection logic, so we'd
        // have to test it out to make sure it behaves sensibly.
        let channel = Endpoint::from(ipc_endpoint)
            .connect_timeout(Duration::from_secs(2))
            //.timeout(Duration::from_secs(2))
            .connect_with_connector(build_self_signed_https_connector())
            .await
            .with_error_context(|| format!("Failed to connect to Datadog Agent IPC endpoint '{}'", raw_ipc_endpoint))?;

        let mut agent_client = AgentSecureClient::with_interceptor(channel, bearer_token_interceptor);

        // Try and do a basic health check to make sure we can connect and that our authentication token is valid.
        try_query_agent_api(&mut agent_client).await?;

        Ok(Self {
            agent_client,
            tag_interner,
            health,
        })
    }

    fn get_interned_tagset(&self, tags: Vec<String>) -> Option<TagSet> {
        let mut interned_tags = Vec::with_capacity(tags.len());
        for tag in tags {
            let interned_tag = self.tag_interner.try_intern(tag.as_str())?;
            interned_tags.push(Tag::from(interned_tag));
        }

        Some(TagSet::from_iter(interned_tags))
    }
}

#[async_trait]
impl MetadataCollector for RemoteAgentMetadataCollector {
    fn name(&self) -> &'static str {
        "remote-agent"
    }

    async fn watch(&mut self, operations_tx: &mut mpsc::Sender<MetadataOperation>) -> Result<(), GenericError> {
        self.health.mark_ready();

        let mut client = self.agent_client.clone();

        let request = StreamTagsRequest {
            cardinality: RemoteTagCardinality::High.into(),
            ..Default::default()
        };

        // A little trickery here to allow marking liveness of the component while we establish the initial streaming
        // response.
        //
        // Essentially, until the very first message is received, the call to `tagger_stream_entities` won't return,
        // which can happen if there's actually nothing yet in the remote tagger to send back to us. This is, IMO, an
        // issue with the gRPC client/server bits here, and it feels like it should really act more like a socket
        // connection/"OK, we're ready to receive streaming messages", sort of thing... but we have to cope with it
        // as-is for now.
        let stream_fut = client.tagger_stream_entities(request);
        pin!(stream_fut);

        let mut stream = loop {
            select! {
                _ = self.health.live() => continue,
                stream_result = &mut stream_fut => break stream_result?.into_inner(),
            }
        };
        debug!("Established tagger entity stream.");

        loop {
            select! {
                _ = self.health.live() => {},
                result = stream.message() => match result {
                    Ok(Some(response)) => {
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
                                            match self.get_interned_tagset(tags) {
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
                    Ok(None) => break,
                    Err(e) => return Err(e.into()),
                }
            }
        }

        self.health.mark_not_ready();

        Ok(())
    }
}

impl MemoryBounds for RemoteAgentMetadataCollector {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // TODO: Kind of a throwaway calculation because nothing about the gRPC client can really be bounded at the
        // moment.
        builder.firm().with_fixed_amount(std::mem::size_of::<Self>());
    }
}

fn remote_entity_id_to_entity_id(remote_entity_id: RemoteEntityId) -> Option<EntityId> {
    // TODO: Realistically, we should have our own string interner to hold these entity IDs... something like the
    // workload collector having its own or maybe the workload provider, which then passes out a reference to it for all
    // collectors it's using.
    //
    // Either way, we would want to intern, and as such as, we'd want to ideally change our generated code for the
    // protos to hand us string references rather than allocating.
    //
    // Potentially a good reason to do the incremental protobuf encoding work sooner rather than later, since we could
    // switch to the zero-copy structs it has for reading serialized protos.
    match remote_entity_id.prefix.as_str() {
        "container_id" => Some(EntityId::Container(remote_entity_id.uid.into())),
        "kubernetes_pod_uid" => Some(EntityId::PodUid(remote_entity_id.uid.into())),
        other => {
            warn!("Unhandled entity ID prefix: {}", other);
            None
        }
    }
}

async fn try_query_agent_api(
    client: &mut AgentSecureClient<InterceptedService<Channel, BearerAuthInterceptor>>,
) -> Result<(), GenericError> {
    let noop_fetch_request = FetchEntityRequest {
        id: Some(RemoteEntityId {
            prefix: "container_id".to_string(),
            uid: "nonexistent".to_string(),
        }),
        cardinality: RemoteTagCardinality::High.into(),
    };
    match client.tagger_fetch_entity(noop_fetch_request).await {
        Ok(_) => Ok(()),
        Err(e) => match e.code() {
            Code::Unauthenticated => Err(generic_error!(
                "Failed to authenticate to Datadog Agent API. Check that the configured authentication token is correct."
            )),
            _ => Err(e.into()),
        },
    }
}
