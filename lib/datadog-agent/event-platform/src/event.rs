use std::{collections::BTreeMap, fmt};

use bytes::Bytes;
use prost::Message;
use saluki_common::opaque::OpaqueType;
use saluki_error::GenericError;

/// An event platform event: one payload for one track of the event platform intake.
///
/// The body goes to the intake unread. Most tracks carry JSON and some carry protobuf, so the body is bytes.
///
/// The headers are the HTTP headers that the producer of the event knows, such as `Content-Type`.
/// [`Track::headers`](crate::Track::headers) returns the headers that the Datadog Agent sets. The sender adds the
/// headers that only it knows, such as `DD-API-KEY`. Events with different headers can't share a request.
///
/// An event encodes as a protobuf message, with the track in field 1, the headers in field 2, and the body in field 3.
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct EventPlatformEvent {
    #[prost(string, tag = "1")]
    track: String,

    #[prost(btree_map = "string, string", tag = "2")]
    headers: BTreeMap<String, String>,

    #[prost(bytes = "bytes", tag = "3")]
    body: Bytes,
}

impl EventPlatformEvent {
    /// Creates a new `EventPlatformEvent` for the given track, with the given headers and body.
    pub fn new(track: impl Into<String>, headers: BTreeMap<String, String>, body: Bytes) -> Self {
        Self {
            track: track.into(),
            headers,
            body,
        }
    }

    /// Returns the track, as the event type that the producer submitted the event with, such as `dbm-samples`.
    ///
    /// [`Track::by_event_type`](crate::Track::by_event_type) returns the description of the track.
    pub fn track(&self) -> &str {
        &self.track
    }

    /// Returns the HTTP headers that the producer knows.
    pub fn headers(&self) -> &BTreeMap<String, String> {
        &self.headers
    }

    /// Returns the body.
    pub fn body(&self) -> &Bytes {
        &self.body
    }
}

impl fmt::Debug for EventPlatformEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The body can be large, so we show only its length.
        f.debug_struct("EventPlatformEvent")
            .field("track", &self.track)
            .field("headers", &self.headers)
            .field("body_len", &self.body.len())
            .finish()
    }
}

impl OpaqueType for EventPlatformEvent {
    const TYPE_NAME: &'static str = "datadog.agent.event_platform.v1.EventPlatformEvent";

    fn encode(&self) -> Bytes {
        Bytes::from(self.encode_to_vec())
    }

    fn try_decode(data: Bytes) -> Result<Self, GenericError> {
        Ok(<Self as Message>::decode(data)?)
    }
}

#[cfg(test)]
mod tests {
    use saluki_common::opaque::Opaque;

    use super::*;

    #[test]
    fn an_event_round_trips_through_an_opaque_value() {
        // A protobuf body can hold NUL bytes, and bytes that aren't UTF-8.
        let headers = BTreeMap::from([("Content-Type".to_string(), "application/x-protobuf".to_string())]);
        let event = EventPlatformEvent::new("genresources", headers, Bytes::from_static(b"\x0a\x00\xff\xfe"));

        let opaque = Opaque::encode(&event);

        assert_eq!(opaque.type_name(), EventPlatformEvent::TYPE_NAME);
        assert_eq!(
            opaque.try_decode::<EventPlatformEvent>().expect("should decode"),
            Some(event)
        );
    }

    #[test]
    fn invalid_data_fails_to_decode() {
        // A varint that never ends.
        let opaque = Opaque::new(EventPlatformEvent::TYPE_NAME, Bytes::from_static(&[0xff, 0xff, 0xff]));

        assert!(opaque.try_decode::<EventPlatformEvent>().is_err());
    }
}
