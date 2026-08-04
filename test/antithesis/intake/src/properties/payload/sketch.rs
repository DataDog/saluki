//! Sketch (`/api/beta/sketches`) property checks.

use antithesis_sdk::prelude::*;
use datadog_protos::metrics::sketch_payload::Sketch;
use serde_json::json;

use super::constants::{MAX_TAG_LENGTH_BYTES, MAX_TAG_SET_SIZE_BYTES};
use crate::capture::{Target, MAX_HOST_NAME_LEN, MAX_METRIC_NAME_LEN, MAX_TAG_COUNT};

/// Pyld07-sketch -- the sketch body cleanly decodes. A correct Agent produces a payload the intake
/// accepts, so malformed wire and a non-UTF-8 non-tag field fail. A feral tag is venerable Agent
/// behavior, so it is a faithful reject.
pub(crate) fn decode_faithful(target: Target, cleanly_decoded: bool, outcome: &str, body_len: usize) {
    assert_always!(
        cleanly_decoded,
        "Pyld07-sketch.decode_success",
        &json!({ "lane": target, "outcome": outcome, "body_len": body_len })
    );
}

/// Fire the per-sketch shape assertions on a decoded `Sketch`. Same doctrine as the series shape checks:
/// a correct Agent stays behind every intake drop or cap threshold. Pyld20/21 do not apply, since the
/// intake does not NaN-filter sketch summaries and the summary carries no scalar point value.
pub(crate) fn shape(target: Target, sketch: &Sketch) {
    let name = sketch.metric();
    assert_always!(
        !name.is_empty(),
        "Pyld09.metric_non_empty",
        &json!({ "lane": target, "metric": name })
    );
    assert_always!(
        name.len() <= MAX_METRIC_NAME_LEN,
        "Pyld10.metric_name_length",
        &json!({ "lane": target, "metric": name, "len": name.len() })
    );
    assert_always!(
        name.bytes().any(|b| b.is_ascii_alphabetic()),
        "Pyld11.metric_name_alphabetic",
        &json!({ "lane": target, "metric": name })
    );
    assert_always!(
        sketch.tags.len() <= MAX_TAG_COUNT,
        "Pyld13.tag_count",
        &json!({ "lane": target, "metric": name, "tags": sketch.tags.len() })
    );
    assert_always!(
        sketch.host().len() <= MAX_HOST_NAME_LEN,
        "Pyld19.host_name_length",
        &json!({ "lane": target, "metric": name, "host_len": sketch.host().len() })
    );
    let tag_over = sketch
        .tags
        .iter()
        .map(String::len)
        .max()
        .filter(|&len| len > MAX_TAG_LENGTH_BYTES);
    assert_always!(
        tag_over.is_none(),
        "Pyld23.tag_length",
        &json!({ "lane": target, "metric": name, "observed": tag_over })
    );
    let tagset_bytes: usize = sketch.tags.iter().map(String::len).sum();
    assert_always!(
        tagset_bytes <= MAX_TAG_SET_SIZE_BYTES,
        "Pyld24.tag_set_size",
        &json!({ "lane": target, "metric": name, "bytes": tagset_bytes })
    );
    let empty = sketch.dogsketches.is_empty() && sketch.distributions.is_empty();
    assert_always!(
        !empty,
        "Pyld25-sketch.points_non_empty",
        &json!({ "lane": target, "metric": name })
    );
}
