mod rules;
pub use self::rules::{get_compat_remappings, get_datadog_agent_remappings};

#[cfg(test)]
mod tests {
    use saluki_context::Context;
    use saluki_core::data_model::event::{metric::Metric, Event};
    use saluki_core::observability::metrics::{
        AggregatedMetricsProcessor, Processor as _, RemapperRule, TelemetryProcessor,
    };

    use super::*;

    fn render_with(rules: Vec<RemapperRule>, metrics: Vec<Event>) -> String {
        let processor = AggregatedMetricsProcessor;
        let state = processor.build_initial_state();
        for metric in metrics {
            processor.process(metric, &state);
        }
        TelemetryProcessor::new().with_remapper_rules(rules).process(&state)
    }

    #[test]
    fn test_render_rar_telemetry() {
        // Simulate internal metrics that match remapper rules.
        let metrics = vec![
            Event::Metric(Metric::counter(
                Context::from_static_parts("adp.object_pool_acquired", &["pool_name:dsd_packet_bufs"]),
                42.0,
            )),
            Event::Metric(Metric::gauge(
                Context::from_static_parts("adp.object_pool_in_use", &["pool_name:dsd_packet_bufs"]),
                7.0,
            )),
            Event::Metric(Metric::gauge(
                Context::from_static_parts(
                    "adp.component_data_points_sent_total",
                    &["domain:https://api.datadoghq.com"],
                ),
                12.0,
            )),
            Event::Metric(Metric::gauge(
                Context::from_static_parts(
                    "adp.component_data_points_dropped_total",
                    &["domain:https://api.datadoghq.com"],
                ),
                3.0,
            )),
            // This metric should NOT appear in output (no matching rule).
            Event::Metric(Metric::counter(
                Context::from_static_parts("adp.some_unrelated_metric", &[]),
                100.0,
            )),
        ];

        let output = render_with(get_datadog_agent_remappings(), metrics);

        // Matched metrics should appear with remapped names.
        assert!(output.contains("dogstatsd__packet_pool_get "));
        assert!(output.contains("dogstatsd__packet_pool "));
        assert!(output.contains("point__sent{domain=\"https://api.datadoghq.com\"} 12"));
        assert!(output.contains("point__dropped{domain=\"https://api.datadoghq.com\"} 3"));

        // Unmatched metrics should NOT appear.
        assert!(!output.contains("some_unrelated_metric"));

        // Should have TYPE headers.
        assert!(output.contains("# TYPE dogstatsd__packet_pool_get counter"));
        assert!(output.contains("# TYPE dogstatsd__packet_pool gauge"));
        assert!(output.contains("# TYPE point__sent gauge"));
        assert!(output.contains("# TYPE point__dropped gauge"));
    }

    #[test]
    fn test_render_compat_telemetry() {
        let metrics = vec![
            Event::Metric(Metric::counter(
                Context::from_static_parts(
                    "adp.component_events_received_total",
                    &["component_id:dsd_in", "message_type:metrics"],
                ),
                11.0,
            )),
            Event::Metric(Metric::counter(
                Context::from_static_parts(
                    "adp.component_packets_received_total",
                    &["component_id:dsd_in", "listener_type:unix"],
                ),
                7.0,
            )),
            Event::Metric(Metric::counter(
                Context::from_static_parts(
                    "adp.component_packets_received_total",
                    &["component_id:dsd_in", "listener_type:unixgram"],
                ),
                5.0,
            )),
            Event::Metric(Metric::counter(
                Context::from_static_parts(
                    "adp.component_packets_received_total",
                    &["component_id:dsd_in", "listener_type:udp", "state:error"],
                ),
                13.0,
            )),
            Event::Metric(Metric::counter(
                Context::from_static_parts(
                    "adp.component_packets_received_total",
                    &["component_id:dsd_in", "listener_type:unix", "state:error"],
                ),
                17.0,
            )),
            Event::Metric(Metric::counter(
                Context::from_static_parts(
                    "adp.component_packets_received_total",
                    &["component_id:dsd_in", "listener_type:unixgram", "state:error"],
                ),
                19.0,
            )),
            Event::Metric(Metric::counter(
                Context::from_static_parts(
                    "adp.component_errors_total",
                    &["component_id:dsd_in", "listener_type:udp", "error_type:framing"],
                ),
                23.0,
            )),
            Event::Metric(Metric::counter(
                Context::from_static_parts(
                    "adp.component_errors_total",
                    &["component_id:dsd_in", "listener_type:unix", "error_type:framing"],
                ),
                29.0,
            )),
            Event::Metric(Metric::counter(
                Context::from_static_parts(
                    "adp.component_errors_total",
                    &["component_id:dsd_in", "listener_type:unixgram", "error_type:framing"],
                ),
                31.0,
            )),
            Event::Metric(Metric::counter(
                Context::from_static_parts(
                    "adp.network_http_requests_errors_total",
                    &["error_type:connection_error", "error_scope:phase"],
                ),
                3.0,
            )),
            Event::Metric(Metric::counter(
                Context::from_static_parts(
                    "adp.network_http_requests_errors_total",
                    &["error_type:cant_send", "error_scope:transaction"],
                ),
                1.0,
            )),
            Event::Metric(Metric::counter(
                Context::from_static_parts(
                    "adp.network_http_requests_errors_total",
                    &["error_type:sent_request_error", "error_scope:transaction"],
                ),
                2.0,
            )),
            Event::Metric(Metric::gauge("adp.network_http_retry_queue_size", 2.0)),
        ];

        let output = render_with(get_compat_remappings(), metrics);

        assert!(output.contains("dogstatsd_metric_packets 11"));
        assert!(output.contains("dogstatsd_uds_packets 48"));
        assert!(output.contains("dogstatsd_udp_packet_reading_errors 13"));
        assert!(output.contains("dogstatsd_uds_packet_reading_errors 36"));
        assert!(!output.contains("dogstatsd_udp_packet_reading_errors 23"));
        assert!(!output.contains("dogstatsd_uds_packet_reading_errors 60"));
        assert!(
            output.contains("forwarder_transactions_errors_by_type_connection_errors{source=\"agent-data-plane\"} 3")
        );
        assert!(
            output.contains("forwarder_transactions_errors_by_type_sent_request_errors{source=\"agent-data-plane\"} 2")
        );
        assert!(output.contains("forwarder_transactions_errors{source=\"agent-data-plane\"} 3"));
        assert!(output.contains("forwarder_transactions_retry_queue_size{source=\"agent-data-plane\"} 2"));
    }

