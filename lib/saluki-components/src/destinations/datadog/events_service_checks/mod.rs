use std::time::Duration;

use async_trait::async_trait;
use http::{uri::PathAndQuery, Method, Request, Uri};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr};
use saluki_config::{GenericConfiguration, RefreshableConfiguration};
use saluki_core::{
    components::{destinations::*, ComponentContext},
    observability::ComponentMetricsExt as _, pooling::ObjectPool,
};
use saluki_error::{ErrorContext as _, GenericError};
use saluki_event::{eventd::EventD, service_check::ServiceCheck, DataType, Event};
use saluki_io::{buf::{BytesBuffer, FrozenChunkedBytesBuffer}, compression::CompressionScheme, net::util::http::FixedBody};
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use stringtheory::MetaString;
use tokio::select;
use tracing::{debug, error};

use super::common::{
    config::{create_request_builder_buffer_pool, get_buffer_pool_minimum_maximum_size_bytes, ForwarderConfiguration}, io::TransactionForwarder, request_builder::RequestBuilder, telemetry::ComponentTelemetry, transaction::{Metadata, Transaction}
};

mod request_builder;
use self::request_builder::EventsServiceChecksEndpointEncoder;

const MAX_EVENTS_PER_PAYLOAD: usize = 100;
const MAX_SERVICE_CHECKS_PER_PAYLOAD: usize = 100;

const fn default_flush_timeout_secs() -> u64 {
    2
}

fn default_serializer_compressor_kind() -> String {
    DEFAULT_SERIALIZER_COMPRESSOR_KIND.to_owned()
}

const fn default_zstd_compressor_level() -> i32 {
    3
}

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

    /// Flush timeout for pending requests, in seconds.
    ///
    /// When the destination has written events/service checks to the in-flight request payload, but it has not yet
    /// reached the payload size limits that would force the payload to be flushed, the destination will wait for a
    /// period of time before flushing the in-flight request payload. This allows for the possibility of other events to
    /// be processed and written into the request payload, thereby maximizing the payload size and reducing the number
    /// of requests generated and sent overall.
    ///
    /// Defaults to 2 seconds.
    #[serde(default = "default_flush_timeout_secs")]
    flush_timeout_secs: u64,

    /// Compression kind to use for the request payloads.
    ///
    /// Defaults to `zstd`.
    #[serde(
        rename = "serializer_compressor_kind",
        default = "default_serializer_compressor_kind"
    )]
    compressor_kind: String,

    /// Compressor level to use when the compressor kind is `zstd`.
    ///
    /// Defaults to 3.
    #[serde(
        rename = "serializer_zstd_compressor_level",
        default = "default_zstd_compressor_level"
    )]
    zstd_compressor_level: i32,
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
            telemetry.clone(),
            metrics_builder,
        )?;
        let compression_scheme = CompressionScheme::new(&self.compressor_kind, self.zstd_compressor_level);

        // Create our request builders.
        let rb_buffer_pool = create_request_builder_buffer_pool("event_sc", &self.forwarder_config, request_builder::COMPRESSED_SIZE_LIMIT).await;

        let events_encoder = EventsServiceChecksEndpointEncoder::for_events();
        let mut events_request_builder =
            RequestBuilder::new(events_encoder, rb_buffer_pool.clone(), compression_scheme).await?;
        events_request_builder.with_max_inputs_per_payload(MAX_EVENTS_PER_PAYLOAD);

        let service_checks_encoder = EventsServiceChecksEndpointEncoder::for_service_checks();
        let mut service_checks_request_builder =
            RequestBuilder::new(service_checks_encoder, rb_buffer_pool.clone(), compression_scheme).await?;
        service_checks_request_builder.with_max_inputs_per_payload(MAX_SERVICE_CHECKS_PER_PAYLOAD);

        let flush_timeout = match self.flush_timeout_secs {
            // We always give ourselves a minimum flush timeout of 10ms to allow for some very minimal amount of
            // batching, while still practically flushing things almost immediately.
            0 => Duration::from_millis(10),
            secs => Duration::from_secs(secs),
        };

        Ok(Box::new(DatadogEventsServiceChecks {
            events_request_builder,
            service_checks_request_builder,
            forwarder,
            telemetry,
            flush_timeout,
        }))
    }
}

