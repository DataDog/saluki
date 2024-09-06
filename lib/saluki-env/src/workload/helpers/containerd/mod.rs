use std::{path::Path, sync::Arc, time::Duration};

use containerd_client::{
    services::v1::{
        Container, ListContainersRequest, ListNamespacesRequest, ListPidsRequest, Namespace, SubscribeRequest,
    },
    Client,
};
use futures::{Stream, StreamExt as _, TryStreamExt as _};
use hyper_util::rt::TokioIo;
use saluki_config::GenericConfiguration;
use saluki_error::{generic_error, GenericError};
use snafu::{ResultExt as _, Snafu};
use tokio::net::UnixStream;
use tonic::{transport::Endpoint, IntoRequest, Request};
use tower::service_fn;

use crate::features::ContainerdDetector;

pub mod events;
use self::events::{decode_envelope_to_event, ContainerdEvent, ContainerdTopic};

const CONTAINERD_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

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
    client: Arc<Client>,
}

impl ContainerdClient {
    /// Creates a new `ContainerdClient` from the given configuration.
    ///
    /// ## Errors
    ///
    /// If the containerd socket path was not present in the configuration or could not be detected, or if the gRPC
    /// transport to containerd could not be created, an error will be returned.
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

        let channel = Endpoint::try_from("https://[::]")
            .unwrap()
            .connect_timeout(CONTAINERD_CONNECT_TIMEOUT)
            .connect_with_connector(service_fn(move |_| {
                let socket_path = socket_path.clone();
                async move { UnixStream::connect(socket_path).await.map(TokioIo::new) }
            }))
            .await?;

        Ok(Self {
            client: Arc::new(Client::from(channel)),
        })
    }

    /// Lists all namespaces.
    ///
    /// ## Errors
    ///
    /// If an error occurs while sending the request or receiving the response, an error will be returned.
    pub async fn list_namespaces(&self) -> Result<Vec<Namespace>, ClientError> {
        let request = ListNamespacesRequest::default();
        let namespaces = self
            .client
            .namespaces()
            .list(request)
            .await
            .context(Response)?
            .into_inner();

        Ok(namespaces.namespaces)
    }

    /// Lists all containers in the given namespace.
    ///
    /// ## Errors
    ///
    /// If an error occurs while sending the request or receiving the response, an error will be returned.
    pub async fn list_containers(&self, namespace: &Namespace) -> Result<Vec<Container>, ClientError> {
        let request = ListContainersRequest::default();
        let request = create_namespaced_request(request, namespace);

        let response = self
            .client
            .containers()
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

        let request = SubscribeRequest { filters };

        let response = self
            .client
            .events()
            .subscribe(request)
            .await
            .context(Response)?
            .into_inner();

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
        let request = create_namespaced_request(request, namespace);

        let response = self
            .client
            .tasks()
            .list_pids(request)
            .await
            .context(Response)?
            .into_inner();

        Ok(response.processes.into_iter().map(|p| p.pid).collect())
    }
}

fn create_namespaced_request<R>(req: R, ns: &Namespace) -> Request<R>
where
    R: IntoRequest<R>,
{
    let mut req = req.into_request();
    let md = req.metadata_mut();
    md.insert("containerd-namespace", ns.name.parse().unwrap());
    req
}

async fn path_exists(path: &Path) -> bool {
    tokio::fs::metadata(path).await.is_ok()
}
