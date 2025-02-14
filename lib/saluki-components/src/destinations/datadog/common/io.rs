use std::{error::Error as _, sync::Arc};

use futures::FutureExt as _;
use http::{Request, Uri};
use http_body::Body;
use http_body_util::BodyExt as _;
use hyper::body::Incoming;
use saluki_config::RefreshableConfiguration;
use saluki_core::task::spawn_traced;
use saluki_error::{generic_error, GenericError};
use saluki_io::net::client::http::HttpClient;
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;
use tokio::{
    select,
    sync::{mpsc, oneshot, Barrier},
    task::JoinSet,
};
use tower::{BoxError, Service, ServiceBuilder, ServiceExt as _};
use tracing::{debug, error};

use super::{config::ForwarderConfiguration, endpoints::ResolvedEndpoint, telemetry::ComponentTelemetry};
use crate::destinations::datadog::common::middleware::{for_resolved_endpoint, with_version_info};

pub struct Handle<B> {
    requests_tx: mpsc::Sender<(usize, Request<B>)>,
    io_shutdown_rx: oneshot::Receiver<()>,
}

impl<B> Handle<B> {
    pub async fn send_request(&self, events: usize, request: Request<B>) -> Result<(), GenericError> {
        match self.requests_tx.send((events, request)).await {
            Ok(()) => Ok(()),
            Err(_) => Err(generic_error!("Failed to send request to I/O task: receiver dropped.")),
        }
    }

    pub async fn shutdown(self) {
        let Self {
            requests_tx,
            io_shutdown_rx,
        } = self;

        // Drop the sender side of the request channel, which will propagate the actual closure to the main I/O task.
        drop(requests_tx);

        // Wait for the main I/O task to signal that it has shutdown.
        io_shutdown_rx.await.expect("I/O task has already shutdown.");
    }
}

pub struct TransactionForwarder<B> {
    config: ForwarderConfiguration,
    telemetry: ComponentTelemetry,
    client: HttpClient<B>,
    endpoints: Vec<ResolvedEndpoint>,
}

impl<B> TransactionForwarder<B>
where
    B: Body + Clone + Unpin + Send + 'static,
    B::Data: Send,
    B::Error: std::error::Error + Send + Sync,
{
    pub fn from_config<F>(
        config: ForwarderConfiguration, maybe_refreshable_config: Option<RefreshableConfiguration>, endpoint_name: F,
        telemetry: ComponentTelemetry, metrics_builder: MetricsBuilder,
    ) -> Result<Self, GenericError>
    where
        F: Fn(&Uri) -> Option<MetaString> + Send + Sync + 'static,
    {
        let endpoints = config.endpoint().build_resolved_endpoints(maybe_refreshable_config)?;
        let client = HttpClient::builder()
            .with_request_timeout(config.request_timeout())
            .with_retry_policy(config.retry().to_default_http_retry_policy())
            .with_bytes_sent_counter(telemetry.bytes_sent().clone())
            .with_endpoint_telemetry(metrics_builder, Some(endpoint_name))
            .build()?;

        Ok(Self {
            config,
            telemetry,
            client,
            endpoints,
        })
    }

    pub async fn spawn(self) -> Handle<B> {
        let (requests_tx, requests_rx) = mpsc::channel(32);
        let (io_shutdown_tx, io_shutdown_rx) = oneshot::channel();

        let Self {
            config,
            telemetry,
            client,
            endpoints,
        } = self;

        spawn_traced(run_io_loop(
            requests_rx,
            io_shutdown_tx,
            config,
            client,
            telemetry,
            endpoints,
        ));

        Handle {
            requests_tx,
            io_shutdown_rx,
        }
    }
}

async fn run_io_loop<S, B>(
    mut requests_rx: mpsc::Receiver<(usize, Request<B>)>, io_shutdown_tx: oneshot::Sender<()>,
    config: ForwarderConfiguration, service: S, telemetry: ComponentTelemetry,
    resolved_endpoints: Vec<ResolvedEndpoint>,
) where
    S: Service<Request<B>, Response = hyper::Response<Incoming>> + Clone + Send + 'static,
    S::Future: Send,
    S::Error: Send + Into<BoxError>,
    B: Body + Clone + Send + 'static,
{
    // Spawn an endpoint I/O task for each endpoint we're configured to send to.
    //
    // This gives us granularity at just the right level, since the places we expect there to be problems (and thus
    // where concurrency can help make forward progress) are at the endpoint/API key level.
    let mut endpoint_txs = Vec::new();
    let task_barrier = Arc::new(Barrier::new(resolved_endpoints.len() + 1));

    for resolved_endpoint in resolved_endpoints {
        let endpoint_url = resolved_endpoint.endpoint().to_string();

        let (endpoint_tx, endpoint_rx) = mpsc::channel(config.endpoint_buffer_size());
        let task_barrier = Arc::clone(&task_barrier);
        spawn_traced(run_endpoint_io_loop(
            endpoint_rx,
            task_barrier,
            config.clone(),
            service.clone(),
            telemetry.clone(),
            resolved_endpoint,
        ));

        endpoint_txs.push((endpoint_url, endpoint_tx));
    }

    // Listen for requests to forward, and send a copy of each one to each endpoint I/O task.
    while let Some((metrics_count, request)) = requests_rx.recv().await {
        for (endpoint_url, endpoint_tx) in &endpoint_txs {
            if endpoint_tx.send((metrics_count, request.clone())).await.is_err() {
                error!(
                    endpoint = endpoint_url,
                    "Failed to send request to endpoint I/O task: receiver dropped."
                );
            }
        }
    }

    debug!("Requests channel for main I/O task complete. Stopping endpoint I/O tasks and synchronizing on shutdown.");

    // Drop our endpoint I/O task channels, which will cause them to shut down once they've processed all outstanding
    // requests in their respective channel. We wait for that to happen by synchronizing on the task barrier.
    //
    // Once all tasks have completed, we signal back to the main component task that the I/O loop has shutdown.
    drop(endpoint_txs);
    task_barrier.wait().await;

    debug!("All endpoint I/O tasks have stopped. Main I/O task shutting down.");

    let _ = io_shutdown_tx.send(());
}

