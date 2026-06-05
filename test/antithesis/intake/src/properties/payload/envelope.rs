//! Envelope checks

use antithesis_sdk::prelude::*;
use axum::http::HeaderMap;
use headers::{ContentEncoding, ContentType, HeaderMapExt};
use mime::Mime;
use serde_json::json;

/// Pyld01 -- Content-Type in {application/x-protobuf, application/json}.
pub(crate) fn content_type(headers: &HeaderMap) -> bool {
    let ok = headers.typed_get::<ContentType>().is_some_and(|ct| {
        matches!(
            Mime::from(ct).essence_str(),
            "application/x-protobuf" | "application/json"
        )
    });
    assert_always!(ok, "Pyld01.content_type", &json!({ "header": "Content-Type" }));
    ok
}

/// Pyld02 -- Content-Encoding in {deflate, gzip, zstd, identity}.
pub(crate) fn content_encoding(headers: &HeaderMap) {
    let ok = match headers.typed_get::<ContentEncoding>() {
        None => true,
        Some(enc) => ["deflate", "gzip", "zstd", "identity"].iter().any(|e| enc.contains(*e)),
    };
    assert_always!(ok, "Pyld02.content_encoding", &json!({ "header": "Content-Encoding" }));
}

/// Pyld03 -- DD-Api-Key present and non-empty.
pub(crate) fn api_key(headers: &HeaderMap) -> bool {
    let ok = headers.get("dd-api-key").is_some_and(|v| !v.as_bytes().is_empty());
    assert_always!(ok, "Pyld03.api_key_present", &json!({ "header": "DD-Api-Key" }));
    ok
}
