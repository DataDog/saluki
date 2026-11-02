//! Forwarder transaction status reported through the existing remote agent service.

use std::collections::BTreeMap;

use saluki_core::observability::metrics::{AggregatedMetricValue, AggregatedMetricsState};

use super::{StatusBuilder, StatusSectionWriter};

const HTTP_RETRY_QUEUE_SIZE: &str = "adp.network_http_retry_queue_size";
const HTTP_REQUESTS_REQUEUED: &str = "adp.network_http_requests_requeued_total";
const HTTP_REQUESTS_RETRIED: &str = "adp.network_http_requests_retries_total";
const HTTP_REQUESTS_SUCCESS: &str = "adp.network_http_requests_success_total";
const HTTP_REQUESTS_ERRORS: &str = "adp.network_http_requests_errors_total";
// The HTTP client used for API key validation tags all of its telemetry with this. The Core Agent does not count
// validation requests as forwarder transactions, so neither do we.
const VALIDATION_PURPOSE: &str = "purpose:api_key_validation";
const ERROR_TYPE_CLIENT: &str = "client_error";
const ERROR_TYPE_CANT_SEND: &str = "cant_send";
const ERROR_TYPE_GT_400: &str = "gt_400";
const ERROR_TYPE_SENT_REQUEST: &str = "sent_request_error";

// ADP error types and the legacy `ErrorsByType` buckets the Core Agent maps them to. `cant_send` is deliberately not
// listed: like the Core Agent, it counts toward the `Errors` total only.
const LEGACY_ERROR_TYPES: [(&str, &str); 5] = [
    ("dns_error", "DNSErrors"),
    ("tls_error", "TLSErrors"),
    ("connection_error", "ConnectionErrors"),
    ("wrote_request_error", "WroteRequestErrors"),
    (ERROR_TYPE_SENT_REQUEST, "SentRequestErrors"),
];

/// Adds `value` to the entry for `label`, only allocating the key the first time a label is seen.
fn add_to_breakdown(breakdown: &mut BTreeMap<String, f64>, label: &str, value: f64) {
    match breakdown.get_mut(label) {
        Some(total) => *total += value,
        None => {
            breakdown.insert(label.to_string(), value);
        }
    }
}

fn set_breakdown<K: std::fmt::Display>(section: &mut StatusSectionWriter<'_>, name: &str, values: BTreeMap<K, f64>) {
    for (label, count) in values {
        // Like the Core Agent, only list labels that have occurred. ADP registers some counters at zero on startup.
        if count == 0.0 {
            continue;
        }
        section.set_field(format!("{name}: {label}"), count.to_string());
    }
}

