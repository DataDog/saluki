use http::Request;
use saluki_common::buf::FrozenChunkedBytesBuffer;

/// An HTTP payload.
#[derive(Clone)]
pub struct HttpPayload {
    req: Request<FrozenChunkedBytesBuffer>,
}

impl HttpPayload {
    /// Creates a new `HttpPayload` from the given request.
    pub fn new(req: Request<FrozenChunkedBytesBuffer>) -> Self {
        HttpPayload { req }
    }

    /// Consumes the `HttpPayload` and returns the underlying request.
    pub fn into_inner(self) -> Request<FrozenChunkedBytesBuffer> {
        self.req
    }
}
