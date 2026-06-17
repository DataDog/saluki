use async_trait::async_trait;
use datadog_protos::events as proto;
use facet::Facet;
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use protobuf::{rt::WireType, CodedOutputStream};
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::iter::ReusableDeduplicator;
use saluki_config_tools::GenericConfiguration;
use saluki_context::tags::Tag;
use saluki_core::{
    components::{encoders::*, ComponentContext},
    data_model::{
        event::{eventd::EventD, Event, EventType},
        payload::{HttpPayload, Payload, PayloadMetadata, PayloadType},
    },
    observability::ComponentMetricsExt as _,
    topology::PayloadsDispatcher,
};
use saluki_error::{ErrorContext as _, GenericError};
use saluki_io::compression::CompressionScheme;
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use tracing::{debug, error, warn};

use crate::common::datadog::{
    clamp_payload_limits,
    io::RB_BUFFER_CHUNK_SIZE,
    request_builder::{EndpointEncoder, RequestBuilder},
    telemetry::ComponentTelemetry,
    DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT, DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT,
    DEFAULT_SERIALIZER_COMPRESSED_SIZE_LIMIT, DEFAULT_SERIALIZER_UNCOMPRESSED_SIZE_LIMIT,
};

const DEFAULT_SERIALIZER_COMPRESSOR_KIND: &str = "zstd";
const MAX_EVENTS_PER_PAYLOAD: usize = 100;
const EVENTS_FIELD_NUMBER: u32 = 1;

static CONTENT_TYPE_PROTOBUF: HeaderValue = HeaderValue::from_static("application/x-protobuf");

fn default_serializer_compressor_kind() -> String {
    DEFAULT_SERIALIZER_COMPRESSOR_KIND.to_owned()
}

const fn default_zstd_compressor_level() -> i32 {
    3
}

const fn default_max_payload_size() -> usize {
    DEFAULT_SERIALIZER_COMPRESSED_SIZE_LIMIT
}

const fn default_max_uncompressed_payload_size() -> usize {
    DEFAULT_SERIALIZER_UNCOMPRESSED_SIZE_LIMIT
}

const fn default_log_payloads() -> bool {
    false
}

/// Datadog Events incremental encoder.
///
/// Generates Datadog Events payloads for the Datadog platform.
#[derive(Deserialize, Facet)]
#[cfg_attr(test, derive(Debug, PartialEq, serde::Serialize))]
pub struct DatadogEventsConfiguration {
    /// Maximum compressed size, in bytes, of an events payload.
    ///
    /// This uses the same generic event payload setting as the Datadog Agent. ADP sends events to
    /// `/api/v1/events_batch`, so the effective value is clamped to that endpoint's global intake limit of 3,200,000
    /// bytes. If set to `0`, every non-empty compressed payload exceeds the limit and is dropped during flush.
    ///
    /// Defaults to 2,621,440 bytes.
    #[serde(rename = "serializer_max_payload_size", default = "default_max_payload_size")]
    max_payload_size: usize,

    /// Maximum uncompressed size, in bytes, of an events payload.
    ///
    /// This uses the same generic event payload setting as the Datadog Agent. ADP sends events to
    /// `/api/v1/events_batch`, so the effective value is clamped to that endpoint's global intake limit of 62,914,560
    /// bytes. Values smaller than the minimum endpoint framing size prevent the request builder from starting.
    ///
    /// Defaults to 4,194,304 bytes.
    #[serde(
        rename = "serializer_max_uncompressed_payload_size",
        default = "default_max_uncompressed_payload_size"
    )]
    max_uncompressed_payload_size: usize,

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

    /// Whether to log event payload contents before encoding.
    ///
    /// This logs decoded event objects, not the encoded HTTP body.
    ///
    /// Defaults to `false`.
    #[serde(default = "default_log_payloads")]
    log_payloads: bool,
}

impl DatadogEventsConfiguration {
    /// Creates a new `DatadogEventsConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }
}

#[async_trait]
impl IncrementalEncoderBuilder for DatadogEventsConfiguration {
    type Output = DatadogEvents;

    fn input_event_type(&self) -> EventType {
        EventType::EventD
    }

    fn output_payload_type(&self) -> PayloadType {
        PayloadType::Http
    }

    async fn build(&self, context: ComponentContext) -> Result<Self::Output, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let compression_scheme = CompressionScheme::new(&self.compressor_kind, self.zstd_compressor_level);

