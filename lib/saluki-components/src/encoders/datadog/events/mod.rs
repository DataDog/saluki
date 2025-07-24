use std::time::Duration;

use async_trait::async_trait;
use datadog_protos::events as proto;
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use protobuf::{rt::WireType, CodedOutputStream};
use saluki_common::task::HandleExt as _;
use saluki_config::GenericConfiguration;
use saluki_context::tags::{Tag, TagsExt as _};
use saluki_core::{
    components::{encoders::*, ComponentContext},
    data_model::{
        event::{eventd::EventD, EventType},
        payload::{HttpPayload, Payload, PayloadMetadata, PayloadType},
    },
    observability::ComponentMetricsExt as _,
    topology::{EventsBuffer, PayloadsBuffer},
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::compression::CompressionScheme;
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use tokio::{select, sync::mpsc, time::sleep};
use tracing::{debug, error, warn};

use crate::common::datadog::{
    io::RB_BUFFER_CHUNK_SIZE,
    request_builder::{EndpointEncoder, RequestBuilder},
    telemetry::ComponentTelemetry,
    DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT, DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT,
};

const DEFAULT_SERIALIZER_COMPRESSOR_KIND: &str = "zstd";
const MAX_EVENTS_PER_PAYLOAD: usize = 100;
const EVENTS_FIELD_NUMBER: u32 = 1;

static CONTENT_TYPE_PROTOBUF: HeaderValue = HeaderValue::from_static("application/x-protobuf");

const fn default_flush_timeout_secs() -> u64 {
    2
}

fn default_serializer_compressor_kind() -> String {
    DEFAULT_SERIALIZER_COMPRESSOR_KIND.to_owned()
}

const fn default_zstd_compressor_level() -> i32 {
    3
}

/// Datadog Events encoder.
///
/// Generates Datadog Events payloads for the Datadog platform.
#[derive(Deserialize)]
pub struct DatadogEventsConfiguration {
    /// Flush timeout for pending requests, in seconds.
    ///
    /// When the encoder has written events to the in-flight request payload, but it has not yet reached the payload
    /// size limits that would force the payload to be flushed, the encoder will wait for a period of time before
    /// flushing the in-flight request payload. This allows for the possibility of other events to be processed and
    /// written into the request payload, thereby maximizing the payload size and reducing the number of requests
    /// generated and sent overall.
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

impl DatadogEventsConfiguration {
    /// Creates a new `DatadogEventsConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }
}

#[async_trait]
impl EncoderBuilder for DatadogEventsConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::EventD
    }

    fn output_payload_type(&self) -> PayloadType {
        PayloadType::Http
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Encoder + Send>, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let compression_scheme = CompressionScheme::new(&self.compressor_kind, self.zstd_compressor_level);

        // Create our request builder.
        let mut request_builder =
            RequestBuilder::new(EventsEndpointEncoder, compression_scheme, RB_BUFFER_CHUNK_SIZE).await?;
        request_builder.with_max_inputs_per_payload(MAX_EVENTS_PER_PAYLOAD);

        let flush_timeout = match self.flush_timeout_secs {
            // We always give ourselves a minimum flush timeout of 10ms to allow for some very minimal amount of
            // batching, while still practically flushing things almost immediately.
            0 => Duration::from_millis(10),
            secs => Duration::from_secs(secs),
        };

        Ok(Box::new(DatadogEvents {
            request_builder,
            telemetry,
            flush_timeout,
        }))
    }
}

impl MemoryBounds for DatadogEventsConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // TODO: How do we properly represent the requests we can generate that may be sitting around in-flight?
        //
        // Theoretically, we'll end up being limited by the size of the downstream forwarder's interconnect, and however
        // many payloads it will buffer internally... so realistically the firm limit boils down to the forwarder itself
        // but we'll have a hard time in the forwarder knowing the maximum size of any given payload being sent in, which
        // then makes it hard to calculate a proper firm bound even though we know the rest of the values required to
        // calculate the firm bound.
        builder
            .minimum()
            .with_single_value::<DatadogEvents>("component struct")
            .with_array::<EventsBuffer>("request builder events channel", 8)
            .with_array::<PayloadsBuffer>("request builder payloads channel", 8);

        builder
            .firm()
            // Capture the size of the "split re-encode" buffer in the request builder, which is where we keep owned
            // versions of events that we encode in case we need to actually re-encode them during a split operation.
            .with_array::<EventD>("events split re-encode buffer", MAX_EVENTS_PER_PAYLOAD);
    }
}

pub struct DatadogEvents {
    request_builder: RequestBuilder<EventsEndpointEncoder>,
    telemetry: ComponentTelemetry,
    flush_timeout: Duration,
}

#[async_trait]
impl Encoder for DatadogEvents {
    async fn run(mut self: Box<Self>, mut context: EncoderContext) -> Result<(), GenericError> {
        let Self {
            request_builder,
            telemetry,
            flush_timeout,
        } = *self;

        let mut health = context.take_health_handle();

        // Spawn our request builder task.
        let (events_tx, events_rx) = mpsc::channel(8);
        let (payloads_tx, mut payloads_rx) = mpsc::channel(8);
        let request_builder_fut =
            run_request_builder(request_builder, telemetry, events_rx, payloads_tx, flush_timeout);
        let request_builder_handle = context
            .topology_context()
            .global_thread_pool()
            .spawn_traced_named("dd-events-request-builder", request_builder_fut);

        health.mark_ready();
        debug!("Datadog Events encoder started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_event_buffer = context.events().next() => match maybe_event_buffer {
                    Some(event_buffer) => events_tx.send(event_buffer).await
                        .error_context("Failed to send event buffer to request builder task.")?,
                    None => break,
                },
                maybe_payload = payloads_rx.recv() => match maybe_payload {
                    Some(payload) => {
                        if let Err(e) = context.dispatcher().dispatch(payload).await {
                            error!("Failed to dispatch payload: {}", e);
                        }
                    }
                    None => {
                        warn!("Payload channel closed. Request builder task likely stopped due to error.");
                        break
                    },
                },
            }
        }

        // Drop the request builder events channel, which allows the request builder task to naturally shut down once it has
        // received and built all requests. We wait for its task handle to complete before letting ourselves return.
        drop(events_tx);
        match request_builder_handle.await {
            Ok(Ok(())) => debug!("Request builder task stopped."),
            Ok(Err(e)) => error!(error = %e, "Request builder task failed."),
            Err(e) => error!(error = %e, "Request builder task panicked."),
        }

        debug!("Datadog Events encoder stopped.");

        Ok(())
    }
}

