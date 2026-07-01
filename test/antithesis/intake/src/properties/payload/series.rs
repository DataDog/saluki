//! `MetricSeries`-level checks

use antithesis_sdk::prelude::*;
use datadog_protos::metrics::metric_payload::MetricSeries;
use datadog_protos::metrics::MetricType;
use serde_json::json;

use super::constants::{MAX_POINTS_PER_PAYLOAD, MAX_TAG_LENGTH_BYTES, MAX_TAG_SET_SIZE_BYTES};
use crate::capture::Target;

/// `MaxTags(orgID)` default (Pyld13).
const MAX_TAGS_PER_SERIES: usize = 100;

/// Metric name byte-length cap (Pyld10).
const MAX_METRIC_NAME_BYTES: usize = 350;

/// Origin product ordinal upper bound (Pyld16).
const ORIGIN_PRODUCT_MAX: u32 = 45;

/// Origin category ordinal upper bound (Pyld16).
const ORIGIN_CATEGORY_MAX: u32 = 87;

/// Origin category reserved ordinals (Pyld16).
const ORIGIN_CATEGORY_RESERVED: [u32; 2] = [1, 13];

/// Origin service ordinal upper bound (Pyld16).
const ORIGIN_SERVICE_MAX: u32 = 519;

/// Origin service reserved ordinals (Pyld16).
const ORIGIN_SERVICE_RESERVED: [u32; 8] = [8, 31, 32, 33, 46, 88, 123, 159];

/// Tag prefixes the Agent reserves for resource promotion.
const RESERVED_TAG_PREFIXES: [&str; 2] = ["device:", "dd.internal.resource:"];

/// Pyld09 -- metric name non-empty.
pub(crate) fn metric_non_empty(target: Target, ms: &MetricSeries) {
    let ok = !ms.metric().is_empty();
    assert_always!(
        ok,
        "Pyld09.metric_non_empty",
        &json!({ "lane": target, "metric": ms.metric() })
    );
}

/// Pyld10 -- metric name at most 350 bytes.
pub(crate) fn metric_length(target: Target, ms: &MetricSeries) {
    let over = (ms.metric().len() > MAX_METRIC_NAME_BYTES).then_some(ms.metric().len());
    assert_always!(
        over.is_none(),
        "Pyld10.metric_name_length",
        &json!({ "lane": target, "metric": ms.metric(), "observed_len": over })
    );
}

/// Pyld11 -- metric name carries at least one ASCII alphabetic byte.
pub(crate) fn metric_alphabetic(target: Target, ms: &MetricSeries) {
    let no_alpha = !ms.metric().bytes().any(|b| b.is_ascii_alphabetic());
    assert_always!(
        !no_alpha,
        "Pyld11.metric_name_alphabetic",
        &json!({ "lane": target, "metric": ms.metric() })
    );
}

/// Pyld12 -- metric type in {COUNT, RATE, GAUGE}.
pub(crate) fn type_in_domain(target: Target, ms: &MetricSeries) {
    let in_domain = matches!(
        ms.type_.enum_value(),
        Ok(MetricType::COUNT | MetricType::RATE | MetricType::GAUGE)
    );
    let out = (!in_domain).then(|| ms.type_.value());
    assert_always!(
        out.is_none(),
        "Pyld12.type_in_domain",
        &json!({ "lane": target, "metric": ms.metric(), "out_of_domain_type": out })
    );
}

/// Pyld13 -- tag count at most MaxTags(orgID).
pub(crate) fn tag_count(target: Target, ms: &MetricSeries) {
    let over = (ms.tags.len() > MAX_TAGS_PER_SERIES).then_some(ms.tags.len());
    assert_always!(
        over.is_none(),
        "Pyld13.tag_count",
        &json!({ "lane": target, "metric": ms.metric(), "max_tags": MAX_TAGS_PER_SERIES, "observed": over })
    );
}

