use saluki_core::data_model::event::{eventd::EventD, service_check::ServiceCheck};
use saluki_io::deser::codec::dogstatsd::MetricPacket;

/// Filters payloads based on whether or not they are enabled.
///
/// All payloads are allowed by default.
#[derive(Copy, Clone)]
pub struct EnablePayloadsFilter {
    allow_series: bool,
    allow_sketches: bool,
    allow_events: bool,
    allow_service_checks: bool,
}

impl Default for EnablePayloadsFilter {
    fn default() -> Self {
        EnablePayloadsFilter {
            allow_series: true,
            allow_sketches: true,
            allow_events: true,
            allow_service_checks: true,
        }
    }
}

impl EnablePayloadsFilter {
    pub fn with_allow_series(mut self, allow_series: bool) -> Self {
        self.allow_series = allow_series;
        self
    }

    pub fn with_allow_sketches(mut self, allow_sketches: bool) -> Self {
        self.allow_sketches = allow_sketches;
        self
    }

    pub fn with_allow_events(mut self, allow_events: bool) -> Self {
        self.allow_events = allow_events;
        self
    }

    pub fn with_allow_service_checks(mut self, allow_service_checks: bool) -> Self {
        self.allow_service_checks = allow_service_checks;
        self
    }

    pub fn allow_metric(&self, metric: &MetricPacket<'_>) -> bool {
        if !self.allow_series && metric.values.is_serie() {
            return false;
        }

        if !self.allow_sketches && metric.values.is_sketch() {
            return false;
        }

        true
    }

    pub fn allow_event(&self, _event: &EventD) -> bool {
        self.allow_events
    }

    pub fn allow_service_check(&self, _service_check: &ServiceCheck) -> bool {
        self.allow_service_checks
    }
}

#[cfg(test)]
mod tests {
    use saluki_context::tags::RawTags;
    use saluki_core::data_model::event::{
        metric::MetricValues::{self, Counter, Distribution},
        service_check::CheckStatus,
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
    fn test_enable_payloads_filter_metrics() {
        let serie_metric = metric_packet(Counter(1.0.into()));
        let sketch_metric = metric_packet(Distribution(1.0.into()));
        let mut filter = EnablePayloadsFilter::default();
        assert!(filter.allow_metric(&serie_metric));
        assert!(filter.allow_metric(&sketch_metric));

        filter = filter.with_allow_series(false).with_allow_sketches(false);
        assert!(!filter.allow_metric(&serie_metric));
        assert!(!filter.allow_metric(&sketch_metric));
    }

    #[test]
    fn test_enable_payloads_filter_events() {
        let event = EventD::new("event", "text");
        let mut filter = EnablePayloadsFilter::default();
        assert!(filter.allow_event(&event));

        filter = filter.with_allow_events(false);
        assert!(!filter.allow_event(&event));
    }

    #[test]
    fn test_enable_payloads_filter_service_checks() {
        let service_check = ServiceCheck::new("service check", CheckStatus::Critical);
        let mut filter = EnablePayloadsFilter::default();
        assert!(filter.allow_service_check(&service_check));

        filter = filter.with_allow_service_checks(false);
        assert!(!filter.allow_service_check(&service_check));
    }
}