    #[test]
    fn test_compat_remappings_cover_expected_names() {
        let rules = get_compat_remappings();
        let expected_names = [
            "dogstatsd_event_packets",
            "dogstatsd_event_parse_errors",
            "dogstatsd_metric_packets",
            "dogstatsd_metric_parse_errors",
            "dogstatsd_service_check_packets",
            "dogstatsd_service_check_parse_errors",
            "dogstatsd_udp_packets",
            "dogstatsd_udp_bytes",
            "dogstatsd_udp_packet_reading_errors",
            "dogstatsd_uds_packets",
            "dogstatsd_uds_bytes",
            "dogstatsd_uds_packet_reading_errors",
            "dogstatsd_uds_origin_detection_errors",
            "forwarder_transactions_dropped",
            "forwarder_transactions_success",
            "forwarder_transactions_errors",
            "forwarder_transactions_http_errors",
            "forwarder_transactions_errors_by_type_connection_errors",
            "forwarder_transactions_errors_by_type_dns_errors",
            "forwarder_transactions_errors_by_type_tls_errors",
            "forwarder_transactions_errors_by_type_wrote_request_errors",
            "forwarder_transactions_errors_by_type_sent_request_errors",
            "forwarder_transactions_retry_queue_size",
            "retry_queue_duration_bytes_per_sec",
        ];

        for expected_name in expected_names {
            assert!(
                rules.iter().any(|rule| rule.remapped_name() == expected_name),
                "missing compat rule for {expected_name}"
            );
        }
    }

    #[test]
    fn test_match_context() {
        let rules = get_datadog_agent_remappings();

        let context = Context::from_static_parts("adp.object_pool_acquired", &["pool_name:dsd_packet_bufs"]);
        let matched = rules.iter().find_map(|r| r.try_match_no_context(&context));
        let remapped = matched.expect("should have matched");
        assert_eq!(remapped.name, "dogstatsd.packet_pool_get");

        // Should not match without the required tag.
        let context = Context::from_static_parts("adp.object_pool_acquired", &["pool_name:other"]);
        let matched = rules.iter().find_map(|r| r.try_match_no_context(&context));
        assert!(matched.is_none());

        // Test tag remapping.
        let context = Context::from_static_parts(
            "adp.component_events_received_total",
            &["component_id:dsd_in", "message_type:metrics"],
        );
        let matched = rules.iter().find_map(|r| r.try_match_no_context(&context));
        let remapped = matched.expect("should have matched");
        assert_eq!(remapped.name, "dogstatsd.processed");
        assert!(remapped.tags.iter().any(|t| t.as_ref() == "message_type:metrics"));
        assert!(remapped.tags.iter().any(|t| t.as_ref() == "state:ok"));
    }