async fn run_endpoint_io_loop<S, B>(
    mut requests_rx: mpsc::Receiver<(usize, Request<B>)>, task_barrier: Arc<Barrier>, config: ForwarderConfiguration,
    service: S, telemetry: ComponentTelemetry, endpoint: ResolvedEndpoint,
) where
    S: Service<Request<B>, Response = hyper::Response<Incoming>> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<BoxError> + Send + 'static,
    B: Body,
{
    let endpoint_url = endpoint.endpoint().to_string();
    debug!(
        endpoint = endpoint_url,
        num_workers = config.endpoint_concurrency(),
        "Starting endpoint I/O task."
    );

    // Build our endpoint service.
    //
    // This is where we'll modify the incoming request for our our specific endpoint, such as setting the host portion
    // of the URI, adding the API key as a header, and so on.
    let mut service = ServiceBuilder::new()
        // Set the request's URI to the endpoint's URI, and add the API key as a header.
        .map_request(for_resolved_endpoint::<B>(endpoint))
        // Set the User-Agent and DD-Agent-Version headers indicating the version of the data plane sending the request.
        .map_request(with_version_info::<B>())
        .concurrency_limit(config.endpoint_concurrency())
        .service(service);

    let mut in_flight = JoinSet::new();
    let mut done = false;

    loop {
        select! {
            // While we're still receiving requests, wait for the service to become ready, and then take the next
            // request, and send it to the service.
            svc = service.ready(), if !done => match svc {
                Ok(svc) => match requests_rx.recv().await {
                    Some((events, request)) => {
                        let fut = svc.call(request).map(move |result| (events, result));
                        in_flight.spawn(fut);
                    }
                    None => {
                        done = true;
                        debug!(endpoint = endpoint_url, "Consumed all incoming requests for endpoint I/O task. Completing any in-flight requests...");
                    },
                },
                Err(e) => {
                    let e: tower::BoxError = e.into();
                    error!(endpoint = endpoint_url, error = %e, error_source = ?e.source(), "Unexpected error when querying service for readiness.");
                    break;
                }
            },

            // Drive any in-flight requests to completion.
            maybe_result = in_flight.join_next(), if !in_flight.is_empty() => {
                let task_result = maybe_result.expect("in_flight marked as not being empty");
                match task_result {
                    Ok((events, Ok(response))) => {
                        let status = response.status();
                        if status.is_success() {
                            debug!(endpoint = endpoint_url, %status, "Request sent.");

                            telemetry.events_sent().increment(events as u64);
                            telemetry.events_sent_batch_size().record(events as f64);
                        } else {
                            telemetry.http_failed_send().increment(1);
                            telemetry.events_dropped_http().increment(events as u64);

                            match response.into_body().collect().await {
                                Ok(body) => {
                                    let body = body.to_bytes();
                                    let body_str = String::from_utf8_lossy(&body[..]);
                                    error!(endpoint = endpoint_url, %status, "Received non-success response. Body: {}", body_str);
                                }
                                Err(e) => {
                                    error!(endpoint = endpoint_url, %status, error = %e, "Failed to read response body of non-success response.");
                                }
                            }
                        }
                    }
                    Ok((_, Err(e))) => {
                        let e: tower::BoxError = e.into();
                        error!(endpoint = endpoint_url, error = %e, error_source = ?e.source(), "Failed to send request.");
                    }
                    Err(e) => {
                        error!(endpoint = endpoint_url, error = %e, error_source = ?e.source(), "Request task failed to run to completion.");
                    }
                }
            },

            else => break,
        }
    }

    debug!(
        endpoint = endpoint_url,
        "Requests channel for endpoint I/O task complete. Synchronizing on shutdown."
    );

    // Signal to the main I/O task that we've finished.
    task_barrier.wait().await;
}