pub(super) fn write_forwarder_section(builder: &mut StatusBuilder, metrics: &AggregatedMetricsState) {
    // Mirror the transaction fields shown by the Core Agent's Forwarder status, using the same selection and meaning as
    // the Core Agent's RAR-to-expvar mappings. Like the Core Agent, breakdowns list only the labels that have occurred.
    let mut retry_queue_size = 0.0;
    let mut requeued = 0.0;
    let mut retried = 0.0;
    let mut success = 0.0;
    let mut errors = 0.0;
    let mut http_errors = 0.0;
    let mut success_by_endpoint = BTreeMap::new();
    let mut errors_by_type = BTreeMap::new();
    let mut http_errors_by_code = BTreeMap::new();

    metrics.visit_metrics(|context, value| match value {
        AggregatedMetricValue::Gauge(value)
            if context.name() == HTTP_RETRY_QUEUE_SIZE && context.tags().get_single_tag("domain").is_none() =>
        {
            // Each forwarder emits one untagged total alongside per-domain gauges. Sum only the totals to avoid
            // counting each queued transaction twice.
            retry_queue_size += value;
        }
        AggregatedMetricValue::Counter(value) if context.name() == HTTP_REQUESTS_REQUEUED => requeued += value,
        AggregatedMetricValue::Counter(value) if context.name() == HTTP_REQUESTS_RETRIED => retried += value,
        AggregatedMetricValue::Counter(value)
            if context.name() == HTTP_REQUESTS_SUCCESS && !context.tags().has_tag(VALIDATION_PURPOSE) =>
        {
            success += value;
            if let Some(endpoint) = context.tags().get_single_tag("endpoint").and_then(|tag| tag.value()) {
                add_to_breakdown(&mut success_by_endpoint, endpoint, *value);
            }
        }
        AggregatedMetricValue::Counter(value)
            if context.name() == HTTP_REQUESTS_ERRORS && !context.tags().has_tag(VALIDATION_PURPOSE) =>
        {
            let Some(error_type) = context.tags().get_single_tag("error_type").and_then(|tag| tag.value()) else {
                return;
            };

            // Lifecycle failures are also represented by a transaction-scoped error for the same request. Match the
            // Core Agent by counting only transaction-scoped types in the total.
            if matches!(
                error_type,
                ERROR_TYPE_CANT_SEND | ERROR_TYPE_GT_400 | ERROR_TYPE_SENT_REQUEST
            ) {
                errors += value;
            }
            if error_type == ERROR_TYPE_CLIENT {
                http_errors += value;
                if let Some(code) = context.tags().get_single_tag("code").and_then(|tag| tag.value()) {
                    add_to_breakdown(&mut http_errors_by_code, code, *value);
                }
            }
            if let Some((_, legacy_name)) = LEGACY_ERROR_TYPES.iter().find(|(adp_name, _)| *adp_name == error_type) {
                *errors_by_type.entry(*legacy_name).or_insert(0.0) += value;
            }
        }
        _ => {}
    });

    let mut section = builder.named_section("Forwarder");
    section
        .set_field("Requeued", requeued.to_string())
        .set_field("Retried", retried.to_string())
        .set_field("Retry Queue Size", retry_queue_size.to_string())
        .set_field("Success", success.to_string())
        .set_field("Errors", errors.to_string())
        .set_field("HTTP Errors", http_errors.to_string());

    set_breakdown(&mut section, "Success by Endpoint", success_by_endpoint);
    set_breakdown(&mut section, "Errors by Type", errors_by_type);
    set_breakdown(&mut section, "HTTP Errors by Code", http_errors_by_code);
}

#[cfg(test)]
mod tests {
    use saluki_core::{
        data_model::event::{
            metric::{context::Context, Metric},
            Event,
        },
        observability::metrics::{AggregatedMetricsProcessor, MetricsSnapshot, Processor as _},
    };

    use super::*;