/// Pyld14 -- no tag starts with a reserved prefix.
pub(crate) fn tag_prefix(target: Target, ms: &MetricSeries) {
    let offending = ms
        .tags
        .iter()
        .find(|tag| RESERVED_TAG_PREFIXES.iter().any(|prefix| tag.starts_with(prefix)))
        .map(String::as_str);
    assert_always!(
        offending.is_none(),
        "Pyld14.reserved_tag_prefix",
        &json!({ "lane": target, "metric": ms.metric(), "offending": offending })
    );
}

/// Pyld15 -- per-series point count within the cap.
pub(crate) fn series_point_count(target: Target, ms: &MetricSeries) {
    let over = (ms.points.len() > MAX_POINTS_PER_PAYLOAD).then_some(ms.points.len());
    assert_always!(
        over.is_none(),
        "Pyld15.series_point_count",
        &json!({ "lane": target, "metric": ms.metric(), "observed": over })
    );
}

/// A field is in domain when at or below its enum maximum and not reserved.
fn origin_field_in_domain(value: u32, max: u32, reserved: &[u32]) -> bool {
    value <= max && !reserved.contains(&value)
}

/// Pyld16 -- each populated origin field is a defined enum member.
pub(crate) fn origin(target: Target, ms: &MetricSeries) {
    let out = ms.metadata.as_ref().and_then(|m| m.origin.as_ref()).and_then(|o| {
        if !origin_field_in_domain(o.origin_product, ORIGIN_PRODUCT_MAX, &[]) {
            Some(("origin_product", o.origin_product))
        } else if !origin_field_in_domain(o.origin_category, ORIGIN_CATEGORY_MAX, &ORIGIN_CATEGORY_RESERVED) {
            Some(("origin_category", o.origin_category))
        } else if !origin_field_in_domain(o.origin_service, ORIGIN_SERVICE_MAX, &ORIGIN_SERVICE_RESERVED) {
            Some(("origin_service", o.origin_service))
        } else {
            None
        }
    });
    assert_always!(
        out.is_none(),
        "Pyld16.origin_in_domain",
        &json!({ "lane": target, "metric": ms.metric(), "out_of_domain": out })
    );
}

/// Pyld23 -- each tag at most `MAX_TAG_LENGTH_BYTES` bytes.
pub(crate) fn tag_length(target: Target, ms: &MetricSeries) {
    let over = ms
        .tags
        .iter()
        .map(String::len)
        .max()
        .filter(|&len| len > MAX_TAG_LENGTH_BYTES);
    assert_always!(
        over.is_none(),
        "Pyld23.tag_length",
        &json!({ "lane": target, "metric": ms.metric(), "max_tag_bytes": MAX_TAG_LENGTH_BYTES, "observed": over })
    );
}

/// Pyld24 -- total tag-set bytes per series at most `MAX_TAG_SET_SIZE_BYTES`.
pub(crate) fn tag_set_size(target: Target, ms: &MetricSeries) {
    let total: usize = ms.tags.iter().map(String::len).sum();
    let over = (total > MAX_TAG_SET_SIZE_BYTES).then_some(total);
    assert_always!(
        over.is_none(),
        "Pyld24.tag_set_size",
        &json!({ "lane": target, "metric": ms.metric(), "max_set_bytes": MAX_TAG_SET_SIZE_BYTES, "observed": over })
    );
}

/// Pyld25 -- a flushed COUNT or GAUGE series carries at least one point.
pub(crate) fn points_non_empty(target: Target, ms: &MetricSeries) {
    let materialized = matches!(ms.type_.enum_value(), Ok(MetricType::COUNT | MetricType::GAUGE));
    let empty = (materialized && ms.points.is_empty()).then(|| ms.type_.value());
    assert_always!(
        empty.is_none(),
        "Pyld25.series_points_non_empty",
        &json!({ "lane": target, "metric": ms.metric(), "type": ms.type_.value() })
    );
}
