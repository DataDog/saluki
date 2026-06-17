use std::{path::Path, time::Duration};

use containerd_protos::services::{
    containers::v1::{containers_client::ContainersClient, Container, ListContainersRequest},
    events::v1::{events_client::EventsClient, SubscribeRequest},
    namespaces::v1::{namespaces_client::NamespacesClient, ListNamespacesRequest, Namespace},
    tasks::v1::{tasks_client::TasksClient, ListPidsRequest},
};
use futures::{Stream, StreamExt as _, TryStreamExt as _};
use hyper_util::rt::TokioIo;
use saluki_config_tools::GenericConfiguration;
use saluki_error::{generic_error, GenericError};
use serde::Deserialize;
use snafu::{ResultExt as _, Snafu};
use tokio::net::UnixStream;
use tonic::{
    transport::{Channel, Endpoint},
    IntoRequest, Request,
};
use tower::service_fn;

use crate::features::ContainerdDetector;

pub mod events;
use self::events::{decode_envelope_to_event, ContainerdEvent, ContainerdTopic};

const MAX_LIST_CONTAINERS_RESPONSE_SIZE: usize = 16 * 1024 * 1024;

const fn default_connection_timeout_secs() -> u64 {
    1
}

const fn default_query_timeout_secs() -> u64 {
    5
}

/// Containerd gRPC client configuration.
#[derive(Deserialize)]
#[cfg_attr(test, derive(Debug, PartialEq, serde::Serialize))]
pub struct ContainerdConfiguration {
    /// Timeout for establishing the gRPC connection to the containerd socket.
    ///
    /// Defaults to 1 second. Increase this for hosts where containerd may accept socket connections slowly.
    #[serde(default = "default_connection_timeout_secs", rename = "cri_connection_timeout")]
    connection_timeout_secs: u64,

    /// Per-RPC timeout for containerd API calls.
    ///
    /// Defaults to 5 seconds. Each containerd RPC receives this deadline independently.
    #[serde(default = "default_query_timeout_secs", rename = "cri_query_timeout")]
    query_timeout_secs: u64,
}

impl ContainerdConfiguration {
    /// Creates a new `ContainerdConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed::<Self>()?)
    }

    /// Returns the timeout for establishing the gRPC connection to the containerd socket.
    pub const fn connection_timeout(&self) -> Duration {
        Duration::from_secs(self.connection_timeout_secs)
    }

    /// Returns the per-RPC timeout for containerd API calls.
    pub const fn query_timeout(&self) -> Duration {
        Duration::from_secs(self.query_timeout_secs)
    }
}

/// A [`ContainerdClient`] error.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum ClientError {
    /// Failed to send a gRPC request.
    #[snafu(display("failed to make gRPC request: {}", source))]
    Response { source: tonic::Status },

    /// Received an invalid event response from containerd.
    #[snafu(display("invalid containerd event response: {}", reason))]
    InvalidEvent { reason: String },
}

impl ClientError {
    /// Gets the status of the response, if this is a response error.
    pub fn as_response_error(&self) -> Option<tonic::Status> {
        match self {
            ClientError::Response { source } => Some(source.clone()),
            _ => None,
        }
    }
}

/// Containerd gRPC client.
#[derive(Clone)]
pub struct ContainerdClient {
    channel: Channel,
    query_timeout: Duration,
}

impl ContainerdClient {
    /// Creates a new `ContainerdClient` from the given configuration.
    ///
    /// ## Errors
    ///
    /// If the containerd socket path wasn't present in the configuration or couldn't be detected, or if the gRPC
    /// transport to containerd couldn't be created, an error will be returned.
    pub async fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let socket_path = ContainerdDetector::detect_grpc_socket_path(config)
            .ok_or(generic_error!(
                "failed to detect containerd socket path; not available at default path and not specified in configuration (`cri_socket_path`)"
            ))?;

        if !path_exists(&socket_path).await {
            return Err(generic_error!(
                "Detected containerd socket path ({}) but path does not exist, or process lacks permissions.",
                socket_path.to_string_lossy()
            ));
        }

        let containerd_config = ContainerdConfiguration::from_configuration(config)?;
        let channel = Endpoint::try_from("https://[::]")
            .unwrap()
            .connect_timeout(containerd_config.connection_timeout())
            .connect_with_connector(service_fn(move |_| {
                let socket_path = socket_path.clone();
                async move { UnixStream::connect(socket_path).await.map(TokioIo::new) }
            }))
            .await?;

