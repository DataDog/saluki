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

/// Pyld06 -- body at or below the uncompressed cap. The claim is unconditional: `uncompressed_len` is
/// the body byte count the handler already holds, which for an identity body is its true uncompressed
/// size, so the cap is asserted on every request regardless of whether the intake decompressed it.
pub(crate) fn uncompressed_size(target: Target, uncompressed_len: u64) -> bool {
    let ok = uncompressed_len <= PYLD06_UNCOMPRESSED_CAP_BYTES;
    assert_always!(
        ok,
        "Pyld06.uncompressed_size",
        &json!({ "lane": target, "uncompressed_bytes": uncompressed_len, "cap_bytes": PYLD06_UNCOMPRESSED_CAP_BYTES })
    );
    ok
}

/// Pyld05 -- a compressed body that overran the intake's buffering cap. The cap sits far above the
/// spec cap, so reaching here is the violation; the exact size is unknown and the detail is a floor.
pub(crate) fn compressed_size_over_cap(target: Target, cap_bytes: u64) {
    assert_always!(
        false,
        "Pyld05.compressed_size",
        &json!({ "lane": target, "compressed_bytes_at_least": cap_bytes, "cap_bytes": PYLD05_COMPRESSED_CAP_BYTES })
    );
}

/// Pyld06 -- a decompressed body that overran the intake's buffering cap. As with Pyld05 the cap sits
/// far above the spec cap, so the detail is a floor rather than the true size.
pub(crate) fn uncompressed_size_over_cap(target: Target, cap_bytes: u64) {
    assert_always!(
        false,
        "Pyld06.uncompressed_size",
        &json!({ "lane": target, "uncompressed_bytes_at_least": cap_bytes, "cap_bytes": PYLD06_UNCOMPRESSED_CAP_BYTES })
    );
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

#[cfg(test)]
mod tests {
    use super::{uncompressed_size, PYLD06_UNCOMPRESSED_CAP_BYTES};
    use crate::capture::Target;

    // Pyld06 is an unconditional claim: an over-cap body fails whether or not the intake
    // decompressed it. The identity-encoded body carries its true uncompressed length too.
    #[test]
    fn pyld06_rejects_oversized_body_regardless_of_encoding() {
        assert!(!uncompressed_size(Target::Agent, PYLD06_UNCOMPRESSED_CAP_BYTES + 1));
        assert!(uncompressed_size(Target::Agent, PYLD06_UNCOMPRESSED_CAP_BYTES));
    }
}
