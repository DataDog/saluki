use std::time::Duration;

use async_trait::async_trait;
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr};
use saluki_common::task::HandleExt as _;
use saluki_config::{GenericConfiguration, RefreshableConfiguration};
use saluki_core::data_model::event::{service_check::ServiceCheck, EventType};
use saluki_core::topology::EventsBuffer;
use saluki_core::{
    components::{destinations::*, ComponentContext},
    observability::ComponentMetricsExt as _,
    pooling::ElasticObjectPool,
};
use saluki_error::{ErrorContext as _, GenericError};
use saluki_io::{
    buf::{BytesBuffer, FrozenChunkedBytesBuffer},
    compression::CompressionScheme,
};
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use stringtheory::MetaString;
use tokio::{select, sync::mpsc, time::sleep};
use tracing::{debug, error};

use super::common::{
    config::ForwarderConfiguration,
    io::{create_request_builder_buffer_pool, get_buffer_pool_min_max_size_bytes, Handle, TransactionForwarder},
    request_builder::{EndpointEncoder, RequestBuilder},
    telemetry::ComponentTelemetry,
    transaction::{Metadata, Transaction},
};

const CHECK_RUN_V1_API_PATH: &str = "/api/v1/check_run";
const COMPRESSED_SIZE_LIMIT: usize = 3_200_000; // 3 MB
const UNCOMPRESSED_SIZE_LIMIT: usize = 62_914_560; // 60 MB
const DEFAULT_SERIALIZER_COMPRESSOR_KIND: &str = "zstd";
const MAX_SERVICE_CHECKS_PER_PAYLOAD: usize = 100;

static CONTENT_TYPE_JSON: HeaderValue = HeaderValue::from_static("application/json");

const fn default_flush_timeout_secs() -> u64 {
    2
}

fn default_serializer_compressor_kind() -> String {
    DEFAULT_SERIALIZER_COMPRESSOR_KIND.to_owned()
}

const fn default_zstd_compressor_level() -> i32 {
    3
}

/// Datadog Service Checks destination.
///
/// Forwards service checks to the Datadog platform.
#[derive(Deserialize)]
pub struct DatadogServiceChecksConfiguration {
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

impl DatadogServiceChecksConfiguration {
    /// Creates a new `DatadogServiceChecksConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }

    /// Add option to retrieve configuration values from a `RefreshableConfiguration`.
    pub fn add_refreshable_configuration(&mut self, refresher: RefreshableConfiguration) {
        self.config_refresher = Some(refresher);
    }
}

#[async_trait]
impl DestinationBuilder for DatadogServiceChecksConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::ServiceCheck
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let forwarder = TransactionForwarder::from_config(
            context,
            self.forwarder_config.clone(),
            self.config_refresher.clone(),
            get_service_checks_endpoint_name,
            telemetry.clone(),
            metrics_builder,
        )?;
        let compression_scheme = CompressionScheme::new(&self.compressor_kind, self.zstd_compressor_level);

        // Create our request builder.
        let rb_buffer_pool =
            create_request_builder_buffer_pool("service_checks", &self.forwarder_config, COMPRESSED_SIZE_LIMIT).await;

        let mut request_builder =
            RequestBuilder::new(ServiceChecksEndpointEncoder, rb_buffer_pool, compression_scheme).await?;
        request_builder.with_max_inputs_per_payload(MAX_SERVICE_CHECKS_PER_PAYLOAD);

        let flush_timeout = match self.flush_timeout_secs {
            // We always give ourselves a minimum flush timeout of 10ms to allow for some very minimal amount of
            // batching, while still practically flushing things almost immediately.
            0 => Duration::from_millis(10),
            secs => Duration::from_secs(secs),
        };

        Ok(Box::new(DatadogServiceChecks {
            request_builder,
            forwarder,
            telemetry,
            flush_timeout,
        }))
    }
}