    #[test]
    fn test_rar_telemetry_deduplicates_remapped_metrics() {
        // Two source metrics with different source tags that remap to the same (name, tags) identity
        // should be deduplicated (counters summed) in the RAR output.
        let metrics = vec![
            Event::Metric(Metric::counter(
                Context::from_static_parts(
                    "adp.component_events_received_total",
                    &["component_id:dsd_in", "message_type:metrics", "listener_type:udp"],
                ),
                10.0,
            )),
            Event::Metric(Metric::counter(
                Context::from_static_parts(
                    "adp.component_events_received_total",
                    &["component_id:dsd_in", "message_type:metrics", "listener_type:unixgram"],
                ),
                25.0,
            )),
        ];

        let output = render_with(get_datadog_agent_remappings(), metrics);

        // Should only have one dogstatsd__processed series with state="ok" and message_type="metrics",
        // with the summed value of 35.
        let processed_lines: Vec<&str> = output
            .lines()
            .filter(|line| {
                line.starts_with("dogstatsd__processed{")
                    && line.contains("state=\"ok\"")
                    && line.contains("message_type=\"metrics\"")
            })
            .collect();
        assert_eq!(
            processed_lines.len(),
            1,
            "expected exactly one deduplicated series, got: {processed_lines:?}"
        );
        assert!(
            processed_lines[0].ends_with(" 35"),
            "expected summed value of 35, got: {}",
            processed_lines[0]
        );
    }

    #[test]
    fn test_render_rar_telemetry_remaps_filterlist_metrics() {
        let metrics = vec![
            Event::Metric(Metric::gauge(
                Context::from_static_parts("adp.metric_filterlist_size", &["component_id:dsd_prefix_filter"]),
                2.0,
            )),
            Event::Metric(Metric::counter(
                Context::from_static_parts(
                    "adp.metric_filterlist_updates_total",
                    &["component_id:dsd_prefix_filter"],
                ),
                3.0,
            )),
            Event::Metric(Metric::counter(
                Context::from_static_parts(
                    "adp.dogstatsd_listener_filtered_points_total",
                    &["component_id:dsd_prefix_filter"],
                ),
                5.0,
            )),
            Event::Metric(Metric::counter(
                Context::from_static_parts(
                    "adp.dogstatsd_post_aggregate_filtered_metrics_total",
                    &["component_id:dsd_post_agg_filter"],
                ),
                7.0,
            )),
            Event::Metric(Metric::gauge(
                Context::from_static_parts("adp.tag_filterlist_size", &["component_id:dsd_tag_filterlist"]),
                9.0,
            )),
        ];

        let output = render_with(get_datadog_agent_remappings(), metrics);

        assert!(output.contains("datadog__agent__filterlist__size 2"));
        assert!(output.contains("datadog__agent__filterlist__updates 3"));
        assert!(output.contains("datadog__agent__dogstatsd__listener_filtered_points 5"));
        assert!(output.contains("datadog__agent__aggregator__dogstatsd_filtered_metrics 7"));
        assert!(output.contains("datadog__agent__tag_filterlist__size 9"));
        assert!(!output.contains("component_id="));
    }

    #[test]
    fn test_rar_rules_carry_expected_help_text() {
        // Ensure that the help text for overlapped metric names matches what the Datadog Agent expects.
        // The Datadog Agent will fail to parse metrics whose `# HELP` text doesn't match its registered
        // help text, so this guards against drift.
        let rules = get_datadog_agent_remappings();
        let find = |name: &str| {
            rules
                .iter()
                .find(|r| r.remapped_name() == name)
                .and_then(|r| r.help_text())
        };

        assert_eq!(
            find("no_aggregation.flush"),
            Some("Count the number of flushes done by the no-aggregation pipeline worker")
        );
        assert_eq!(
            find("no_aggregation.processed"),
            Some("Count the number of samples processed by the no-aggregation pipeline worker")
        );
        assert_eq!(
            find("aggregator.dogstatsd_contexts_by_mtype"),
            Some("Count the number of dogstatsd contexts in the aggregator, by metric type")
        );
        assert_eq!(
            find("aggregator.flush"),
            Some("Number of metrics/service checks/events flushed")
        );
        assert_eq!(
            find("aggregator.dogstatsd_contexts_bytes_by_mtype"),
            Some("Estimated count of bytes taken by contexts in the aggregator, by metric type")
        );
        assert_eq!(
            find("aggregator.dogstatsd_contexts"),
            Some("Count the number of dogstatsd contexts in the aggregator")
        );
        assert_eq!(
            find("aggregator.processed"),
            Some("Amount of metrics/services_checks/events processed by the aggregator")
        );
        assert_eq!(find("datadog.agent.filterlist.size"), Some("Metric filter list size"));
        assert_eq!(
            find("datadog.agent.filterlist.updates"),
            Some("Incremented when a reconfiguration of the metric filterlist happened")
        );
        assert_eq!(
            find("datadog.agent.dogstatsd.listener_filtered_points"),
            Some("How many points were filtered out")
        );
        assert_eq!(
            find("datadog.agent.aggregator.dogstatsd_filtered_metrics"),
            Some("How many metrics were filtered in the time samplers")
        );
        assert_eq!(find("datadog.agent.tag_filterlist.size"), Some("Tag filter list size"));
    }
}
