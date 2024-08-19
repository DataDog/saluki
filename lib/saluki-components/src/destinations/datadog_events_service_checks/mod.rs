use std::error::Error as _;

use async_trait::async_trait;
use http::{Request, Uri};
use http_body_util::BodyExt;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_core::{components::destinations::*, spawn_traced};
use saluki_error::GenericError;
use saluki_event::{DataType, Event};
use saluki_io::net::client::http::HttpClient;
use serde::Deserialize;
use tokio::{
    select,
    sync::{mpsc, oneshot},
};
use tracing::{debug, error};
mod request_builder;
use request_builder::{EventsServiceChecksEndpoint, RequestBuilder};

const DEFAULT_SITE: &str = "datadoghq.com";

fn default_site() -> String {
    DEFAULT_SITE.to_owned()
}

/// Datadog Events and Service Checks destination.
///
/// Forwards events and service checks to the Datadog platform.
#[derive(Deserialize)]
pub struct DatadogEventsServiceChecksConfiguration {
    /// The API key to use.
    api_key: String,

    /// The site to send events / service checks to.
    ///
    /// This is the base domain for the Datadog site in which the API key originates from. This will generally be a
    /// portion of the domain used to access the Datadog UI, such as `datadoghq.com` or `us5.datadoghq.com`.
    ///
    /// Defaults to `datadoghq.com`.
    #[serde(default = "default_site")]
    site: String,

    /// The full URL base to send events / service checks to.
    ///
    /// This takes precedence over `site`, and is not altered in any way. This can be useful to specifying the exact
    /// endpoint used, such as when looking to change the scheme (e.g. `http` vs `https`) or specifying a custom port,
    /// which are both useful when proxying traffic to an intermediate destination before forwarding to Datadog.
    ///
    /// Defaults to unset.
    #[serde(default)]
    dd_url: Option<String>,
}

impl DatadogEventsServiceChecksConfiguration {
    /// Creates a new `DatadogEventsServieCheckConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }

    fn api_base(&self) -> Result<Uri, GenericError> {
        match &self.dd_url {
            Some(url) => Uri::try_from(url).map_err(Into::into),
            None => {
                let site = if self.site.is_empty() {
                    DEFAULT_SITE
                } else {
                    self.site.as_str()
                };
                let authority = format!("api.{}", site);

                Uri::builder()
                    .scheme("https")
                    .authority(authority.as_str())
                    .path_and_query("/")
                    .build()
                    .map_err(Into::into)
            }
        }
    }
}

#[async_trait]
impl DestinationBuilder for DatadogEventsServiceChecksConfiguration {
    fn input_data_type(&self) -> DataType {
        DataType::EventD | DataType::ServiceCheck
    }

    async fn build(&self) -> Result<Box<dyn Destination + Send>, GenericError> {
        let http_client = HttpClient::https()?;

        let api_base = self.api_base()?;

        let events_request_builder = RequestBuilder::new(
            self.api_key.clone(),
            api_base.clone(),
            EventsServiceChecksEndpoint::Events,
        )?;
        let service_checks_request_builder = RequestBuilder::new(
            self.api_key.clone(),
            api_base,
            EventsServiceChecksEndpoint::ServiceChecks,
        )?;
        Ok(Box::new(DatadogEventsServiceChecks {
            http_client,
            events_request_builder,
            service_checks_request_builder,
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
    http_client: HttpClient<HttpsConnector<HttpConnector>, String>,
    events_request_builder: RequestBuilder,
    service_checks_request_builder: RequestBuilder,
}

#[async_trait]
impl Destination for DatadogEventsServiceChecks {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), ()> {
        let Self {
            http_client,
            mut events_request_builder,
            mut service_checks_request_builder,
        } = *self;

        let mut health = context.take_health_handle();

        // Spawn our IO task to handle sending requests.
        let (io_shutdown_tx, io_shutdown_rx) = oneshot::channel();
        let (requests_tx, requests_rx) = mpsc::channel(32);
        spawn_traced(run_io_loop(requests_rx, io_shutdown_tx, http_client));

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
                                        let request_builder = &mut events_request_builder;
                                        let json = serde_json::to_string(&eventd).unwrap();

                                        match request_builder.create_request(json) {
                                            Ok(request) => {
                                                if requests_tx.send((1, request)).await.is_err() {
                                                    error!("Failed to send request to IO task: receiver dropped.");
                                                    return Err(());
                                                }
                                            }
                                            Err(e) => {
                                                error!(error = %e, "Failed to create request for event.");
                                                continue;
                                            }
                                        }
                                    }
                                    Event::ServiceCheck(service_check) => {
                                        let request_builder = &mut service_checks_request_builder;
                                        let json = serde_json::to_string(&service_check).unwrap();
                                        match request_builder.create_request(json) {
                                            Ok(request) => {
                                                if requests_tx.send((1, request)).await.is_err() {
                                                    error!("Failed to send request to IO task: receiver dropped.");
                                                    return Err(());
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

async fn run_io_loop(
    mut requests_rx: mpsc::Receiver<(usize, Request<String>)>, io_shutdown_tx: oneshot::Sender<()>,
    http_client: HttpClient<HttpsConnector<HttpConnector>, String>,
) {
    // Loop and process all incoming requests.
    while let Some((_events_count, request)) = requests_rx.recv().await {
        // TODO: We currently are not emitting any metrics.
        match http_client.send(request).await {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    debug!(%status, "Request sent.");
                } else {
                    match response.into_body().collect().await {
                        Ok(body) => {
                            let body = body.to_bytes();
                            let body_str = String::from_utf8_lossy(&body[..]);
                            error!(%status, "Received non-success response. Body: {}", body_str);
                        }
                        Err(e) => {
                            error!(%status, error = %e, "Failed to read response body of non-success response.");
                        }
                    }
                }
            }
            Err(e) => {
                error!(error = %e, error_source = ?e.source(), "Failed to send request.");
            }
        }
    }

    // Signal back to the main task that we've stopped.
    let _ = io_shutdown_tx.send(());
}
