use async_trait::async_trait;
use http::{uri::PathAndQuery, HeaderValue, Method, Request, Uri};
use http_body::Body;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::{GenericConfiguration, RefreshableConfiguration};
use saluki_core::{
    components::{destinations::*, ComponentContext},
    observability::ComponentMetricsExt as _,
    task::spawn_traced,
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_event::{eventd::EventD, service_check::ServiceCheck, DataType, Event};
use saluki_io::net::{client::http::HttpClient, util::retry::agent::DatadogAgentForwarderRetryConfiguration};
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use stringtheory::MetaString;
use tokio::{
    select,
    sync::{mpsc, oneshot},
};
use tower::{BoxError, Service, ServiceBuilder};
use tracing::{debug, error};

use super::common::{
    endpoints::{EndpointConfiguration, ResolvedEndpoint},
    middleware::{for_resolved_endpoint, with_version_info},
    telemetry::ComponentTelemetry,
};

static CONTENT_TYPE_JSON: HeaderValue = HeaderValue::from_static("application/json");

/// Datadog Events and Service Checks destination.
///
/// Forwards events and service checks to the Datadog platform.
#[derive(Deserialize)]
pub struct DatadogEventsServiceChecksConfiguration {
    /// Endpoint configuration settings
    ///
    /// See [`EndpointConfiguration`] for more information about the available settings.
    #[serde(flatten)]
    endpoint_config: EndpointConfiguration,

    /// Retry configuration settings.
    ///
    /// See [`DatadogAgentForwarderRetryConfiguration`] for more information about the available settings.
    #[serde(flatten)]
    retry_config: DatadogAgentForwarderRetryConfiguration,

    #[serde(skip)]
    config_refresher: Option<RefreshableConfiguration>,
}

impl DatadogEventsServiceChecksConfiguration {
    /// Creates a new `DatadogEventsServiceCheckConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }

    /// Add option to retrieve configuration values from a `RefreshableConfiguration`.
    pub fn add_refreshable_configuration(&mut self, refresher: RefreshableConfiguration) {
        self.config_refresher = Some(refresher);
    }
}

#[async_trait]
impl DestinationBuilder for DatadogEventsServiceChecksConfiguration {
    fn input_data_type(&self) -> DataType {
        DataType::EventD | DataType::ServiceCheck
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(context);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);

        let service = HttpClient::builder()
            .with_retry_policy(self.retry_config.into_default_http_retry_policy())
            .with_bytes_sent_counter(metrics_builder.register_debug_counter("component_bytes_sent_total"))
            .with_endpoint_telemetry(metrics_builder, Some(get_events_checks_endpoint_name))
            .build()?;

        // Resolve all endpoints that we'll be forwarding metrics to.
        let endpoints = self
            .endpoint_config
            .build_resolved_endpoints(self.config_refresher.clone())?;

        Ok(Box::new(DatadogEventsServiceChecks {
            service,
            telemetry,
            endpoints,
        }))
    }
}

impl MemoryBounds for DatadogEventsServiceChecksConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built.
            .with_single_value::<DatadogEventsServiceChecks>()
            // Capture the size of the requests channel.
            .with_array::<(usize, Request<String>)>(32);
    }
}

pub struct DatadogEventsServiceChecks {
    service: HttpClient<String>,
    telemetry: ComponentTelemetry,
    endpoints: Vec<ResolvedEndpoint>,
}

#[async_trait]
impl Destination for DatadogEventsServiceChecks {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let Self {
            service,
            telemetry,
            endpoints,
        } = *self;

        let mut health = context.take_health_handle();

        // Spawn our IO task to handle sending requests.
        let (io_shutdown_tx, io_shutdown_rx) = oneshot::channel();
        let (requests_tx, requests_rx) = mpsc::channel(32);
        spawn_traced(run_io_loop(requests_rx, io_shutdown_tx, service, telemetry, endpoints));

