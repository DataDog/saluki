use std::{sync::Arc, time::Duration};

use containerd_client::{
    services::v1::{
        Container, GetContainerRequest, ListContainersRequest, ListNamespacesRequest, ListPidsRequest, Namespace,
        SubscribeRequest,
    },
    Client,
};
use futures::{Stream, StreamExt as _, TryStreamExt as _};
use saluki_config::GenericConfiguration;
use snafu::{ResultExt as _, Snafu};
use tokio::net::UnixStream;
use tonic::{transport::Endpoint, IntoRequest, Request};
use tower::service_fn;

pub mod events;
use self::events::{decode_envelope_to_event, ContainerdEvent, ContainerdTopic};

const CONTAINERD_SOCKET_PATH_KEY: &str = "cri_socket_path";
const CONTAINERD_SOCKET_PATH_DEFAULT: &str = "/var/run/containerd/containerd.sock";
const CONTAINERD_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum ClientError {
    #[snafu(display("failed to read required containerd setting '{}'", key))]
    Configuration {
        key: &'static str,
        source: saluki_config::Error,
    },
    #[snafu(display("failed to create gRPC transport: {}", source))]
    Tonic { source: tonic::transport::Error },
    #[snafu(display("failed to make gRPC request: {}", source))]
    Response { source: tonic::Status },
    #[snafu(display("invalid containerd event response: {}", reason))]
    InvalidEvent { reason: String },
    #[snafu(display("failed to get resource"))]
    ResourceMissing,
}

impl ClientError {
    pub fn as_response_error(&self) -> Option<tonic::Status> {
        match self {
            ClientError::Response { source } => Some(source.clone()),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct ContainerdClient {
    client: Arc<Client>,
}

impl ContainerdClient {
    pub async fn from_configuration(config: &GenericConfiguration) -> Result<Self, ClientError> {
        let socket_path = config
            .get_typed::<String>(CONTAINERD_SOCKET_PATH_KEY)
            .context(Configuration {
                key: CONTAINERD_SOCKET_PATH_KEY,
            })?
            .unwrap_or_else(|| CONTAINERD_SOCKET_PATH_DEFAULT.to_string());

        let channel = Endpoint::try_from("https://[::]")
            .unwrap()
            .connect_timeout(CONTAINERD_CONNECT_TIMEOUT)
            .connect_with_connector(service_fn(move |_| UnixStream::connect(socket_path.clone())))
            .await
            .context(Tonic)?;

        Ok(Self {
            client: Arc::new(Client::from(channel)),
        })
    }

    pub async fn get_namespaces(&self) -> Result<Vec<Namespace>, ClientError> {
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

    pub async fn get_container(&self, namespace: &Namespace, id: String) -> Result<Container, ClientError> {
        let request = GetContainerRequest { id: id.clone() };
        let request = create_namespaced_request(request, namespace);

        let response = self
            .client
            .containers()
            .get(request)
            .await
            .context(Response)?
            .into_inner();

        response.container.ok_or(ClientError::ResourceMissing)
    }

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

    pub async fn get_pids_for_container(
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
