use datadog_protos::events as proto;
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use protobuf::{rt::WireType, CodedOutputStream};
use saluki_context::tags::{SharedTagSet, Tag, TagsExt};
use saluki_core::data_model::event::eventd::EventD;

use super::{COMPRESSED_SIZE_LIMIT, EVENTS_BATCH_V1_API_PATH, UNCOMPRESSED_SIZE_LIMIT};
use crate::destinations::datadog::common::request_builder::EndpointEncoder;

const EVENTS_FIELD_NUMBER: u32 = 1;

static CONTENT_TYPE_PROTOBUF: HeaderValue = HeaderValue::from_static("application/x-protobuf");

/// An `EndpointEncoder` for sending events to Datadog.
#[derive(Debug, Default)]
pub struct EventsEndpointEncoder {
    additional_tags: SharedTagSet,
}

impl EventsEndpointEncoder {
    /// Sets the additional tags to be included with every event encoded by this encoder.
    ///
    /// These tags are added in a deduplicated fashion, the same as instrumented tags and origin tags.
    pub fn with_additional_tags(mut self, additional_tags: SharedTagSet) -> Self {
        self.additional_tags = additional_tags;
        self
    }
}

impl EndpointEncoder for EventsEndpointEncoder {
    type Input = EventD;
    type EncodeError = protobuf::Error;

    fn encoder_name() -> &'static str {
        "events"
    }

    fn compressed_size_limit(&self) -> usize {
        COMPRESSED_SIZE_LIMIT
    }

    fn uncompressed_size_limit(&self) -> usize {
        UNCOMPRESSED_SIZE_LIMIT
    }

    fn encode(&mut self, input: &Self::Input, buffer: &mut Vec<u8>) -> Result<(), Self::EncodeError> {
        encode_and_write_eventd(input, &self.additional_tags, buffer)
    }

    fn endpoint_uri(&self) -> Uri {
        PathAndQuery::from_static(EVENTS_BATCH_V1_API_PATH).into()
    }

    fn endpoint_method(&self) -> Method {
        Method::POST
    }

    fn content_type(&self) -> HeaderValue {
        CONTENT_TYPE_PROTOBUF.clone()
    }
}

fn encode_and_write_eventd(
    eventd: &EventD, additional_tags: &SharedTagSet, buf: &mut Vec<u8>,
) -> Result<(), protobuf::Error> {
    let mut output_stream = CodedOutputStream::vec(buf);

    // Write the field tag.
    output_stream.write_tag(EVENTS_FIELD_NUMBER, WireType::LengthDelimited)?;

    // Write the message.
    let encoded_eventd = encode_eventd(eventd, additional_tags);
    output_stream.write_message_no_tag(&encoded_eventd)
}

fn encode_eventd(eventd: &EventD, additional_tags: &SharedTagSet) -> proto::Event {
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

    let deduplicated_tags = get_deduplicated_tags(eventd, additional_tags);

    event.set_tags(deduplicated_tags.map(|tag| tag.as_str().into()).collect());

    event
}

fn get_deduplicated_tags<'a>(eventd: &'a EventD, additional_tags: &'a SharedTagSet) -> impl Iterator<Item = &'a Tag> {
    eventd
        .tags()
        .into_iter()
        .chain(additional_tags)
        .chain(eventd.origin_tags())
        .deduplicated()
}
