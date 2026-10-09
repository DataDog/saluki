//! ADP-specific sections of the status details reported to the Datadog Agent.

use datadog_agent_runtime::remote_agent::{StatusBuilder, StatusSectionProvider};
use saluki_core::observability::metrics::{
    get_shared_metrics_state, AggregatedMetricsProcessor, AggregatedMetricsState, Reflector,
};

const EVENTS_RECEIVED: &str = "adp.component_events_received_total";
const PACKETS_RECEIVED: &str = "adp.component_packets_received_total";
const BYTES_RECEIVED: &str = "adp.component_bytes_received_total";
const ERRORS: &str = "adp.component_errors_total";
const DSD_COMP_ID: &str = "component_id:dsd_in";
const ERROR_DECODE: &str = "error_type:decode";
const ERROR_FRAMING: &str = "error_type:framing";
const TYPE_EVENTS: &str = "message_type:events";
const TYPE_METRICS: &str = "message_type:metrics";
const TYPE_SERVICE_CHECKS: &str = "message_type:service_checks";
const LISTENER_UDP: &str = "listener_type:udp";
const LISTENER_UNIX: &str = "listener_type:unix";
const LISTENER_UNIXGRAM: &str = "listener_type:unixgram";

/// The `DogStatsD` status section, which mirrors the Core Agent's own DogStatsD status fields.
///
/// The fields are derived from the internal metrics of the DogStatsD source.
pub struct DogStatsDStatusSection {
    internal_metrics: Reflector<AggregatedMetricsProcessor>,
}

impl DogStatsDStatusSection {
    /// Creates a new `DogStatsDStatusSection` backed by the process-wide internal metrics.
    pub fn new() -> Self {
        Self {
            internal_metrics: get_shared_metrics_state(),
        }
    }
}

impl StatusSectionProvider for DogStatsDStatusSection {
    fn write_status(&self, builder: &mut StatusBuilder) {
        write_dogstatsd_section(builder, self.internal_metrics.state());
    }
}

fn write_dogstatsd_section(builder: &mut StatusBuilder, metrics: &AggregatedMetricsState) {
    // Grab some simple metrics from the DogStatsD source.
    let event_packets = metrics.get_aggregated_with_tags(EVENTS_RECEIVED, &[DSD_COMP_ID, TYPE_EVENTS]);
    let metric_packets = metrics.get_aggregated_with_tags(EVENTS_RECEIVED, &[DSD_COMP_ID, TYPE_METRICS]);
    let scheck_packets = metrics.get_aggregated_with_tags(EVENTS_RECEIVED, &[DSD_COMP_ID, TYPE_SERVICE_CHECKS]);

    let event_parse_errors = metrics.get_aggregated_with_tags(ERRORS, &[DSD_COMP_ID, ERROR_DECODE, TYPE_EVENTS]);
    let metric_parse_errors = metrics.get_aggregated_with_tags(ERRORS, &[DSD_COMP_ID, ERROR_DECODE, TYPE_METRICS]);
    let scheck_parse_errors =
        metrics.get_aggregated_with_tags(ERRORS, &[DSD_COMP_ID, ERROR_DECODE, TYPE_SERVICE_CHECKS]);

    let get_listener_metrics = |listener_type: &str| {
        (
            metrics.get_aggregated_with_tags(BYTES_RECEIVED, &[DSD_COMP_ID, listener_type]),
            metrics
                .find_single_with_tags(ERRORS, &[DSD_COMP_ID, listener_type, ERROR_FRAMING])
                .unwrap_or(0.0),
            metrics
                .find_single_with_tags(PACKETS_RECEIVED, &[DSD_COMP_ID, listener_type, "state:ok"])
                .unwrap_or(0.0),
        )
    };

    let (udp_bytes, udp_errors, udp_packets) = get_listener_metrics(LISTENER_UDP);
    let (unix_bytes, unix_errors, unix_packets) = get_listener_metrics(LISTENER_UNIX);
    let (unixgram_bytes, unixgram_errors, unixgram_packets) = get_listener_metrics(LISTENER_UNIXGRAM);

    let uds_bytes = unix_bytes + unixgram_bytes;
    let uds_errors = unix_errors + unixgram_errors;
    let uds_packets = unix_packets + unixgram_packets;

    builder
        .named_section("DogStatsD")
        .set_field("Event Packets", event_packets.to_string())
        .set_field("Event Parse Errors", event_parse_errors.to_string())
        .set_field("Metric Packets", metric_packets.to_string())
        .set_field("Metric Parse Errors", metric_parse_errors.to_string())
        .set_field("Service Check Packets", scheck_packets.to_string())
        .set_field("Service Check Parse Errors", scheck_parse_errors.to_string())
        .set_field("Udp Bytes", udp_bytes.to_string())
        .set_field("Udp Packet Reading Errors", udp_errors.to_string())
        .set_field("Udp Packets", udp_packets.to_string())
        .set_field("Uds Bytes", uds_bytes.to_string())
        .set_field("Uds Packet Reading Errors", uds_errors.to_string())
        .set_field("Uds Packets", uds_packets.to_string());
}

