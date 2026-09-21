use std::sync::{Arc, Mutex};

use metrics::Counter;
use saluki_common::collections::FastHashMap;
use saluki_metrics::MetricsBuilder;

use super::http::RetryCause;

const NETWORK_HTTP_REQUESTS_RETRY_CAUSES_TOTAL: &str = "network_http_requests_retry_causes_total";

/// Counts retried requests by what caused the retry.
///
/// Attach this to [`StandardHttpRetryLifecycle`][super::StandardHttpRetryLifecycle] to count retries as they're decided,
/// at the point where the response or the error that triggered them is still available.
///
/// ## Metrics
///
/// - `network_http_requests_retry_causes_total`: the total number of retried requests, tagged with the `domain` given at
///   construction and with a bounded description of the failure:
///   - `cause`: one of `http_status`, `http2`, `tls`, `timeout`, `connection`, `client`, or `other`.
///   - `frame` and `initiator`: for `cause:http2` only. `frame` is the HTTP/2 frame the error came from, and
///     `initiator` is the side of the connection that sent it, where `local` means this client ended the request rather
///     than the remote endpoint.
///   - `reason`: for `cause:http2`, the HTTP/2 reason code by name; for `cause:timeout`, the stage that timed out; for
///     `cause:connection`, the kind of transport failure. Absent when the failure carries no reason we recognize.
///
/// Retries caused by response status codes aren't broken down by code here, since
/// `network_http_requests_errors_total` already counts every non-success response by status code.
#[derive(Clone)]
pub struct RetryCauseTelemetry {
    builder: MetricsBuilder,

    /// Counters by cause, registered on first use.
    ///
    /// The key is an enumerated value, so this map is bounded by the number of causes that can be expressed, and it only
    /// grows on the failure path.
    counters: Arc<Mutex<FastHashMap<RetryCause, Counter>>>,
}

impl RetryCauseTelemetry {
    /// Creates retry cause telemetry for requests sent to `domain`.
    ///
    /// The domain is used as-is for the `domain` tag, so pass the same value that the rest of a client's telemetry uses:
    /// the scheme, host, and port of the remote endpoint.
    pub fn from_builder(builder: &MetricsBuilder, domain: &str) -> Self {
        Self {
            builder: builder.clone().add_default_tag(("domain", domain.to_string())),
            counters: Arc::new(Mutex::new(FastHashMap::default())),
        }
    }

    /// Counts a single retry.
    pub(super) fn increment(&self, cause: RetryCause) {
        let mut counters = self.counters.lock().unwrap();
        counters
            .entry(cause)
            .or_insert_with(|| {
                self.builder
                    .register_counter_with_tags(NETWORK_HTTP_REQUESTS_RETRY_CAUSES_TOTAL, cause.tags())
            })
            .increment(1);
    }
}