    fn forwarder_status_fields(metrics: Vec<Event>) -> Vec<(String, String)> {
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
        write_forwarder_section(&mut builder, &state);

        let response = builder.into_response();
        assert!(
            response
                .main_section
                .expect("main section is always present")
                .fields
                .is_empty(),
            "named status sections must not write into the main section"
        );

        let mut fields = response.named_sections["Forwarder"]
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

    fn gauge(name: &'static str, tags: &'static [&'static str], value: f64) -> Event {
        Event::Metric(Metric::gauge(Context::from_static_parts(name, tags), value))
    }

    #[test]
    fn forwarder_section_matches_core_status_fields_and_semantics() {
        const SERIES: &[&str] = &[
            "component_id:dd_out",
            "domain:https://api.datadoghq.com",
            "endpoint:series_v2",
        ];
        const SKETCHES: &[&str] = &[
            "component_id:mrf_dd_out",
            "domain:https://api.datadoghq.eu",
            "endpoint:sketches_v2",
        ];
        let fields = forwarder_status_fields(vec![
            // Success sums across components, domains and protocol versions, and breaks down by endpoint.
            counter(
                HTTP_REQUESTS_SUCCESS,
                &[
                    "component_id:dd_out",
                    "domain:https://api.datadoghq.com",
                    "endpoint:series_v2",
                    "proto_version:HTTP/2.0",
                ],
                10.0,
            ),
            counter(
                HTTP_REQUESTS_SUCCESS,
                &[
                    "component_id:mrf_dd_out",
                    "domain:https://api.datadoghq.eu",
                    "endpoint:series_v2",
                    "proto_version:HTTP/1.1",
                ],
                5.0,
            ),
            counter(
                HTTP_REQUESTS_SUCCESS,
                &[
                    "component_id:dd_out",
                    "domain:https://api.datadoghq.com",
                    "endpoint:sketches_v2",
                    "proto_version:HTTP/2.0",
                ],
                7.0,
            ),
            // API key validation is excluded everywhere, as in the Core Agent.
            counter(
                HTTP_REQUESTS_SUCCESS,
                &["component_id:dd_out", "endpoint:/api/v1/validate", VALIDATION_PURPOSE],
                100.0,
            ),
            counter(HTTP_REQUESTS_REQUEUED, SERIES, 2.0),
            counter(HTTP_REQUESTS_REQUEUED, SKETCHES, 3.0),
            counter(HTTP_REQUESTS_RETRIED, SERIES, 4.0),
            counter(HTTP_REQUESTS_RETRIED, SKETCHES, 1.0),
            // `Dropped` is intentionally not reported: ADP's counter omits retry-queue evictions.
            counter("adp.network_http_requests_failed_total", SERIES, 1000.0),
            counter(HTTP_REQUESTS_ERRORS, &["error_type:cant_send"], 2.0),
            counter(HTTP_REQUESTS_ERRORS, &["error_type:gt_400"], 3.0),
            counter(HTTP_REQUESTS_ERRORS, &["error_type:sent_request_error"], 4.0),
            counter(HTTP_REQUESTS_ERRORS, &["error_type:dns_error"], 5.0),
            // Counters registered at zero on startup must not produce breakdown entries.
            counter(HTTP_REQUESTS_ERRORS, &["error_type:tls_error"], 0.0),
            counter(
                HTTP_REQUESTS_ERRORS,
                &["error_type:client_error", "code:404", "endpoint:series_v2"],
                0.0,
            ),
            counter(
                HTTP_REQUESTS_SUCCESS,
                &["component_id:dd_out", "endpoint:logs_v2", "proto_version:HTTP/2.0"],
                0.0,
            ),
            counter(HTTP_REQUESTS_ERRORS, &["error_type:connection_error"], 7.0),
            counter(HTTP_REQUESTS_ERRORS, &["error_type:wrote_request_error"], 8.0),
            counter(
                HTTP_REQUESTS_ERRORS,
                &["error_type:client_error", "code:500", "endpoint:series_v2"],
                9.0,
            ),
            counter(
                HTTP_REQUESTS_ERRORS,
                &["error_type:client_error", "code:429", "endpoint:sketches_v2"],
                1.0,
            ),
            // Validation traffic emits every error type, none of which belong to the forwarder.
            counter(
                HTTP_REQUESTS_ERRORS,
                &[
                    "error_type:client_error",
                    "code:500",
                    "endpoint:/api/v1/validate",
                    VALIDATION_PURPOSE,
                ],
                100.0,
            ),
            counter(
                HTTP_REQUESTS_ERRORS,
                &["error_type:cant_send", VALIDATION_PURPOSE],
                100.0,
            ),
            counter(HTTP_REQUESTS_ERRORS, &["error_type:gt_400", VALIDATION_PURPOSE], 100.0),
            counter(
                HTTP_REQUESTS_ERRORS,
                &["error_type:dns_error", VALIDATION_PURPOSE],
                100.0,
            ),
            counter(
                HTTP_REQUESTS_ERRORS,
                &["error_type:tls_error", VALIDATION_PURPOSE],
                100.0,
            ),
            counter(
                HTTP_REQUESTS_ERRORS,
                &["error_type:connection_error", VALIDATION_PURPOSE],
                100.0,
            ),
            counter(
                HTTP_REQUESTS_ERRORS,
                &["error_type:wrote_request_error", VALIDATION_PURPOSE],
                100.0,
            ),
            // Unmapped error types must not change a status field.
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

        let expected = [
            ("Errors", "9"),
            ("Errors by Type: ConnectionErrors", "7"),
            ("Errors by Type: DNSErrors", "5"),
            ("Errors by Type: SentRequestErrors", "4"),
            ("Errors by Type: WroteRequestErrors", "8"),
            ("HTTP Errors", "10"),
            ("HTTP Errors by Code: 429", "1"),
            ("HTTP Errors by Code: 500", "9"),
            ("Requeued", "5"),
            ("Retried", "5"),
            ("Retry Queue Size", "10"),
            ("Success", "22"),
            ("Success by Endpoint: series_v2", "15"),
            ("Success by Endpoint: sketches_v2", "7"),
        ]
        .map(|(name, value)| (name.to_string(), value.to_string()));
        assert_eq!(fields, expected);
    }
}
