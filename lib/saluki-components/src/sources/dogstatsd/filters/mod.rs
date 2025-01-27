use saluki_io::deser::codec::dogstatsd::MetricPacket;

/// A filter for determining whether a metric packet should be materialized into an `Event`.
pub trait Filter {
    fn allow_metric<'a>(&self, metric: &MetricPacket<'a>) -> bool;
}

/// Filter metrics based on the the config values `enable_payloads_series` and `enable_payloads_sketches`
pub struct EnablePayloadsFilter {
    allow_series: bool,
    allow_sketches: bool,
}

impl EnablePayloadsFilter {
    pub fn new(allow_series: bool, allow_sketches: bool) -> EnablePayloadsFilter {
        EnablePayloadsFilter {
            allow_series,
            allow_sketches,
        }
    }
}

impl Filter for EnablePayloadsFilter {
    fn allow_metric<'a>(&self, metric: &MetricPacket<'a>) -> bool {
        if !self.allow_series && metric.values.is_serie() {
            return false;
        }

        if !self.allow_sketches && metric.values.is_sketch() {
            return false;
        }

        true
    }
}