async fn run_request_builder(
    mut request_builder: RequestBuilder<EventsEndpointEncoder>, telemetry: ComponentTelemetry,
    mut events_rx: mpsc::Receiver<EventsBuffer>, payloads_tx: mpsc::Sender<PayloadsBuffer>, flush_timeout: Duration,
) -> Result<(), GenericError> {
    let mut pending_flush = false;
    let pending_flush_timeout = sleep(flush_timeout);
    tokio::pin!(pending_flush_timeout);

    loop {
        select! {
            Some(event_buffer) = events_rx.recv() => {
                for event in event_buffer {
                    let eventd = match event.try_into_eventd() {
                        Some(eventd) => eventd,
                        None => continue,
                    };

                    // Encode the event. If we get it back, that means the current request is full, and we need to
                    // flush it before we can try to encode the event again... so we'll hold on to it in that case
                    // before flushing and trying to encode it again.
                    let eventd_to_retry = match request_builder.encode(eventd).await {
                        Ok(None) => continue,
                        Ok(Some(eventd)) => eventd,
                        Err(e) => {
                            error!(error = %e, "Failed to encode event.");
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
                                let payload_meta = PayloadMetadata::from_event_count(events);
                                let http_payload = HttpPayload::new(payload_meta,request);
                                let payload = Payload::Http(http_payload);

                                payloads_tx.send(payload).await
                                    .map_err(|_| generic_error!("Failed to send payload to encoder."))?;
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
                    if let Err(e) = request_builder.encode(eventd_to_retry).await {
                        error!(error = %e, "Failed to encode event.");
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
                            debug!("Flushed request from events request builder.");

                            let payload_meta = PayloadMetadata::from_event_count(events);
                            let http_payload = HttpPayload::new(payload_meta,request);
                            let payload = Payload::Http(http_payload);

                            payloads_tx.send(payload).await
                                .map_err(|_| generic_error!("Failed to send payload to encoder."))?;
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

                debug!("All flushed requests sent. Waiting for next event buffer...");
            },

            // Event buffers channel has been closed, and we have no pending flushing, so we're all done.
            else => break,
        }
    }

    Ok(())
}

#[derive(Debug)]
struct EventsEndpointEncoder;

impl EndpointEncoder for EventsEndpointEncoder {
    type Input = EventD;
    type EncodeError = protobuf::Error;

    fn encoder_name() -> &'static str {
        "events"
    }

    fn compressed_size_limit(&self) -> usize {
        DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT
    }

    fn uncompressed_size_limit(&self) -> usize {
        DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT
    }

    fn encode(&mut self, input: &Self::Input, buffer: &mut Vec<u8>) -> Result<(), Self::EncodeError> {
        encode_and_write_eventd(input, buffer)
    }

    fn endpoint_uri(&self) -> Uri {
        PathAndQuery::from_static("/api/v1/events_batch").into()
    }

    fn endpoint_method(&self) -> Method {
        Method::POST
    }

    fn content_type(&self) -> HeaderValue {
        CONTENT_TYPE_PROTOBUF.clone()
    }
}

fn encode_and_write_eventd(eventd: &EventD, buf: &mut Vec<u8>) -> Result<(), protobuf::Error> {
    let mut output_stream = CodedOutputStream::vec(buf);

    // Write the field tag.
    output_stream.write_tag(EVENTS_FIELD_NUMBER, WireType::LengthDelimited)?;

    // Write the message.
    let encoded_eventd = encode_eventd(eventd);
    output_stream.write_message_no_tag(&encoded_eventd)
}

fn encode_eventd(eventd: &EventD) -> proto::Event {
    let mut event = proto::Event::new();
    event.set_title(eventd.title().into());
    event.set_text(eventd.text().into());

    if let Some(timestamp) = eventd.timestamp() {
        event.set_ts(timestamp as i64);
    }

    if let Some(priority) = eventd.priority() {
        event.set_priority(priority.as_str().into());
    }

    if let Some(alert_type) = eventd.alert_type() {
        event.set_alert_type(alert_type.as_str().into());
    }

    if let Some(hostname) = eventd.hostname() {
        event.set_host(hostname.into());
    }

    if let Some(aggregation_key) = eventd.aggregation_key() {
        event.set_aggregation_key(aggregation_key.into());
    }

    if let Some(source_type_name) = eventd.source_type_name() {
        event.set_source_type_name(source_type_name.into());
    }

    let deduplicated_tags = get_deduplicated_tags(eventd);

    event.set_tags(deduplicated_tags.map(|tag| tag.as_str().into()).collect());

    event
}

fn get_deduplicated_tags(eventd: &EventD) -> impl Iterator<Item = &Tag> {
    eventd.tags().into_iter().chain(eventd.origin_tags()).deduplicated()
}
