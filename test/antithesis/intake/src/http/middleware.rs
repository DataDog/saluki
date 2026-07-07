//! HTTP middleware used by Datadog-compatible routes.
//!
//! The `/api/v2/series` route stacks measurement middleware ahead of the
//! decompression layer so Pyld05 (compressed size), Pyld06 (uncompressed size),
//! and Pyld22 (content-length) can read both the on-the-wire and decompressed
//! body lengths, recorded as request extensions before `RequestDecompressionLayer`
//! consumes the encoding headers.

use axum::{
    body::Body,
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use headers::{ContentEncoding, ContentLength, HeaderMapExt};

use super::MAX_COMPRESSED_BODY_BYTES;

/// Wire measurements recorded before decompression, attached as a request extension for Pyld05/Pyld06/Pyld22.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Measurements {
    /// Compressed, on-the-wire body length, read before decompression.
    pub(crate) compressed_len: u64,
    /// Whether the request entered the decompression path.
    pub(crate) decompression_applied: bool,
    /// The declared `Content-Length`, or `None` when the header was absent.
    pub(crate) declared_content_length: Option<u64>,
}

/// Buffer the body and record compressed size, encoding, and content-length before decompression.
pub(super) async fn measure_compressed_size(req: Request, next: Next) -> Response {
    let (parts, body) = req.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_COMPRESSED_BODY_BYTES).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let len = bytes.len() as u64;
    let applied = parts
        .headers
        .typed_get::<ContentEncoding>()
        .is_some_and(|enc| enc.contains("deflate") || enc.contains("gzip") || enc.contains("zstd"));
    let declared = parts.headers.typed_get::<ContentLength>().map(|cl| cl.0);
    let mut req = Request::from_parts(parts, Body::from(bytes));
    req.extensions_mut().insert(Measurements {
        compressed_len: len,
        decompression_applied: applied,
        declared_content_length: declared,
    });
    next.run(req).await
}