        health.mark_ready();
        debug!("Datadog Events and Service Checks destination started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next_ready() => match maybe_events {
                    Some(event_buffers) => {
                        debug!(event_buffers_len = event_buffers.len(), "Received event buffers.");

                        for event_buffer in event_buffers {
                            debug!(events_len = event_buffer.len(), "Processing event buffer.");

                            for event in event_buffer {
                                match event {
                                    Event::EventD(eventd) => {
                                        match build_eventd_request(&eventd) {
                                            Ok(request) => {
                                                if requests_tx.send((1, request)).await.is_err() {
                                                    return Err(generic_error!("Failed to send request to IO task: receiver dropped."));
                                                }
                                            }
                                            Err(e) => {
                                                error!(error = %e, "Failed to create request for event.");
                                                continue;
                                            }
                                        }
                                    }
                                    Event::ServiceCheck(service_check) => {
                                        match build_service_check_request(&service_check) {
                                            Ok(request) => {
                                                if requests_tx.send((1, request)).await.is_err() {
                                                    return Err(generic_error!("Failed to send request to IO task: receiver dropped."));
                                                }
                                            }
                                            Err(e) => {
                                                error!(error = %e, "Failed to create request for service check.");
                                                continue;
                                            }
                                        }
                                    }
                                    _ => {
                                        error!("Received non eventd/service checks event type.")
                                    }
                                }
                            }
                        }

                        debug!("All event buffers processed.");
                    },
                    None => break,
                },
            }
        }

        // Drop the requests channel, which allows the IO task to naturally shut down once it has received and sent all
        // requests. We then wait for it to signal back to us that it has stopped before exiting ourselves.
        drop(requests_tx);
        let _ = io_shutdown_rx.await;

        debug!("Datadog Events Service Checks destination stopped.");
        Ok(())
    }
}

async fn run_io_loop<S, B>(
    mut requests_rx: mpsc::Receiver<(usize, Request<B>)>, io_shutdown_tx: oneshot::Sender<()>, service: S,
    telemetry: ComponentTelemetry, mut endpoints: Vec<ResolvedEndpoint>,
) where
    S: Service<Request<B>, Response = hyper::Response<Incoming>> + Send + 'static,
    S::Future: Send,
    S::Error: Send + Into<BoxError>,
    B: Body,
{
    // TODO: We currently only handle a single endpoint.
    let endpoint = endpoints.remove(0);
    let endpoint_url = endpoint.endpoint().to_string();
    debug!(endpoint = endpoint_url, "Starting endpoint I/O task.");

    // Build our endpoint service.
    //
    // This is where we'll modify the incoming request for our our specific endpoint, such as setting the host portion
    // of the URI, adding the API key as a header, and so on.
    let mut service = ServiceBuilder::new()
        // Set the request's URI to the endpoint's URI, and add the API key as a header.
        .map_request(for_resolved_endpoint::<B>(endpoint))
        // Set the User-Agent and DD-Agent-Version headers indicating the version of the data plane sending the request.
        .map_request(with_version_info::<B>())
        .service(service);

    // Process all requests until the channel is closed.
    while let Some((event_count, request)) = requests_rx.recv().await {
        match service.call(request).await {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    debug!(endpoint = endpoint_url, %status, "Request sent.");

                    telemetry.events_sent().increment(event_count as u64);
                } else {
                    telemetry.http_failed_send().increment(1);
                    telemetry.events_dropped_http().increment(event_count as u64);

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
            Err(e) => {
                let e: tower::BoxError = e.into();
                error!(endpoint = endpoint_url, error = %e, error_source = ?e.source(), "Failed to send request.");
            }
        }
    }

    debug!(
        endpoint = endpoint_url,
        "Requests channel for endpoint I/O task complete."
    );

    // Signal back to the main task that we've stopped.
    let _ = io_shutdown_tx.send(());
}

fn build_request(data: String, relative_path: &'static str) -> Result<Request<String>, GenericError> {
    Request::builder()
        .method(Method::POST)
        .uri(Uri::from(PathAndQuery::from_static(relative_path)))
        .header(http::header::CONTENT_TYPE, CONTENT_TYPE_JSON.clone())
        .body(data)
        .error_context("Failed to build request body.")
}

fn build_eventd_request(eventd: &EventD) -> Result<Request<String>, GenericError> {
    let json = serde_json::to_string(eventd).error_context("Failed to serialize eventd.")?;
    build_request(json, "/api/v1/events")
}

fn build_service_check_request(service_check: &ServiceCheck) -> Result<Request<String>, GenericError> {
    let json = serde_json::to_string(service_check).error_context("Failed to serialize service check.")?;
    build_request(json, "/api/v1/check_run")
}

fn get_events_checks_endpoint_name(uri: &Uri) -> Option<MetaString> {
    match uri.path() {
        "/api/v1/events" => Some(MetaString::from_static("events_v1")),
        "/api/v1/check_run" => Some(MetaString::from_static("check_run_v1")),
        _ => None,
    }
}