impl MemoryBounds for DatadogEventsServiceChecksConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // The request builder buffer pool is shared between both the events and the service checks request builder, so we
        // only count it once.
        let (pool_size_min_bytes, _) = get_buffer_pool_minimum_maximum_size_bytes(&self.forwarder_config, request_builder::COMPRESSED_SIZE_LIMIT);

        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built.
            .with_single_value::<DatadogEventsServiceChecks>("component struct")
            .with_fixed_amount("buffer pool", pool_size_min_bytes)
            // Capture the size of the requests channel.
            .with_array::<(usize, Request<String>)>("requests channel", 32);

        builder
            .firm()
            // This represents the potential growth of the buffer pool to allow for requests to continue to be built
            // while we're retrying the current request, and having to enqueue pending requests in memory.
            .with_expr(UsageExpr::sum(
                "buffer pool",
                UsageExpr::config(
                    "forwarder_retry_queue_payloads_max_size",
                    self.forwarder_config.retry().queue_max_size_bytes() as usize,
                ),
                UsageExpr::product(
                    "high priority queue",
                    UsageExpr::config(
                        "forwarder_high_prio_buffer_size",
                        self.forwarder_config.endpoint_buffer_size(),
                    ),
                    UsageExpr::constant("maximum compressed payload size", request_builder::COMPRESSED_SIZE_LIMIT),
                ),
            ))
            // Capture the size of the "split re-encode" buffers in the request builders, which is where we keep owned
            // versions of events/service checks that we encode in case we need to actually re-encode them during a split operation.
            .with_array::<EventD>("events split re-encode buffer", MAX_EVENTS_PER_PAYLOAD)
            .with_array::<ServiceCheck>("service checks split re-encode buffer", MAX_SERVICE_CHECKS_PER_PAYLOAD);
    }
}

pub struct DatadogEventsServiceChecks<O>
where
    O: ObjectPool<Item = BytesBuffer> + 'static,
{
    events_request_builder: RequestBuilder<EventsServiceChecksEndpointEncoder<EventD>, O>,
    service_checks_request_builder: RequestBuilder<EventsServiceChecksEndpointEncoder<ServiceCheck>, O>,
    forwarder: TransactionForwarder<FrozenChunkedBytesBuffer>,
    telemetry: ComponentTelemetry,
    flush_timeout: Duration,
}

#[async_trait]
impl<O> Destination for DatadogEventsServiceChecks<O>
where
    O: ObjectPool<Item = BytesBuffer> + 'static,
{
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let Self {
            events_request_builder,
            service_checks_request_builder,
            forwarder,
            telemetry,
            flush_timeout,
        } = *self;

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
                                    match build_eventd_transaction(&eventd) {
                                        Ok(transaction) => forwarder_handle.send_transaction(transaction).await?,
                                        Err(e) => {
                                            error!(error = %e, "Failed to create transaction for event.");
                                            continue;
                                        }
                                    }
                                }
                                Event::ServiceCheck(service_check) => {
                                    match build_service_check_transaction(&service_check) {
                                        Ok(transaction) => forwarder_handle.send_transaction(transaction).await?,
                                        Err(e) => {
                                            error!(error = %e, "Failed to create transaction for service check.");
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

fn build_transaction(data: String, relative_path: &'static str) -> Result<Transaction<FixedBody>, GenericError> {
    let request = Request::builder()
        .method(Method::POST)
        .uri(Uri::from(PathAndQuery::from_static(relative_path)))
        .header(http::header::CONTENT_TYPE, CONTENT_TYPE_JSON.clone())
        .body(FixedBody::new(data))
        .error_context("Failed to build request body.")?;

    Ok(Transaction::from_original(Metadata::from_event_count(1), request))
}

fn build_eventd_transaction(eventd: &EventD) -> Result<Transaction<FixedBody>, GenericError> {
    let json = serde_json::to_string(eventd).error_context("Failed to serialize eventd.")?;
    build_transaction(json, "/api/v1/events")
}

fn build_service_check_transaction(service_check: &ServiceCheck) -> Result<Transaction<FixedBody>, GenericError> {
    let json = serde_json::to_string(service_check).error_context("Failed to serialize service check.")?;
    build_transaction(json, "/api/v1/check_run")
}

fn get_events_checks_endpoint_name(uri: &Uri) -> Option<MetaString> {
    match uri.path() {
        "/api/v1/events" => Some(MetaString::from_static("events_v1")),
        "/api/v1/check_run" => Some(MetaString::from_static("check_run_v1")),
        _ => None,
    }
}