#[cfg(test)]
mod tests {
    use saluki_core::{
        data_model::event::{
            metric::{context::Context, Metric},
            Event,
        },
        observability::metrics::{MetricsSnapshot, Processor as _},
    };

    use super::*;

    fn status_fields(metrics: Vec<Event>) -> Vec<(String, String)> {
        let processor = AggregatedMetricsProcessor;
        let state = processor.build_initial_state();
        processor.process(
            MetricsSnapshot {
                upserts: metrics,
                evictions: Vec::new(),
            },
            &state,
        );

        let mut builder = StatusBuilder::new();
        write_dogstatsd_section(&mut builder, &state);

        let response = builder.into_response();
        assert!(
            response
                .main_section
                .expect("main section is always present")
                .fields
                .is_empty(),
            "the DogStatsD section must not write into the main section"
        );

        let mut fields = response.named_sections["DogStatsD"]
            .fields
            .clone()
            .into_iter()
            .collect::<Vec<_>>();
        fields.sort();
        fields
    }

    fn counter(name: &'static str, tags: &'static [&'static str], value: f64) -> Event {
        Event::Metric(Metric::counter(Context::from_static_parts(name, tags), value))
    }

    #[test]
    fn dogstatsd_section_reports_every_core_agent_field() {
        let fields = status_fields(Vec::new());

        let names = fields.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "Event Packets",
                "Event Parse Errors",
                "Metric Packets",
                "Metric Parse Errors",
                "Service Check Packets",
                "Service Check Parse Errors",
                "Udp Bytes",
                "Udp Packet Reading Errors",
                "Udp Packets",
                "Uds Bytes",
                "Uds Packet Reading Errors",
                "Uds Packets",
            ]
        );
        assert!(fields.iter().all(|(_, value)| value == "0"), "{fields:?}");
    }

    #[test]
    fn dogstatsd_section_combines_unix_listeners_into_uds() {
        let fields = status_fields(vec![
            counter(EVENTS_RECEIVED, &[DSD_COMP_ID, TYPE_METRICS, LISTENER_UDP], 5.0),
            counter(BYTES_RECEIVED, &[DSD_COMP_ID, LISTENER_UNIX], 100.0),
            counter(BYTES_RECEIVED, &[DSD_COMP_ID, LISTENER_UNIXGRAM], 20.0),
            counter(PACKETS_RECEIVED, &[DSD_COMP_ID, LISTENER_UNIX, "state:ok"], 3.0),
            counter(PACKETS_RECEIVED, &[DSD_COMP_ID, LISTENER_UNIXGRAM, "state:ok"], 4.0),
        ]);

        let field = |name: &str| {
            fields
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(field("Metric Packets"), Some("5"));
        assert_eq!(field("Uds Bytes"), Some("120"));
        assert_eq!(field("Uds Packets"), Some("7"));
        assert_eq!(field("Udp Bytes"), Some("0"));
    }
}
