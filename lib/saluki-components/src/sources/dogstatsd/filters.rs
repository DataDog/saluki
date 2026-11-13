use saluki_io::deser::codec::dogstatsd::{EventPacket, MetricPacket, ServiceCheckPacket};

/// Filters payloads based on whether or not they're enabled.
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

    pub fn allow_event(&self, _event: &EventPacket) -> bool {
        self.allow_events
    }

    pub fn allow_service_check(&self, _service_check: &ServiceCheckPacket) -> bool {
        self.allow_service_checks
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::{
        event::{
            metric::MetricValues::{self, Counter, Distribution},
            service_check::CheckStatus,
        },
        tags::RawTags,
    };

    use super::*;

    fn metric_packet(values: MetricValues) -> MetricPacket<'static> {
        MetricPacket {
            metric_name: "",
            tags: RawTags::empty(),
            values,
            num_points: 1,
            timestamp: None,
            local_data: None,
            external_data: None,
            cardinality: None,
            unit: None,
        }
    }

    fn event_packet() -> EventPacket<'static> {
        EventPacket {
            title: "event".into(),
            text: "text".into(),
            timestamp: None,
            hostname: None,
            aggregation_key: None,
            priority: None,
            alert_type: None,
            source_type_name: None,
            tags: RawTags::empty(),
            local_data: None,
            external_data: None,
            cardinality: None,
        }
    }

    fn service_check_packet() -> ServiceCheckPacket<'static> {
        ServiceCheckPacket {
            name: "service check".into(),
            status: CheckStatus::Critical,
            message: "message".into(),
            tags: RawTags::empty(),
            hostname: None,
            local_data: None,
            external_data: None,
            cardinality: None,
            timestamp: None,
        }
    }

    #[test]
    fn enable_payloads_filter_allows_only_enabled_payload_types() {
        let default = EnablePayloadsFilter::default;
        // Expected verdicts for a series, a sketch, an event, and a service check.
        let cases = [
            ("default", default(), [true, true, true, true]),
            (
                "series off",
                default().with_allow_series(false),
                [false, true, true, true],
            ),
            (
                "sketches off",
                default().with_allow_sketches(false),
                [true, false, true, true],
            ),
            (
                "series and sketches off",
                default().with_allow_series(false).with_allow_sketches(false),
                [false, false, true, true],
            ),
            (
                "events off",
                default().with_allow_events(false),
                [true, true, false, true],
            ),
            (
                "service checks off",
                default().with_allow_service_checks(false),
                [true, true, true, false],
            ),
        ];

        let serie = metric_packet(Counter(1.0.into()));
        let sketch = metric_packet(Distribution(1.0.into()));
        let (event, service_check) = (event_packet(), service_check_packet());
        for (name, filter, expected) in cases {
            let actual = [
                filter.allow_metric(&serie),
                filter.allow_metric(&sketch),
                filter.allow_event(&event),
                filter.allow_service_check(&service_check),
            ];
            assert_eq!(actual, expected, "{name}");
        }
    }
}
