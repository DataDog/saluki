use saluki_io::deser::codec::dogstatsd::MetricPacket;

/// A filter for determining whether a metric packet should be materialized into an `Event`.
pub trait Filter {
    fn allow_metric<'a>(&self, metric: &MetricPacket<'a>) -> bool;
}

/// Filters metric payloads based on whether or not the specific metric type (series or sketch) is enabled.
pub struct EnablePayloadsFilter {
    allow_series: bool,
    allow_sketches: bool,
}

impl EnablePayloadsFilter {
    pub fn new(allow_series: bool, allow_sketches: bool) -> Self {
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

#[cfg(test)]
mod tests {
    use saluki_context::tags::RawTags;
    use saluki_event::metric::{
        MetricValues,
        MetricValues::{Counter, Distribution, Histogram},
    };

    use super::*;

    fn metric_packet(values: MetricValues) -> MetricPacket<'static> {
        MetricPacket {
            metric_name: "",
            tags: RawTags::empty(),
            values,
            num_points: 1,
            timestamp: None,
            container_id: None,
            external_data: None,
            pod_uid: None,
            cardinality: None,
            jmx_check_name: None,
        }
    }

    #[test]
    fn test_enable_payloads_filter() {
        let serie_metric = metric_packet(Counter(1.0.into()));
        let sketch_metric = metric_packet(Distribution(1.0.into()));
        let filter = EnablePayloadsFilter::new(true, true);
        assert!(filter.allow_metric(&serie_metric));
        assert!(filter.allow_metric(&sketch_metric));

        let serie_metric = metric_packet(Histogram(1.0.into()));
        let sketch_metric = metric_packet(Distribution(1.0.into()));
        let filter = EnablePayloadsFilter::new(false, false);
        assert_eq!(filter.allow_metric(&serie_metric), false);
        assert_eq!(filter.allow_metric(&sketch_metric), false);
    }
}
