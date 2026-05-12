use super::RemapperRule;

const DSD_COMPONENT_TAG: &str = "component_id:dsd_in";
const LISTENER_UDP_TAG: &str = "listener_type:udp";
const LISTENER_UNIX_TAG: &str = "listener_type:unix";
const LISTENER_UNIXGRAM_TAG: &str = "listener_type:unixgram";
const ERROR_DECODE_TAG: &str = "error_type:decode";
const ERROR_ORIGIN_DETECTION_TAG: &str = "error_type:origin_detection";
const ERROR_SCOPE_PHASE_TAG: &str = "error_scope:phase";
const ERROR_SCOPE_TRANSACTION_TAG: &str = "error_scope:transaction";
const MESSAGE_EVENTS_TAG: &str = "message_type:events";
const MESSAGE_METRICS_TAG: &str = "message_type:metrics";
const MESSAGE_SERVICE_CHECKS_TAG: &str = "message_type:service_checks";
const SOURCE_TAG: &str = "source:agent-data-plane";
const STATE_ERROR_TAG: &str = "state:error";

/// Returns remapper rules for the ADP telemetry compatibility endpoint.
pub fn get_compat_remappings() -> Vec<RemapperRule> {
    vec![
        RemapperRule::by_name_and_tags(
            "adp.component_events_received_total",
            &[DSD_COMPONENT_TAG, MESSAGE_EVENTS_TAG],
            "dogstatsd_event_packets",
        ),
        RemapperRule::by_name_and_tags(
            "adp.component_errors_total",
            &[DSD_COMPONENT_TAG, ERROR_DECODE_TAG, MESSAGE_EVENTS_TAG],
            "dogstatsd_event_parse_errors",
        ),
        RemapperRule::by_name_and_tags(
            "adp.component_events_received_total",
            &[DSD_COMPONENT_TAG, MESSAGE_METRICS_TAG],
            "dogstatsd_metric_packets",
        ),
        RemapperRule::by_name_and_tags(
            "adp.component_errors_total",
            &[DSD_COMPONENT_TAG, ERROR_DECODE_TAG, MESSAGE_METRICS_TAG],
            "dogstatsd_metric_parse_errors",
        ),
        RemapperRule::by_name_and_tags(
            "adp.component_events_received_total",
            &[DSD_COMPONENT_TAG, MESSAGE_SERVICE_CHECKS_TAG],
            "dogstatsd_service_check_packets",
        ),
        RemapperRule::by_name_and_tags(
            "adp.component_errors_total",
            &[DSD_COMPONENT_TAG, ERROR_DECODE_TAG, MESSAGE_SERVICE_CHECKS_TAG],
            "dogstatsd_service_check_parse_errors",
        ),
        RemapperRule::by_name_and_tags(
            "adp.component_packets_received_total",
            &[DSD_COMPONENT_TAG, LISTENER_UDP_TAG],
            "dogstatsd_udp_packets",
        )
        .with_continued_matching(),
        RemapperRule::by_name_and_tags(
            "adp.component_bytes_received_total",
            &[DSD_COMPONENT_TAG, LISTENER_UDP_TAG],
            "dogstatsd_udp_bytes",
        ),
        RemapperRule::by_name_and_tags(
            "adp.component_packets_received_total",
            &[DSD_COMPONENT_TAG, LISTENER_UDP_TAG, STATE_ERROR_TAG],
            "dogstatsd_udp_packet_reading_errors",
        ),
        RemapperRule::by_name_and_tags(
            "adp.component_packets_received_total",
            &[DSD_COMPONENT_TAG, LISTENER_UNIX_TAG],
            "dogstatsd_uds_packets",
        )
        .with_continued_matching(),
        RemapperRule::by_name_and_tags(
            "adp.component_packets_received_total",
            &[DSD_COMPONENT_TAG, LISTENER_UNIXGRAM_TAG],
            "dogstatsd_uds_packets",
        )
        .with_continued_matching(),
        RemapperRule::by_name_and_tags(
            "adp.component_bytes_received_total",
            &[DSD_COMPONENT_TAG, LISTENER_UNIX_TAG],
            "dogstatsd_uds_bytes",
        ),
        RemapperRule::by_name_and_tags(
            "adp.component_bytes_received_total",
            &[DSD_COMPONENT_TAG, LISTENER_UNIXGRAM_TAG],
            "dogstatsd_uds_bytes",
        ),
        RemapperRule::by_name_and_tags(
            "adp.component_packets_received_total",
            &[DSD_COMPONENT_TAG, LISTENER_UNIX_TAG, STATE_ERROR_TAG],
            "dogstatsd_uds_packet_reading_errors",
        ),
        RemapperRule::by_name_and_tags(
            "adp.component_packets_received_total",
            &[DSD_COMPONENT_TAG, LISTENER_UNIXGRAM_TAG, STATE_ERROR_TAG],
            "dogstatsd_uds_packet_reading_errors",
        ),
        RemapperRule::by_name_and_tags(
            "adp.component_errors_total",
            &[DSD_COMPONENT_TAG, ERROR_ORIGIN_DETECTION_TAG],
            "dogstatsd_uds_origin_detection_errors",
        ),
        RemapperRule::by_name(
            "adp.network_http_requests_failed_total",
            "forwarder_transactions_dropped",
        )
        .with_additional_tags([SOURCE_TAG]),
        RemapperRule::by_name(
            "adp.network_http_requests_success_total",
            "forwarder_transactions_success",
        )
        .with_additional_tags([SOURCE_TAG]),
        RemapperRule::by_name_and_tags(
            "adp.network_http_requests_errors_total",
            &["error_type:client_error"],
            "forwarder_transactions_http_errors",
        )
        .with_additional_tags([SOURCE_TAG])
        .with_continued_matching(),
        RemapperRule::by_name_and_tags(
            "adp.network_http_requests_errors_total",
            &["error_type:connection_error", ERROR_SCOPE_PHASE_TAG],
            "forwarder_transactions_errors_by_type_connection_errors",
        )
        .with_additional_tags([SOURCE_TAG])
        .with_continued_matching(),
        RemapperRule::by_name_and_tags(
            "adp.network_http_requests_errors_total",
            &["error_type:dns_error", ERROR_SCOPE_PHASE_TAG],
            "forwarder_transactions_errors_by_type_dns_errors",
        )
        .with_additional_tags([SOURCE_TAG])
        .with_continued_matching(),
        RemapperRule::by_name_and_tags(
            "adp.network_http_requests_errors_total",
            &["error_type:tls_error", ERROR_SCOPE_PHASE_TAG],
            "forwarder_transactions_errors_by_type_tls_errors",
        )
        .with_additional_tags([SOURCE_TAG])
        .with_continued_matching(),
        RemapperRule::by_name_and_tags(
            "adp.network_http_requests_errors_total",
            &["error_type:wrote_request_error", ERROR_SCOPE_PHASE_TAG],
            "forwarder_transactions_errors_by_type_wrote_request_errors",
        )
        .with_additional_tags([SOURCE_TAG])
        .with_continued_matching(),
        RemapperRule::by_name_and_tags(
            "adp.network_http_requests_errors_total",
            &["error_type:sent_request_error", ERROR_SCOPE_TRANSACTION_TAG],
            "forwarder_transactions_errors_by_type_sent_request_errors",
        )
        .with_additional_tags([SOURCE_TAG])
        .with_continued_matching(),
        RemapperRule::by_name(
            "adp.network_http_requests_errors_total",
            "forwarder_transactions_errors",
        )
        .with_additional_tags([SOURCE_TAG]),
        RemapperRule::by_name(
            "adp.network_http_retry_queue_size",
            "forwarder_transactions_retry_queue_size",
        )
        .with_additional_tags([SOURCE_TAG]),
        RemapperRule::by_name(
            "adp.network_http_retry_queue_bytes_per_sec",
            "retry_queue_duration_bytes_per_sec",
        )
        .with_additional_tags([SOURCE_TAG]),
    ]
}
