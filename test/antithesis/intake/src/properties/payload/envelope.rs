//! Envelope checks

use antithesis_sdk::prelude::*;
use axum::http::HeaderMap;
use headers::{ContentType, HeaderMapExt};
use mime::Mime;
use serde_json::json;

use crate::capture::Target;
use crate::sut_config::SutConfig;

/// Pyld01 -- Content-Type in {application/x-protobuf, application/json}.
pub(crate) fn content_type(target: Target, headers: &HeaderMap) -> bool {
    let ok = headers.typed_get::<ContentType>().is_some_and(|ct| {
        matches!(
            Mime::from(ct).essence_str(),
            "application/x-protobuf" | "application/json"
        )
    });
    assert_always!(
        ok,
        "Pyld01.content_type",
        &json!({ "lane": target, "header": "Content-Type" })
    );
    ok
}

/// Whether a `Content-Encoding` holds. Absent passes. A present value must be text and name only
/// encodings the intake accepts, so a value carrying bytes no header parser can read fails rather than
/// reading as absent and passing vacuously.
fn encoding_ok(encoding: Option<&[u8]>) -> bool {
    encoding.is_none_or(|value| {
        str::from_utf8(value).is_ok_and(|value| {
            value
                .split(',')
                .all(|part| matches!(part.trim(), "deflate" | "gzip" | "zstd" | "identity"))
        })
    })
}

/// Pyld02 -- Content-Encoding absent or in {deflate, gzip, zstd, identity}. Takes the raw
/// pre-decompression bytes because `RequestDecompressionLayer` strips the header before the handler runs.
pub(crate) fn content_encoding(target: Target, encoding: Option<&[u8]>) {
    assert_always!(
        encoding_ok(encoding),
        "Pyld02.content_encoding",
        &json!({ "lane": target, "content_encoding": encoding.map(String::from_utf8_lossy) })
    );
}

/// Pyld03 -- DD-Api-Key present and non-empty.
pub(crate) fn api_key(target: Target, headers: &HeaderMap) -> bool {
    let ok = headers.get("dd-api-key").is_some_and(|v| !v.as_bytes().is_empty());
    assert_always!(
        ok,
        "Pyld03.api_key_present",
        &json!({ "lane": target, "header": "DD-Api-Key" })
    );
    ok
}

/// Pyld60 -- the payload arrived on the series API the config selected. Both targets downgrade a v3
/// timeline to v2 under zlib, which is the fault this catches rather than a carve-out it grants.
pub(crate) fn series_api_as_configured(target: Target, config: &SutConfig, observed_v3: bool) {
    let expected_v3 = config.expected_series_v3();
    assert_always!(
        observed_v3 == expected_v3,
        "Pyld60.series_api_as_configured",
        &json!({ "lane": target, "expected_v3": expected_v3, "observed_v3": observed_v3, "compressor": config.compressor() })
    );
}

#[cfg(test)]
mod tests {
    use super::encoding_ok;

    #[test]
    fn encoding_ok_fails_a_present_unreadable_value() {
        assert!(encoding_ok(None));
        assert!(encoding_ok(Some(b"zstd".as_slice())));
        assert!(encoding_ok(Some(b"gzip, identity".as_slice())));
        assert!(!encoding_ok(Some(b"br".as_slice())));
        // A header value the parser cannot read is present and wrong, not absent.
        assert!(!encoding_ok(Some(b"\xff\xfe".as_slice())));
        assert!(!encoding_ok(Some(b"".as_slice())));
    }
}
