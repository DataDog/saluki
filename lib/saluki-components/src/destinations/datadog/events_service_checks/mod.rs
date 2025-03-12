use async_trait::async_trait;
use http::{uri::PathAndQuery, HeaderValue, Method, Request, Uri};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::{GenericConfiguration, RefreshableConfiguration};
use saluki_core::{
    components::{destinations::*, ComponentContext},
    observability::ComponentMetricsExt as _,
};
use saluki_error::{ErrorContext as _, GenericError};
use saluki_event::{eventd::EventD, service_check::ServiceCheck, DataType, Event};
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use stringtheory::MetaString;
use tokio::select;
use tracing::{debug, error};

use super::common::{config::ForwarderConfiguration, io::TransactionForwarder, telemetry::ComponentTelemetry};

static CONTENT_TYPE_JSON: HeaderValue = HeaderValue::from_static("application/json");

/// Datadog Events and Service Checks destination.
///
/// Forwards events and service checks to the Datadog platform.
#[derive(Deserialize)]
pub struct DatadogEventsServiceChecksConfiguration {
    /// Forwarder configuration settings.
    ///
    /// See [`ForwarderConfiguration`] for more information about the available settings.
    #[serde(flatten)]
    forwarder_config: ForwarderConfiguration,

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
        let forwarder = TransactionForwarder::from_config(
            self.forwarder_config.clone(),
            self.config_refresher.clone(),
            get_events_checks_endpoint_name,
            telemetry,
            metrics_builder,
        )?;

        Ok(Box::new(DatadogEventsServiceChecks { forwarder }))
    }
}

impl MemoryBounds for DatadogEventsServiceChecksConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built.
            .with_single_value::<DatadogEventsServiceChecks>("component struct")
            // Capture the size of the requests channel.
            .with_array::<(usize, Request<String>)>("requests channel", 32);
    }
}

pub struct DatadogEventsServiceChecks {
    forwarder: TransactionForwarder<String>,
}

#[async_trait]
impl Destination for DatadogEventsServiceChecks {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let Self { forwarder } = *self;

        let mut health = context.take_health_handle();

        // Spawn our forwarder task to handle sending requests.
        let forwarder_handle = forwarder.spawn().await;

        health.mark_ready();
        debug!("Datadog Events and Service Checks destination started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(event_buffer) => {
                        for event in event_buffer {
                            match event {
                                Event::EventD(eventd) => {
                                    match build_eventd_request(&eventd) {
                                        Ok(request) => forwarder_handle.send_request(1, request).await?,
                                        Err(e) => {
                                            error!(error = %e, "Failed to create request for event.");
                                            continue;
                                        }
                                    }
                                }
                                Event::ServiceCheck(service_check) => {
                                    match build_service_check_request(&service_check) {
                                        Ok(request) => forwarder_handle.send_request(1, request).await?,
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
                    },
                    None => break,
                },
            }
        }

        // Signal the forwarder to shutdown, and wait until it does so.
        forwarder_handle.shutdown().await;

        debug!("Datadog Events Service Checks destination stopped.");
        Ok(())
    }
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
