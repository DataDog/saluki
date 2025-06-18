use http::Request;
use saluki_common::buf::FrozenChunkedBytesBuffer;

use super::PayloadMetadata;

/// An HTTP payload.
#[derive(Clone)]
pub struct HttpPayload {
    metadata: PayloadMetadata,
    req: Request<FrozenChunkedBytesBuffer>,
}

impl HttpPayload {
    /// Creates a new `HttpPayload` from the given request.
    pub fn new(metadata: PayloadMetadata, req: Request<FrozenChunkedBytesBuffer>) -> Self {
        HttpPayload { metadata, req }
    }

    /// Consumes the HTTP payload and returns the individual parts.
    pub fn into_parts(self) -> (PayloadMetadata, Request<FrozenChunkedBytesBuffer>) {
        (self.metadata, self.req)
    }
}
