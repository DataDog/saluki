//! Byte-level checks

use antithesis_sdk::prelude::*;
use serde_json::json;

use crate::capture::Target;

/// Compressed body cap in bytes.
const PYLD05_COMPRESSED_CAP_BYTES: u64 = 512_000;
/// Uncompressed body cap in bytes.
const PYLD06_UNCOMPRESSED_CAP_BYTES: u64 = 5 * 1024 * 1024;

/// Pyld05 -- compressed body strictly below the cap.
pub(crate) fn compressed_size(target: Target, compressed_len: u64) -> bool {
    let ok = compressed_len < PYLD05_COMPRESSED_CAP_BYTES;
    assert_always!(
        ok,
        "Pyld05.compressed_size",
        &json!({ "lane": target, "compressed_bytes": compressed_len, "cap_bytes": PYLD05_COMPRESSED_CAP_BYTES })
    );
    ok
}

/// Pyld06 -- decompressed body at or below the cap.
pub(crate) fn uncompressed_size(target: Target, uncompressed_len: u64, decompression_applied: bool) -> bool {
    if !decompression_applied {
        return true;
    }
    let ok = uncompressed_len <= PYLD06_UNCOMPRESSED_CAP_BYTES;
    assert_always!(
        ok,
        "Pyld06.uncompressed_size",
        &json!({ "lane": target, "uncompressed_bytes": uncompressed_len, "cap_bytes": PYLD06_UNCOMPRESSED_CAP_BYTES })
    );
    ok
}

/// Pyld22 -- Content-Length absent or equal to the wire body length.
pub(crate) fn content_length(target: Target, declared: Option<u64>, body_len: u64) {
    let ok = declared.is_none_or(|d| d == body_len);
    assert_always!(
        ok,
        "Pyld22.content_length",
        &json!({ "lane": target, "compressed_bytes": body_len, "declared_content_length": declared })
    );
}
