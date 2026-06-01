use http::Request;
use saluki_common::buf::FrozenChunkedBytesBuffer;
use saluki_core::data_model::event::metric::Metric;

/// Limits used when building V3 metrics payloads.
#[derive(Clone, Copy, Debug)]
pub(crate) struct V3PayloadLimits {
    pub(crate) max_compressed_size: usize,
    pub(crate) max_uncompressed_size: usize,
    max_metrics_per_payload: usize,
    pub(crate) max_points_per_payload: usize,
}

impl V3PayloadLimits {
    pub(crate) const fn new(
        max_compressed_size: usize, max_uncompressed_size: usize, max_metrics_per_payload: usize,
        max_points_per_payload: usize,
    ) -> Self {
        Self {
            max_compressed_size,
            max_uncompressed_size,
            max_metrics_per_payload,
            max_points_per_payload,
        }
    }

    pub(crate) fn request_fits(self, request: &V3EncodedRequest) -> bool {
        request.compressed_len <= self.max_compressed_size && request.uncompressed_len <= self.max_uncompressed_size
    }

    pub(crate) fn point_count_fits(self, count: usize) -> bool {
        count <= self.max_points_per_payload
    }

    pub(crate) fn should_flush_metric_count_limit(self, metrics: &[Metric]) -> bool {
        metrics.len() >= self.max_metrics_per_payload
    }
}

/// Encoded V3 request with measured payload sizes.
pub(crate) struct V3EncodedRequest {
    pub(crate) request: Request<FrozenChunkedBytesBuffer>,
    pub(crate) compressed_len: usize,
    pub(crate) uncompressed_len: usize,
}

/// V3 payload request ready to send with telemetry counts.
pub(crate) struct V3PayloadRequest {
    pub(crate) request: Request<FrozenChunkedBytesBuffer>,
    pub(crate) event_count: usize,
    pub(crate) data_point_count: usize,
}