        // Create our request builder.
        let mut request_builder =
            RequestBuilder::new(EventsEndpointEncoder::new(), compression_scheme, RB_BUFFER_CHUNK_SIZE).await?;
        let (uncompressed_limit, compressed_limit) = clamp_payload_limits(
            self.max_uncompressed_payload_size,
            self.max_payload_size,
            DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT,
            DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT,
        );
        request_builder.with_len_limits(uncompressed_limit, compressed_limit)?;
        request_builder.with_max_inputs_per_payload(MAX_EVENTS_PER_PAYLOAD);

        Ok(DatadogEvents {
            request_builder,
            telemetry,
            log_payloads: self.log_payloads,
        })
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
        builder.minimum().with_single_value::<DatadogEvents>("component struct");

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
    log_payloads: bool,
}

#[async_trait]
impl IncrementalEncoder for DatadogEvents {
    async fn process_event(&mut self, event: Event) -> Result<ProcessResult, GenericError> {
        let eventd = match event.try_into_eventd() {
            Some(eventd) => eventd,
            None => return Ok(ProcessResult::Continue),
        };

        if self.log_payloads {
            debug!(event = ?eventd, "Flushing event.");
        }

        match self.request_builder.encode(eventd).await {
            Ok(None) => Ok(ProcessResult::Continue),
            Ok(Some(eventd)) => Ok(ProcessResult::FlushRequired(Event::EventD(eventd))),
            Err(e) => {
                if e.is_recoverable() {
                    warn!(error = %e, "Failed to encode Datadog event due to recoverable error. Continuing...");

                    // TODO: Get the actual number of events dropped from the error itself.
                    self.telemetry.events_dropped_encoder().increment(1);

                    Ok(ProcessResult::Continue)
                } else {
                    Err(e).error_context("Failed to encode Datadog event due to unrecoverable error.")
                }
            }
        }
    }

    async fn flush(&mut self, dispatcher: &PayloadsDispatcher) -> Result<(), GenericError> {
        let maybe_requests = self.request_builder.flush().await;
        for maybe_request in maybe_requests {
            match maybe_request {
                Ok((events, _data_points, request)) => {
                    let payload_meta = PayloadMetadata::from_event_count(events);
                    let http_payload = HttpPayload::new(payload_meta, request);
                    let payload = Payload::Http(http_payload);

                    dispatcher.dispatch(payload).await?;
                }
                Err(e) => error!(error = %e, "Failed to build Datadog events payload. Continuing..."),
            }
        }

        Ok(())
    }
}

#[derive(Debug)]
struct EventsEndpointEncoder {
    tags_deduplicator: ReusableDeduplicator<Tag>,
}

impl EventsEndpointEncoder {
    fn new() -> Self {
        Self {
            tags_deduplicator: ReusableDeduplicator::new(),
        }
    }
}

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
        encode_and_write_eventd(input, buffer, &mut self.tags_deduplicator)
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

fn encode_and_write_eventd(
    eventd: &EventD, buf: &mut Vec<u8>, tags_deduplicator: &mut ReusableDeduplicator<Tag>,
) -> Result<(), protobuf::Error> {
    let mut output_stream = CodedOutputStream::vec(buf);

    // Write the field tag.
    output_stream.write_tag(EVENTS_FIELD_NUMBER, WireType::LengthDelimited)?;

    // Write the message.
    let encoded_eventd = encode_eventd(eventd, tags_deduplicator);
    output_stream.write_message_no_tag(&encoded_eventd)
}

fn encode_eventd(eventd: &EventD, tags_deduplicator: &mut ReusableDeduplicator<Tag>) -> proto::Event {
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

    let chained_tags = eventd.tags().into_iter().chain(eventd.origin_tags());
    let deduplicated_tags = tags_deduplicator.deduplicated(chained_tags);

    event.set_tags(deduplicated_tags.map(|tag| tag.as_str().into()).collect());

    event
}

#[cfg(test)]
mod config_smoke {
    use datadog_agent_config_testing::config_registry::structs;
    use datadog_agent_config_testing::run_config_smoke_tests;
    use serde_json::json;

    use super::DatadogEventsConfiguration;
    use crate::config::{DatadogRemapper, KEY_ALIASES};

    #[tokio::test]
    async fn smoke_test() {
        run_config_smoke_tests(
            structs::DATADOG_EVENTS_CONFIGURATION,
            &[],
            json!({}),
            |cfg| {
                cfg.as_typed::<DatadogEventsConfiguration>()
                    .expect("DatadogEventsConfiguration should deserialize")
            },
            KEY_ALIASES,
            DatadogRemapper::new,
        )
        .await
    }
}