impl MemoryBounds for DatadogServiceChecksConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        let (pool_size_min_bytes, _) =
            get_buffer_pool_min_max_size_bytes(&self.forwarder_config, COMPRESSED_SIZE_LIMIT);

        builder
            .minimum()
            .with_single_value::<DatadogServiceChecks>("component struct")
            .with_fixed_amount("buffer pool", pool_size_min_bytes)
            .with_array::<Transaction<FrozenChunkedBytesBuffer>>("requests channel", 32);

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
                    UsageExpr::constant("maximum compressed payload size", COMPRESSED_SIZE_LIMIT),
                ),
            ))
            // Capture the size of the "split re-encode" buffer in the request builder, which is where we keep owned
            // versions of events that we encode in case we need to actually re-encode them during a split operation.
            .with_array::<ServiceCheck>("events split re-encode buffer", MAX_SERVICE_CHECKS_PER_PAYLOAD);
    }
}

pub struct DatadogServiceChecks {
    request_builder: RequestBuilder<ServiceChecksEndpointEncoder, ElasticObjectPool<BytesBuffer>>,
    forwarder: TransactionForwarder<FrozenChunkedBytesBuffer>,
    telemetry: ComponentTelemetry,
    flush_timeout: Duration,
}

#[async_trait]
impl Destination for DatadogServiceChecks {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let Self {
            request_builder,
            forwarder,
            telemetry,
            flush_timeout,
        } = *self;

        let mut health = context.take_health_handle();

        // Spawn our forwarder task to handle sending requests.
        let forwarder_handle = forwarder.spawn().await;

        // Spawn our request builder task.
        let (builder_tx, builder_rx) = mpsc::channel(8);
        let request_builder_fut =
            run_request_builder(request_builder, telemetry, builder_rx, forwarder_handle, flush_timeout);
        let request_builder_handle = context
            .topology_context()
            .global_thread_pool()
            .spawn_traced_named("dd-service-checks-request-builder", request_builder_fut);

        health.mark_ready();
        debug!("Datadog Service Checks destination started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_event_buffer = context.events().next() => match maybe_event_buffer {
                    Some(event_buffer) => builder_tx.send(event_buffer).await
                        .error_context("Failed to send event buffer to request builder task.")?,
                    None => break,
                },
            }
        }

        // Drop the request builder channel, which allows the request builder task to naturally shut down once it has
        // received and built all requests. This includes also triggering the forwarder to shutdown, and waiting for it
        // to do so.
        //
        // We wait for the request builder task to signal back to us that it has stopped before letting ourselves return.
        drop(builder_tx);
        match request_builder_handle.await {
            Ok(Ok(())) => debug!("Request builder task stopped."),
            Ok(Err(e)) => error!(error = %e, "Request builder task failed."),
            Err(e) => error!(error = %e, "Request builder task panicked."),
        }

        debug!("Datadog Service Checks destination stopped.");

        Ok(())
    }
}

