//! HTTP middleware used by Datadog-compatible routes.
//!
//! The `/api/v2/series` route stacks measurement middleware ahead of the
//! decompression layer so Pyld05 (compressed size), Pyld06 (uncompressed size),
//! and Pyld22 (content-length) can read both the on-the-wire and decompressed
//! body lengths, recorded as request extensions before `RequestDecompressionLayer`
//! consumes the encoding headers.

use axum::{
    body::{Body, Bytes},
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use headers::{ContentEncoding, ContentLength, HeaderMapExt};

use super::{body_over_cap, MAX_COMPRESSED_BODY_BYTES};

/// Wire measurements recorded before decompression, attached as a request extension for Pyld02/05/06/22.
#[derive(Clone, Debug)]
pub(crate) struct Measurements {
    /// Compressed, on-the-wire body length, read before decompression.
    pub(crate) compressed_len: u64,
    /// Whether the request entered the decompression path.
    pub(crate) decompression_applied: bool,
    /// The declared `Content-Length`, or `None` when the header was absent.
    pub(crate) declared_content_length: Option<u64>,
    /// The raw pre-decompression `Content-Encoding`, or `None` when absent. Kept because
    /// `RequestDecompressionLayer` strips the header before the handler runs, which would leave Pyld02
    /// reading a stripped `None` and passing vacuously.
    pub(crate) content_encoding: Option<Vec<u8>>,
    /// Whether the wire body overran [`MAX_COMPRESSED_BODY_BYTES`]. The lane is only known in the
    /// handler, so the overrun rides here instead of answering 413 from the middleware, and the
    /// handler fires Pyld05 for its own lane.
    pub(crate) over_compressed_cap: bool,
}

/// Buffer the body and record compressed size, encoding, and content-length before decompression.
pub(super) async fn measure_compressed_size(req: Request, next: Next) -> Response {
    let (parts, body) = req.into_parts();
    let (bytes, over_compressed_cap) = match axum::body::to_bytes(body, MAX_COMPRESSED_BODY_BYTES).await {
        Ok(bytes) => (bytes, false),
        Err(e) => {
            // A read failure is what an injected network fault looks like, so it answers 413 without
            // blaming a size property. An overrun is the producer's, and rides to the handler.
            if !body_over_cap(e) {
                return StatusCode::PAYLOAD_TOO_LARGE.into_response();
            }
            (Bytes::new(), true)
        }
    };
    let len = bytes.len() as u64;
    let applied = parts
        .headers
        .typed_get::<ContentEncoding>()
        .is_some_and(|enc| enc.contains("deflate") || enc.contains("gzip") || enc.contains("zstd"));
    let declared = parts.headers.typed_get::<ContentLength>().map(|cl| cl.0);
    // The raw bytes, not `to_str().ok()`: a value the parser cannot read must reach Pyld02 as present
    // and wrong rather than collapsing to the absent case, which passes.
    let content_encoding = parts
        .headers
        .get(axum::http::header::CONTENT_ENCODING)
        .map(|value| value.as_bytes().to_vec());
    let mut req = Request::from_parts(parts, Body::from(bytes));
    req.extensions_mut().insert(Measurements {
        compressed_len: len,
        decompression_applied: applied,
        declared_content_length: declared,
        content_encoding,
        over_compressed_cap,
    });
    next.run(req).await
}
