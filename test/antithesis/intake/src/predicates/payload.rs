//! Envelope and byte-level payload predicates from the `invariant-jig`
//! `README.md` §Properties.Payloads.

use axum::http::HeaderMap;
use datadog_protos::metrics::MetricPayload;

/// W1 -- Envelope, Content-Type. Specification §Properties.Payloads W1:
/// `Content-Type` in `{application/x-protobuf, application/json}` after
/// `mime.ParseMediaType` normalization. We do the equivalent normalization:
/// strip parameters (everything from the first `;`), trim surrounding
/// whitespace, and fold the media type to ASCII lowercase. An absent or empty
/// header fails the check. The Agent emits `application/x-protobuf` on metric
/// submissions and `application/json` on connectivity probes.
#[must_use]
pub fn w1_content_type(h: &HeaderMap) -> bool {
    let Some(value) = h.get("content-type").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let essence = value.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    matches!(essence.as_str(), "application/x-protobuf" | "application/json")
}

/// W2 -- Envelope, Content-Encoding. Specification §Properties.Payloads W2:
/// `Content-Encoding` in `{deflate, gzip, zstd, identity}`. An absent header is
/// identity per RFC 7231, so it holds. Content-coding values are
/// case-insensitive. Intake does not reject an unknown value at the header
/// layer. It falls through to no-compression decode, so W2 is
/// observation-only in the pipeline.
#[must_use]
pub fn w2_content_encoding(h: &HeaderMap) -> bool {
    let Some(value) = h.get("content-encoding").map(axum::http::HeaderValue::as_bytes) else {
        return true;
    };
    [b"deflate".as_slice(), b"gzip", b"zstd", b"identity"]
        .iter()
        .any(|accepted| value.eq_ignore_ascii_case(accepted))
}

/// W3 -- Envelope, API Key. Specification §Properties.Payloads W3: `DD-Api-Key`
/// is present iff a non-empty key was configured.
#[must_use]
pub fn w3_api_key(h: &HeaderMap) -> bool {
    h.get("dd-api-key").is_some_and(|v| !v.as_bytes().is_empty())
}

// W4 -- Envelope, User-Agent -- is intentionally not asserted. The invariant-jig
// W4 checks a `datadog-agent/` User-Agent prefix, sourced from the Go Agent's
// default forwarder. ADP identifies itself as `agent-data-plane/x.y.z` by design
// (see `common/datadog/middleware.rs`), so the Go Agent's prefix never holds.
// Intake does not validate User-Agent, so there is no cross-cut contract to
// assert here. Run 5ef26a05...-54-9 confirmed a 100% false-positive firing.

/// W5 -- Bytes, Compressed Size. Specification §Properties.Payloads W5:
/// `body < 500 KiB compressed`. The Agent caps at `<= 512000` and intake
/// rejects at `>= 512000`, so the cross-cut bound is strict.
#[must_use]
pub fn w5_compressed_size(body_len: u64, cap: u64) -> bool {
    body_len < cap
}

/// W6 -- Bytes, Uncompressed Size. Specification §Properties.Payloads W6:
/// `body <= 5 MiB uncompressed`. The bound is inclusive. The caller passes the
/// decompressed body length. The asymmetry note records intake enforcing the
/// cap only on the decompression path.
#[must_use]
pub fn w6_uncompressed_size(uncompressed_len: u64, cap: u64) -> bool {
    uncompressed_len <= cap
}

/// W8 -- `MetricPayload`, Point Count. Specification §Properties.Payloads W8:
/// total points across every series at or below the configured
/// `serializer_max_series_points_per_payload` cap. The Agent never assembles a
/// payload past it, and intake does not re-validate, so W8 is observation-only
/// in the pipeline. Returns the observed total when it exceeds the cap, else
/// `None`.
#[must_use]
pub fn w8_point_count(payload: &MetricPayload, max_points: usize) -> Option<usize> {
    let total: usize = payload.series.iter().map(|s| s.points.len()).sum();
    (total > max_points).then_some(total)
}

/// W22 -- Bytes, Content-Length. Specification §Properties.Payloads W22:
/// `Content-Length` absent or its value equals the body byte count. Both the
/// Agent and intake leave this to HTTP framing, so W22 is observation-only in
/// the pipeline. `declared` is the `Content-Length` value read from the wire
/// before the decompression layer strips it, and `body_len` is the measured
/// wire body length, which is the byte count the header describes.
#[must_use]
pub fn w22_content_length(declared: Option<u64>, body_len: u64) -> bool {
    declared.is_none_or(|declared| declared == body_len)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn w1_absent_header_fails() {
        assert!(!w1_content_type(&HeaderMap::new()));
    }

    #[test]
    fn w1_strips_parameters_and_folds_case() {
        let mut h = HeaderMap::new();
        h.insert(
            "content-type",
            HeaderValue::from_static("Application/X-Protobuf; charset=utf-8"),
        );
        assert!(w1_content_type(&h));
    }

    #[test]
    fn w1_rejects_unknown_media_type() {
        let mut h = HeaderMap::new();
        h.insert("content-type", HeaderValue::from_static("text/plain"));
        assert!(!w1_content_type(&h));
    }

    #[test]
    fn w2_absent_header_is_identity() {
        assert!(w2_content_encoding(&HeaderMap::new()));
    }

    #[test]
    fn w2_case_insensitive_accepts() {
        let mut h = HeaderMap::new();
        h.insert("content-encoding", HeaderValue::from_static("GZIP"));
        assert!(w2_content_encoding(&h));
    }

    #[test]
    fn w2_rejects_unknown_encoding() {
        let mut h = HeaderMap::new();
        h.insert("content-encoding", HeaderValue::from_static("br"));
        assert!(!w2_content_encoding(&h));
    }

    #[test]
    fn w3_present_nonempty_holds() {
        let mut h = HeaderMap::new();
        h.insert("dd-api-key", HeaderValue::from_static("abc"));
        assert!(w3_api_key(&h));
        assert!(!w3_api_key(&HeaderMap::new()));
    }

    #[test]
    fn w5_is_strict_below_the_cap() {
        let cap = 512_000_u64;
        assert!(w5_compressed_size(511_999, cap));
        assert!(!w5_compressed_size(cap, cap));
        assert!(!w5_compressed_size(512_001, cap));
    }

    #[test]
    fn w6_is_inclusive_at_the_cap() {
        let cap = 5_242_880_u64;
        assert!(w6_uncompressed_size(5_242_879, cap));
        assert!(w6_uncompressed_size(cap, cap));
        assert!(!w6_uncompressed_size(5_242_881, cap));
    }

    #[test]
    fn w22_absent_holds_and_mismatch_fails() {
        assert!(w22_content_length(None, 42));
        assert!(w22_content_length(Some(42), 42));
        assert!(!w22_content_length(Some(43), 42));
    }
}