async fn run_request_builder(
    mut request_builder: RequestBuilder<ServiceChecksEndpointEncoder, ElasticObjectPool<BytesBuffer>>,
    telemetry: ComponentTelemetry, mut request_builder_rx: mpsc::Receiver<EventsBuffer>,
    forwarder_handle: Handle<FrozenChunkedBytesBuffer>, flush_timeout: Duration,
) -> Result<(), GenericError> {
    let mut pending_flush = false;
    let pending_flush_timeout = sleep(flush_timeout);
    tokio::pin!(pending_flush_timeout);

    loop {
        select! {
            Some(event_buffer) = request_builder_rx.recv() => {
                for event in event_buffer {
                    let service_check = match event.try_into_service_check() {
                        Some(service_check) => service_check,
                        None => continue,
                    };

                    // Encode the event. If we get it back, that means the current request is full, and we need to
                    // flush it before we can try to encode the event again... so we'll hold on to it in that case
                    // before flushing and trying to encode it again.
                    let service_check_to_retry = match request_builder.encode(service_check).await {
                        Ok(None) => continue,
                        Ok(Some(service_check)) => service_check,
                        Err(e) => {
                            error!(error = %e, "Failed to encode service check.");
                            telemetry.events_dropped_encoder().increment(1);
                            continue;
                        }
                    };


                    let maybe_requests = request_builder.flush().await;
                    if maybe_requests.is_empty() {
                        panic!("builder told us to flush, but gave us nothing");
                    }

                    for maybe_request in maybe_requests {
                        match maybe_request {
                            Ok((events, request)) => {
                                let transaction = Transaction::from_original(Metadata::from_event_count(events), request);
                                forwarder_handle.send_transaction(transaction).await?
                            },

                            // TODO: Increment a counter here that events were dropped due to a flush failure.
                            Err(e) => if e.is_recoverable() {
                                // If the error is recoverable, we'll hold on to the event to retry it later.
                                continue;
                            } else {
                                return Err(GenericError::from(e).context("Failed to flush request."));
                            }
                        }
                    }

                    // Now try to encode the event again. If it fails again, we'll just log it because it shouldn't
                    // be possible to fail at this point, otherwise we would have already caught that the first
                    // time.
                    if let Err(e) = request_builder.encode(service_check_to_retry).await {
                        error!(error = %e, "Failed to encode service check.");
                        telemetry.events_dropped_encoder().increment(1);
                    }
                }

                debug!("Processed event buffer.");

                // If we're not already pending a flush, we'll start the countdown.
                if !pending_flush {
                    pending_flush_timeout.as_mut().reset(tokio::time::Instant::now() + flush_timeout);
                    pending_flush = true;
                }
            },
            _ = &mut pending_flush_timeout, if pending_flush => {
                debug!("Flushing pending request(s).");

                pending_flush = false;

                // Once we've encoded and written all metrics, we flush the request builders to generate a request with
                // anything left over. Again, we'll enqueue those requests to be sent immediately.
                let maybe_requests = request_builder.flush().await;
                for maybe_request in maybe_requests {
                    match maybe_request {
                        Ok((events, request)) => {
                            debug!("Flushed request from series request builder. Sending to I/O task...");
                            let transaction = Transaction::from_original(Metadata::from_event_count(events), request);
                            forwarder_handle.send_transaction(transaction).await?
                        },

                        // TODO: Increment a counter here that events were dropped due to a flush failure.
                        Err(e) => if e.is_recoverable() {
                            // If the error is recoverable, we'll hold on to the event to retry it later.
                            continue;
                        } else {
                            return Err(GenericError::from(e).context("Failed to flush request."));
                        }
                    }
                }

                debug!("All flushed requests sent to I/O task. Waiting for next event buffer...");
            },

            // Event buffers channel has been closed, and we have no pending flushing, so we're all done.
            else => break,
        }
    }

    debug!("Waiting for forwarder to shutdown...");

    // Signal the forwarder to shutdown, and wait for it to do so.
    forwarder_handle.shutdown().await;

    Ok(())
}

fn get_service_checks_endpoint_name(uri: &Uri) -> Option<MetaString> {
    match uri.path() {
        CHECK_RUN_V1_API_PATH => Some(MetaString::from_static("check_run_v1")),
        _ => None,
    }
}

#[derive(Debug)]
struct ServiceChecksEndpointEncoder;

impl EndpointEncoder for ServiceChecksEndpointEncoder {
    type Input = ServiceCheck;
    type EncodeError = serde_json::Error;

    fn encoder_name() -> &'static str {
        "service_check"
    }

    fn compressed_size_limit(&self) -> usize {
        COMPRESSED_SIZE_LIMIT
    }

    fn uncompressed_size_limit(&self) -> usize {
        UNCOMPRESSED_SIZE_LIMIT
    }

    fn encode(&mut self, input: &Self::Input, buffer: &mut Vec<u8>) -> Result<(), Self::EncodeError> {
        serde_json::to_writer(buffer, input)
    }

    fn get_payload_prefix(&self) -> Option<&'static [u8]> {
        Some(b"[")
    }

    fn get_payload_suffix(&self) -> Option<&'static [u8]> {
        Some(b"]")
    }

    fn get_input_separator(&self) -> Option<&'static [u8]> {
        Some(b",")
    }

    fn endpoint_uri(&self) -> Uri {
        PathAndQuery::from_static(CHECK_RUN_V1_API_PATH).into()
    }

    fn endpoint_method(&self) -> Method {
        Method::POST
    }

    fn content_type(&self) -> HeaderValue {
        CONTENT_TYPE_JSON.clone()
    }
}
