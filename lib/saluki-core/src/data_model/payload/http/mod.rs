use http::{HeaderMap, HeaderValue, Method};

/// An HTTP payload.
#[derive(Clone)]
pub struct HttpPayload {
    /// The HTTP method.
    pub method: Method,

    /// The HTTP host.
    pub host: String,

    /// The HTTP path.
    pub path: String,

    /// The HTTP headers.
    pub headers: HeaderMap<HeaderValue>,

    /// The payload body.
    pub body: Vec<u8>,
}
