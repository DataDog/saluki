//! Raw `/api/v2/series` observation and payload-property assertions.

use std::sync::OnceLock;

use antithesis_sdk::prelude::*;
use datadog_protos::metrics::{metric_payload::MetricSeries, MetricPayload};
use serde_json::json;

use crate::capture::Target;
use crate::lenient_decode::{decode_metric_payload, Rejection, Source};
use crate::properties::payload::{metric_payload, point, resource, series};

/// A decoded `/api/v2/series` payload.
#[derive(Debug)]
pub(crate) struct SeriesObservation {
    payload: MetricPayload,
}

impl SeriesObservation {
    /// Decode a raw `/api/v2/series` body and fire the Pyld07 production-faithful decode assertion.
    ///
    /// `source` is classified from the request `User-Agent` and gates v2 tag handling exactly as the
    /// backend does: `datadog-agent` sanitizes a feral tag, every other source whole-payload-rejects it.
    ///
    /// Returns the observation when the body decoded, and whether the intake accepted it. A correct
    /// Agent produces a payload the intake accepts cleanly, so any reject fails Pyld07: malformed wire,
    /// a non-UTF-8 non-tag field, or a non-UTF-8 tag from a non-`datadog-agent` source. A feral tag
    /// from the `datadog-agent` source is coerced to `Ok` and so never reaches this as a rejection.
    pub(crate) fn decode(
        target: Target, body_bytes: &[u8], decompression_applied: bool, source: Source,
    ) -> (Option<Self>, bool) {
        let outcome = decode_metric_payload(body_bytes, source);
        let (production_faithful, label) = match &outcome {
            Ok(_) => (true, "accepted"),
            Err(Rejection::NonUtf8StrictField) => (false, "rejected_non_utf8_field"),
            Err(Rejection::NonUtf8Tag) => (false, "rejected_non_utf8_tag"),
            Err(Rejection::MalformedWire) => (false, "malformed_wire"),
        };
        metric_payload::decode_production_faithful(
            target,
            production_faithful,
            label,
            body_bytes.len(),
            decompression_applied,
        );

        let accepted = outcome.is_ok();
        (outcome.ok().map(Self::from_payload), accepted)
    }

    /// Create an observation from an already-decoded payload.
    pub(crate) fn from_payload(payload: MetricPayload) -> Self {
        Self { payload }
    }

    /// Return the number of series carried by this payload.
    pub(crate) fn series_len(&self) -> usize {
        self.payload.series.len()
    }

    /// Fire all payload properties that need the decoded protobuf
    /// shape. `established_host` carries the first-seen host so Pyld17 holds
    /// across all inbound traffic.
    pub(crate) fn assert_payload_properties(&self, target: Target, now_secs: i64, established_host: &OnceLock<String>) {
        if !self.payload.series.is_empty() {
            // Lets triage distinguish an Agent that came up and flushed from one
            // that never did.
            assert_reachable!(
                "intake.first_series_observed",
                &json!({ "lane": target, "series": self.payload.series.len() })
            );
        }

        metric_payload::point_count(target, &self.payload);
        resource::host_consistent(target, &self.payload, established_host);
        for ms in &self.payload.series {
            // Every emitted series is asserted for correct shape. The intake charter is that ADP
            // emits correctly-shaped data, so a series ADP emits gets its shape checked whether or
            // not the intake would keep it. The keep-filter (series_kept_by_intake) governs context
            // capture in `capture.rs`, not assertion scope: gating the checks on it would silently
            // absorb a malformed emission the intake happens to drop, leaving the property untested.
            evaluate_series(target, ms, now_secs);
        }
    }

    /// Return the decoded payload.
    pub(crate) fn into_payload(self) -> MetricPayload {
        self.payload
    }
}

/// Fire every per-series property assertion.
fn evaluate_series(target: Target, ms: &MetricSeries, now_secs: i64) {
    series::type_in_domain(target, ms);
    series::tag_prefix(target, ms);
    series::series_point_count(target, ms);
    series::points_non_empty(target, ms);
    series::origin(target, ms);
    resource::resource_count(target, ms);
    resource::host_name_length(target, ms);
    series::metric_non_empty(target, ms);
    series::metric_length(target, ms);
    series::metric_alphabetic(target, ms);
    series::tag_count(target, ms);
    series::tag_length(target, ms);
    series::tag_set_size(target, ms);
    point::value_not_nan(target, ms);
    point::future_bound(target, ms, now_secs);
}
