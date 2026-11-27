use saluki_io::deser::codec::dogstatsd::{EventPacket, MetricPacket, ServiceCheckPacket};

/// Controls which payload types are forwarded to the backend.
#[derive(Clone, Copy)]
pub struct EnablePayloadsConfiguration {
    /// Whether or not to enable sending series (counter/gauge/rate) payloads.
    pub series: bool,

    /// Whether or not to enable sending sketch (distribution) payloads.
    pub sketches: bool,

    /// Whether or not to enable sending event payloads.
    pub events: bool,

    /// Whether or not to enable sending service check payloads.
    pub service_checks: bool,
}

#[cfg(test)]
impl EnablePayloadsConfiguration {
    /// Creates a fixture configuration with every payload type enabled.
    pub(super) fn for_test() -> Self {
        Self {
            series: true,
            sketches: true,
            events: true,
            service_checks: true,
        }
    }
}

impl EnablePayloadsConfiguration {
    pub(super) fn allow_metric(&self, metric: &MetricPacket<'_>) -> bool {
        if !self.series && metric.values.is_serie() {
            return false;
        }

        if !self.sketches && metric.values.is_sketch() {
            return false;
        }

        true
    }

    pub(super) fn allow_event(&self, _event: &EventPacket) -> bool {
        self.events
    }

    pub(super) fn allow_service_check(&self, _service_check: &ServiceCheckPacket) -> bool {
        self.service_checks
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
    use saluki_io::deser::codec::dogstatsd::ClientOrigin;

    use super::*;

    fn metric_packet(values: MetricValues) -> MetricPacket<'static> {
        MetricPacket {
            metric_name: "",
            tags: RawTags::empty(),
            values,
            num_points: 1,
            timestamp: None,
            origin: ClientOrigin::default(),
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
            origin: ClientOrigin::default(),
        }
    }

    fn service_check_packet() -> ServiceCheckPacket<'static> {
        ServiceCheckPacket {
            name: "service check".into(),
            status: CheckStatus::Critical,
            message: "message".into(),
            tags: RawTags::empty(),
            hostname: None,
            origin: ClientOrigin::default(),
            timestamp: None,
        }
    }

    #[test]
    fn enable_payloads_allows_only_enabled_payload_types() {
        let enabled = |series, sketches, events, service_checks| EnablePayloadsConfiguration {
            series,
            sketches,
            events,
            service_checks,
        };
        // Expected verdicts for a series, a sketch, an event, and a service check.
        let cases = [
            (
                "default",
                EnablePayloadsConfiguration::for_test(),
                [true, true, true, true],
            ),
            (
                "series off",
                enabled(false, true, true, true),
                [false, true, true, true],
            ),
            (
                "sketches off",
                enabled(true, false, true, true),
                [true, false, true, true],
            ),
            (
                "series and sketches off",
                enabled(false, false, true, true),
                [false, false, true, true],
            ),
            (
                "events off",
                enabled(true, true, false, true),
                [true, true, false, true],
            ),
            (
                "service checks off",
                enabled(true, true, true, false),
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
