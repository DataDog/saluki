//! Raw `/api/v2/series` observation and payload-property assertions.

use std::sync::OnceLock;

use antithesis_sdk::prelude::*;
use datadog_protos::metrics::{metric_payload::MetricSeries, MetricPayload};
use protobuf::Message;
use serde_json::json;
use tracing::error;

use crate::properties::payload::{metric_payload, point, resource, series};

/// A decoded `/api/v2/series` payload.
#[derive(Debug)]
pub(crate) struct SeriesObservation {
    payload: MetricPayload,
}

impl SeriesObservation {
    /// Decode a raw `/api/v2/series` body and fire the decode-success assertion.
    pub(crate) fn decode(body_bytes: &[u8], decompression_applied: bool) -> (Option<Self>, bool) {
        let decode_result = MetricPayload::parse_from_bytes(body_bytes);
        metric_payload::decode_success(decode_result.is_ok(), body_bytes.len(), decompression_applied);
        let decode_ok = decode_result.is_ok();
        let observation = decode_result
            .map(Self::from_payload)
            .map_err(|e| error!(error = %e, "Failed to parse /api/v2/series MetricPayload."))
            .ok();

        (observation, decode_ok)
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
    pub(crate) fn assert_payload_properties(&self, now_secs: i64, established_host: &OnceLock<String>) {
        if !self.payload.series.is_empty() {
            // Lets triage distinguish an Agent that came up and flushed from one
            // that never did.
            assert_reachable!(
                "intake.first_series_observed",
                &json!({ "series": self.payload.series.len() })
            );
        }

        metric_payload::point_count(&self.payload);
        resource::host_consistent(&self.payload, established_host);
        for ms in &self.payload.series {
            evaluate_series(ms, now_secs);
        }
    }
}

/// Fire every per-series property assertion.
fn evaluate_series(ms: &MetricSeries, now_secs: i64) {
    series::type_in_domain(ms);
    series::tag_prefix(ms);
    series::series_point_count(ms);
    series::origin(ms);
    resource::resource_count(ms);
    resource::host_name_length(ms);
    series::metric_non_empty(ms);
    series::metric_length(ms);
    series::metric_alphabetic(ms);
    series::tag_count(ms);
    point::value_not_nan(ms);
    point::future_bound(ms, now_secs);
}