        Ok(Self {
            channel,
            query_timeout: containerd_config.query_timeout(),
        })
    }

    /// Lists all namespaces.
    ///
    /// ## Errors
    ///
    /// If an error occurs while sending the request or receiving the response, an error will be returned.
    pub async fn list_namespaces(&self) -> Result<Vec<Namespace>, ClientError> {
        let request = create_timed_request(ListNamespacesRequest::default(), self.query_timeout);

        let mut client = NamespacesClient::new(self.channel.clone());
        let namespaces = client.list(request).await.context(Response)?.into_inner();

        Ok(namespaces.namespaces)
    }

    /// Lists all containers in the given namespace.
    ///
    /// ## Errors
    ///
    /// If an error occurs while sending the request or receiving the response, an error will be returned.
    pub async fn list_containers(&self, namespace: &Namespace) -> Result<Vec<Container>, ClientError> {
        let request = ListContainersRequest::default();
        let request = create_timed_namespaced_request(request, namespace, self.query_timeout);

        let client = ContainersClient::new(self.channel.clone());
        let response = client
            .max_decoding_message_size(MAX_LIST_CONTAINERS_RESPONSE_SIZE)
            .list(request)
            .await
            .context(Response)?
            .into_inner();

        Ok(response.containers)
    }

    /// Watches for specific containerd events in the given namespace.
    ///
    /// Multiple topics (topics map directly to event types) can be watched on the same stream.
    ///
    /// ## Errors
    ///
    /// If an error occurs while sending the request or receiving the response, an error will be returned.
    pub async fn watch_events(
        &self, topics: &[ContainerdTopic], namespace: &Namespace,
    ) -> Result<impl Stream<Item = Result<ContainerdEvent, ClientError>> + Unpin, ClientError> {
        // Create our subscribe request, which requires a filter for each topic we're interested in, that is also
        // combined with the namespace that should be watched.
        let mut filters = Vec::new();
        for topic in topics {
            filters.push(format!(
                "topic==\"{}\",namespace=={}",
                topic.as_topic_str(),
                namespace.name
            ));
        }

        // Match datadog-agent behavior: `cri_query_timeout` is used for unary CRI/containerd queries, but event
        // subscriptions are long-lived streams that should stay open until the collector is canceled.
        let request = SubscribeRequest { filters };

        let mut client = EventsClient::new(self.channel.clone());
        let response = client.subscribe(request).await.context(Response)?.into_inner();

        Ok(response
            .map_err(|source| ClientError::Response { source })
            .filter_map(|result| async move {
                // Filter out all of the non-container events from the stream, converting container events to
                // `ContainerEvent` in the process.
                //
                // If the message is an error, we pass it through unfiltered.
                result
                    .and_then(|envelope| {
                        decode_envelope_to_event(envelope).map_err(|_| ClientError::InvalidEvent {
                            reason: "failed to decode envelope payload".to_string(),
                        })
                    })
                    .transpose()
            })
            .boxed())
    }

    /// Lists all process IDs for the given container in the given namespace.
    ///
    /// ## Errors
    ///
    /// If an error occurs while sending the request or receiving the response, an error will be returned.
    pub async fn list_pids_for_container(
        &self, namespace: &Namespace, container_id: String,
    ) -> Result<Vec<u32>, ClientError> {
        let request = ListPidsRequest { container_id };
        let request = create_timed_namespaced_request(request, namespace, self.query_timeout);

        let mut client = TasksClient::new(self.channel.clone());
        let response = client.list_pids(request).await.context(Response)?.into_inner();

        Ok(response.processes.into_iter().map(|p| p.pid).collect())
    }
}

fn create_timed_request<R>(req: R, timeout: Duration) -> Request<R>
where
    R: IntoRequest<R>,
{
    let mut req = req.into_request();
    req.set_timeout(timeout);
    req
}

fn create_timed_namespaced_request<R>(req: R, ns: &Namespace, timeout: Duration) -> Request<R>
where
    R: IntoRequest<R>,
{
    let mut req = create_timed_request(req, timeout);
    let md = req.metadata_mut();
    md.insert("containerd-namespace", ns.name.parse().unwrap());
    req
}

async fn path_exists(path: &Path) -> bool {
    tokio::fs::metadata(path).await.is_ok()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use saluki_config_tools::ConfigurationLoader;

    use super::*;

    #[tokio::test]
    async fn configuration_uses_core_agent_cri_timeout_defaults() {
        let (config, _updates_tx) = ConfigurationLoader::for_tests(None, None, false).await;
        let containerd_config =
            ContainerdConfiguration::from_configuration(&config).expect("configuration should load");

        assert_eq!(Duration::from_secs(1), containerd_config.connection_timeout());
        assert_eq!(Duration::from_secs(5), containerd_config.query_timeout());
    }

    #[tokio::test]
    async fn configuration_reads_cri_timeout_overrides() {
        let raw_config = serde_yaml::from_str(
            r#"
            cri_connection_timeout: 2
            cri_query_timeout: 7
            "#,
        )
        .expect("config should parse");
        let (config, _updates_tx) = ConfigurationLoader::for_tests(Some(raw_config), None, false).await;
        let containerd_config =
            ContainerdConfiguration::from_configuration(&config).expect("configuration should load");

        assert_eq!(Duration::from_secs(2), containerd_config.connection_timeout());
        assert_eq!(Duration::from_secs(7), containerd_config.query_timeout());
    }

    #[test]
    fn create_timed_request_sets_grpc_timeout() {
        let request = create_timed_request(ListNamespacesRequest::default(), Duration::from_secs(3));

        assert_eq!(
            Some("3000000u"),
            request.metadata().get("grpc-timeout").map(|v| v.to_str().unwrap())
        );
    }

    #[test]
    fn create_timed_namespaced_request_sets_namespace_and_grpc_timeout() {
        let namespace = Namespace {
            name: "k8s.io".to_string(),
            labels: Default::default(),
        };
        let request =
            create_timed_namespaced_request(ListContainersRequest::default(), &namespace, Duration::from_secs(4));

        assert_eq!(
            Some("4000000u"),
            request.metadata().get("grpc-timeout").map(|v| v.to_str().unwrap())
        );
        assert_eq!(
            Some("k8s.io"),
            request
                .metadata()
                .get("containerd-namespace")
                .map(|v| v.to_str().unwrap())
        );
    }
}
