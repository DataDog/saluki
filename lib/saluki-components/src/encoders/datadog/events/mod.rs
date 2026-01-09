use async_trait::async_trait;
use datadog_protos::payload::definitions as proto;
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use protobuf::{rt::WireType, CodedOutputStream};
use saluki_common::iter::ReusableDeduplicator;
use saluki_config::GenericConfiguration;
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
use tracing::{error, warn};

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

fn default_serializer_compressor_kind() -> String {
    DEFAULT_SERIALIZER_COMPRESSOR_KIND.to_owned()
}

const fn default_zstd_compressor_level() -> i32 {
    3
}

/// Datadog Events incremental encoder.
///
/// Generates Datadog Events payloads for the Datadog platform.
#[derive(Deserialize)]
pub struct DatadogEventsConfiguration {
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
        request_builder.with_max_inputs_per_payload(MAX_EVENTS_PER_PAYLOAD);

        Ok(DatadogEvents {
            request_builder,
            telemetry,
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
}

#[async_trait]
impl IncrementalEncoder for DatadogEvents {
    async fn process_event(&mut self, event: Event) -> Result<ProcessResult, GenericError> {
        let eventd = match event.try_into_eventd() {
            Some(eventd) => eventd,
            None => return Ok(ProcessResult::Continue),
        };

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
                Ok((events, request)) => {
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

fn encode_and_write_eventd_builder(
    eventd: &EventD, tags_deduplicator: &mut ReusableDeduplicator<Tag>,
) -> std::io::Result<Vec<u8>> {
    let mut scratch_writer = piecemeal::ScratchWriter::new(Vec::new());

    let mut payload_builder = datadog_protos::payload::builder::EventsPayloadBuilder::new(&mut scratch_writer);
    payload_builder.add_events(|eb| {
        eb.title(eventd.title())?.text(eventd.text())?;

        if let Some(timestamp) = eventd.timestamp() {
            eb.ts(timestamp as i64)?;
        }

        if let Some(priority) = eventd.priority() {
            eb.priority(priority.as_str())?;
        }

        if let Some(alert_type) = eventd.alert_type() {
            eb.alert_type(alert_type.as_str())?;
        }

        if let Some(hostname) = eventd.hostname() {
            eb.host(hostname)?;
        }

        if let Some(aggregation_key) = eventd.aggregation_key() {
            eb.aggregation_key(aggregation_key)?;
        }

        if let Some(source_type_name) = eventd.source_type_name() {
            eb.source_type_name(source_type_name)?;
        }

        let chained_tags = eventd.tags().into_iter().chain(eventd.origin_tags());
        let deduplicated_tags = tags_deduplicator.deduplicated(chained_tags);

        eb.tags(|tb| tb.add_many_mapped(deduplicated_tags, |tag| tag.as_str()))?;

        Ok(())
    })?;

    let mut output_buf = Vec::new();
    scratch_writer.finish(&mut output_buf, false)?;

    Ok(output_buf)
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use saluki_context::tags::TagSet;
    use saluki_core::data_model::event::eventd::Priority;
    use stringtheory::MetaString;

    use super::*;

    #[test]
    fn test_encode_and_write_eventd_builder() {
        let eventd = EventD::new("title", "text");
        let mut tags_deduplicator = ReusableDeduplicator::new();

        let encoded = encode_and_write_eventd_builder(&eventd, &mut tags_deduplicator).unwrap();
        assert!(!encoded.is_empty());
    }

    #[test]
    fn test_encode_and_write_eventd_builder_vs_owned() {
        let eventd = EventD::new("title", "text")
            .with_hostname(MetaString::from_static("fake-host"))
            .with_tags(TagSet::from_iter(vec![Tag::from("tag1"), Tag::from("tag2")]))
            .with_priority(Priority::Normal);

        let mut tags_deduplicator = ReusableDeduplicator::new();

        let encoded_builder = encode_and_write_eventd_builder(&eventd, &mut tags_deduplicator).unwrap();
        assert!(!encoded_builder.is_empty());

        let mut encoded_owned = Vec::new();
        encode_and_write_eventd(&eventd, &mut encoded_owned, &mut tags_deduplicator).unwrap();
        assert!(!encoded_owned.is_empty());

        // Print out both variants as base64:
        println!(
            "Encoded Builder: {}",
            base64::engine::general_purpose::STANDARD.encode(&encoded_builder)
        );
        println!(
            "Encoded Owned: {}",
            base64::engine::general_purpose::STANDARD.encode(&encoded_owned)
        );

        assert_eq!(encoded_owned, encoded_builder);
    }
}
