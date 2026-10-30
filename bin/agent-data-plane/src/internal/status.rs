//! ADP-specific sections of the status details reported to the Datadog Agent.

use std::collections::BTreeMap;

use datadog_agent_runtime::remote_agent::{StatusBuilder, StatusSectionProvider};
use saluki_core::observability::metrics::{
    get_shared_metrics_state, AggregatedMetricValue, AggregatedMetricsProcessor, AggregatedMetricsState, Reflector,
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

const HTTP_RETRY_QUEUE_SIZE: &str = "adp.network_http_retry_queue_size";
const HTTP_REQUESTS_SUCCESS: &str = "adp.network_http_requests_success_total";
const HTTP_REQUESTS_ERRORS: &str = "adp.network_http_requests_errors_total";
const VALIDATION_ENDPOINT: &str = "endpoint:/api/v1/validate";
const ERROR_TYPE_CLIENT: &str = "client_error";
const ERROR_TYPE_CANT_SEND: &str = "cant_send";
const ERROR_TYPE_GT_400: &str = "gt_400";
const ERROR_TYPE_SENT_REQUEST: &str = "sent_request_error";

// ADP error types and the legacy `ErrorsByType` buckets the Core Agent bridge maps them to. `sent_request_error` is
// kept for symmetry with that mapping even though ADP reports send failures as `cant_send`, which the bridge counts
// only in the `Errors` total; the bucket is therefore normally absent rather than zero.
const LEGACY_ERROR_TYPES: [(&str, &str); 5] = [
    ("dns_error", "DNSErrors"),
    ("tls_error", "TLSErrors"),
    ("connection_error", "ConnectionErrors"),
    ("wrote_request_error", "WroteRequestErrors"),
    (ERROR_TYPE_SENT_REQUEST, "SentRequestErrors"),
];

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

/// The `Forwarder` status section, which reports the ADP transaction telemetry selected for legacy expvar parity.
pub struct ForwarderStatusSection {
    internal_metrics: Reflector<AggregatedMetricsProcessor>,
}

impl ForwarderStatusSection {
    /// Creates a new `ForwarderStatusSection` backed by the process-wide internal metrics.
    pub fn new() -> Self {
        Self {
            internal_metrics: get_shared_metrics_state(),
        }
    }
}

impl StatusSectionProvider for ForwarderStatusSection {
    fn write_status(&self, builder: &mut StatusBuilder) {
        write_forwarder_section(builder, self.internal_metrics.state());
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

fn write_forwarder_section(builder: &mut StatusBuilder, metrics: &AggregatedMetricsState) {
    // Keep this selection in sync with the Core Agent's RAR-to-expvar mappings. Those mappings intentionally expose
    // only the legacy Forwarder values with equivalent ADP semantics; endpoint, HTTP-code, and storage details remain
    // Core Agent-only.
    let mut retry_queue_size = 0.0;
    let mut success = 0.0;
    let mut errors = 0.0;
    let mut http_errors = 0.0;
    let mut errors_by_type = BTreeMap::new();

    metrics.visit_metrics(|context, value| match value {
        AggregatedMetricValue::Gauge(value)
            if context.name() == HTTP_RETRY_QUEUE_SIZE && context.tags().get_single_tag("domain").is_none() =>
        {
            // Each forwarder emits one untagged total alongside per-domain gauges. Sum only the totals to avoid
            // counting each queued transaction twice.
            retry_queue_size += value;
        }
        AggregatedMetricValue::Counter(value)
            if context.name() == HTTP_REQUESTS_SUCCESS && !context.tags().has_tag(VALIDATION_ENDPOINT) =>
        {
            success += value;
        }
        AggregatedMetricValue::Counter(value)
            if context.name() == HTTP_REQUESTS_ERRORS && !context.tags().has_tag(VALIDATION_ENDPOINT) =>
        {
            let Some(error_type) = context.tags().get_single_tag("error_type").and_then(|tag| tag.value()) else {
                return;
            };

            // Lifecycle failures are also represented by a transaction-scoped error for the same request. Match the
            // Core Agent bridge by counting only transaction-scoped types in the total.
            if matches!(
                error_type,
                ERROR_TYPE_CANT_SEND | ERROR_TYPE_GT_400 | ERROR_TYPE_SENT_REQUEST
            ) {
                errors += value;
            }
            if error_type == ERROR_TYPE_CLIENT {
                http_errors += value;
            }
            if let Some((_, legacy_name)) = LEGACY_ERROR_TYPES.iter().find(|(adp_name, _)| *adp_name == error_type) {
                *errors_by_type.entry(*legacy_name).or_insert(0.0) += value;
            }
        }
        _ => {}
    });

    let mut section = builder.named_section("Forwarder");
    section
        .set_field("Retry Queue Size", retry_queue_size.to_string())
        .set_field("Success", success.to_string())
        .set_field("Errors", errors.to_string())
        .set_field("HTTP Errors", http_errors.to_string());

    for (error_type, count) in errors_by_type {
        section.set_field(format!("Errors by Type: {error_type}"), count.to_string());
    }
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

    fn status_fields(
        metrics: Vec<Event>, section_name: &str, write_section: fn(&mut StatusBuilder, &AggregatedMetricsState),
    ) -> Vec<(String, String)> {
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
        write_section(&mut builder, &state);

        let response = builder.into_response();
        assert!(
            response
                .main_section
                .expect("main section is always present")
                .fields
                .is_empty(),
            "named status sections must not write into the main section"
        );

        let mut fields = response.named_sections[section_name]
            .fields
            .clone()
            .into_iter()
            .collect::<Vec<_>>();
        fields.sort();
        fields
    }

    fn dogstatsd_status_fields(metrics: Vec<Event>) -> Vec<(String, String)> {
        status_fields(metrics, "DogStatsD", write_dogstatsd_section)
    }

    fn forwarder_status_fields(metrics: Vec<Event>) -> Vec<(String, String)> {
        status_fields(metrics, "Forwarder", write_forwarder_section)
    }

    fn counter(name: &'static str, tags: &'static [&'static str], value: f64) -> Event {
        Event::Metric(Metric::counter(Context::from_static_parts(name, tags), value))
    }

    fn gauge(name: &'static str, tags: &'static [&'static str], value: f64) -> Event {
        Event::Metric(Metric::gauge(Context::from_static_parts(name, tags), value))
    }

    #[test]
    fn dogstatsd_section_reports_every_core_agent_field() {
        let fields = dogstatsd_status_fields(Vec::new());

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
        let fields = dogstatsd_status_fields(vec![
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

    #[test]
    fn forwarder_section_matches_legacy_expvar_selection_and_semantics() {
        const SERIES: &[&str] = &[
            "component_id:dd_out",
            "domain:https://api.datadoghq.com",
            "endpoint:/api/v2/series",
        ];
        let fields = forwarder_status_fields(vec![
            counter(HTTP_REQUESTS_SUCCESS, SERIES, 10.0),
            counter(
                HTTP_REQUESTS_SUCCESS,
                &["component_id:dd_out", VALIDATION_ENDPOINT],
                100.0,
            ),
            // This broader ADP metric is intentionally not equivalent to legacy DroppedOnInput.
            counter("adp.network_http_requests_failed_total", SERIES, 1000.0),
            counter(HTTP_REQUESTS_ERRORS, &["error_type:cant_send"], 2.0),
            counter(HTTP_REQUESTS_ERRORS, &["error_type:gt_400"], 3.0),
            counter(HTTP_REQUESTS_ERRORS, &["error_type:sent_request_error"], 4.0),
            counter(HTTP_REQUESTS_ERRORS, &["error_type:dns_error"], 5.0),
            counter(HTTP_REQUESTS_ERRORS, &["error_type:tls_error"], 6.0),
            counter(HTTP_REQUESTS_ERRORS, &["error_type:connection_error"], 7.0),
            counter(HTTP_REQUESTS_ERRORS, &["error_type:wrote_request_error"], 8.0),
            counter(
                HTTP_REQUESTS_ERRORS,
                &["error_type:client_error", "code:500", "endpoint:/api/v2/series"],
                9.0,
            ),
            counter(
                HTTP_REQUESTS_ERRORS,
                &["error_type:client_error", "code:403", VALIDATION_ENDPOINT],
                100.0,
            ),
            counter(
                HTTP_REQUESTS_ERRORS,
                &["error_type:sent_request_error", VALIDATION_ENDPOINT],
                100.0,
            ),
            // Unmapped error types must not change a legacy status field.
            counter(HTTP_REQUESTS_ERRORS, &["error_type:other"], 1000.0),
            gauge(HTTP_RETRY_QUEUE_SIZE, &["component_id:dd_out"], 4.0),
            gauge(HTTP_RETRY_QUEUE_SIZE, &["component_id:mrf_dd_out"], 6.0),
            // Per-domain gauges describe the same queued transactions and must not be counted again.
            gauge(
                HTTP_RETRY_QUEUE_SIZE,
                &["component_id:dd_out", "domain:https://api.datadoghq.com"],
                4.0,
            ),
            gauge(
                HTTP_RETRY_QUEUE_SIZE,
                &["component_id:mrf_dd_out", "domain:https://api.datadoghq.eu"],
                6.0,
            ),
        ]);

        assert_eq!(
            fields,
            [
                ("Errors".to_string(), "9".to_string()),
                ("Errors by Type: ConnectionErrors".to_string(), "7".to_string()),
                ("Errors by Type: DNSErrors".to_string(), "5".to_string()),
                ("Errors by Type: SentRequestErrors".to_string(), "4".to_string()),
                ("Errors by Type: TLSErrors".to_string(), "6".to_string()),
                ("Errors by Type: WroteRequestErrors".to_string(), "8".to_string()),
                ("HTTP Errors".to_string(), "9".to_string()),
                ("Retry Queue Size".to_string(), "10".to_string()),
                ("Success".to_string(), "10".to_string()),
            ]
        );
    }
}
