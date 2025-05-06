use std::marker::PhantomData;

use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use saluki_event::{eventd::EventD, service_check::ServiceCheck};

use crate::destinations::datadog::common::request_builder::EndpointEncoder;

static CONTENT_TYPE_JSON: HeaderValue = HeaderValue::from_static("application/json");
pub const COMPRESSED_SIZE_LIMIT: usize = 3_200_000; // 3 MB
const UNCOMPRESSED_SIZE_LIMIT: usize = 62_914_560; // 60 MB

/// An `EndpointEncoder` for sending events and service checks to the Datadog platform.
#[derive(Debug)]
pub struct EventsServiceChecksEndpointEncoder<T = ()> {
    _input_type: PhantomData<T>,
}

impl EventsServiceChecksEndpointEncoder<()> {
    /// Creates a new `EventsServiceChecksEndpointEncoder` for events.
    pub const fn for_events() -> EventsServiceChecksEndpointEncoder<EventD> {
        EventsServiceChecksEndpointEncoder { _input_type: PhantomData }
    }

    /// Creates a new `EventsServiceChecksEndpointEncoder` for service checks.
    pub const fn for_service_checks() -> EventsServiceChecksEndpointEncoder<ServiceCheck> {
        EventsServiceChecksEndpointEncoder { _input_type: PhantomData }
    }
}

macro_rules! impl_encoder {
    ($input:ty, $endpoint:literal) => {
        impl EndpointEncoder for EventsServiceChecksEndpointEncoder<$input> {
            type Input = $input;
            type EncodeError = serde_json::Error;

            fn encoder_name() -> &'static str {
                stringify!($input)
            }

            fn compressed_size_limit(&self) -> usize {
                COMPRESSED_SIZE_LIMIT
            }

            fn uncompressed_size_limit(&self) -> usize {
                UNCOMPRESSED_SIZE_LIMIT
            }

            fn encode(&self, input: &Self::Input, buffer: &mut Vec<u8>) -> Result<(), Self::EncodeError> {
                serde_json::to_writer(buffer, input)
            }

            fn endpoint_uri(&self) -> Uri {
                PathAndQuery::from_static($endpoint).into()
            }

            fn endpoint_method(&self) -> Method {
                Method::POST
            }

            fn content_type(&self) -> HeaderValue {
                CONTENT_TYPE_JSON.clone()
            }
        }
    };
}

impl_encoder!(EventD, "/api/v1/events");
impl_encoder!(ServiceCheck, "/api/v1/service_checks");
