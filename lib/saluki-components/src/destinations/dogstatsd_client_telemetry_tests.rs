use std::time::Duration;

use metrics::{set_default_local_recorder, Key, Label};
use saluki_context::Context;
use saluki_core::data_model::event::metric::Metric;
use saluki_metrics::{test::TestRecorder, MetricsBuilder};

use super::dogstatsd_client_telemetry::{normalize_client_transport, DogStatsDClientTelemetry};

fn client_counter_key(metric_name: &'static str, client: &str, client_transport: &str) -> Key {
    Key::from_parts(
        metric_name,
        vec![
            Label::new("client", client.to_string()),
            Label::new("client_transport", client_transport.to_string()),
        ],
    )
}

#[test]
fn records_all_supported_client_telemetry_rates_with_client_dimensions() {
    let recorder = TestRecorder::default();
    let _recorder_guard = set_default_local_recorder(&recorder);
    let mut telemetry = DogStatsDClientTelemetry::new(MetricsBuilder::default());

    for (metric_name, telemetry_name) in [
        (
            "datadog.dogstatsd.client.bytes_sent",
            "dogstatsd_client_telemetry_bytes_sent",
        ),
        (
            "datadog.dogstatsd.client.bytes_dropped",
            "dogstatsd_client_telemetry_bytes_dropped",
        ),
        (
            "datadog.dogstatsd.client.bytes_dropped_queue",
            "dogstatsd_client_telemetry_bytes_dropped_queue",
        ),
        (
            "datadog.dogstatsd.client.bytes_dropped_writer",
            "dogstatsd_client_telemetry_bytes_dropped_writer",
        ),
    ] {
        let metric = Metric::rate(
            Context::from_static_parts(metric_name, &["client:go", "client_transport:uds"]),
            [(100, 7.0)],
            Duration::from_secs(10),
        );
        telemetry.record_metric(&metric);

        assert_eq!(
            recorder.counter(client_counter_key(telemetry_name, "go", "uds")),
            Some(7),
            "{metric_name}"
        );
    }
}

#[test]
fn ignores_unknown_client_telemetry_names_and_non_rate_values() {
    let recorder = TestRecorder::default();
    let _recorder_guard = set_default_local_recorder(&recorder);
    let mut telemetry = DogStatsDClientTelemetry::new(MetricsBuilder::default());

    telemetry.record_metric(&Metric::rate(
        "datadog.dogstatsd.client.unrecognized",
        [(100, 7.0)],
        Duration::from_secs(10),
    ));
    telemetry.record_metric(&Metric::counter("datadog.dogstatsd.client.bytes_sent", 11.0));
    telemetry.record_metric(&Metric::rate(
        "datadog.dogstatsd.client.bytes_sent",
        [(100, 1.5)],
        Duration::from_secs(10),
    ));
    telemetry.record_metric(&Metric::rate(
        "datadog.dogstatsd.client.metrics",
        [(100, 11.0)],
        Duration::from_secs(10),
    ));

    assert_eq!(
        recorder.counter(client_counter_key(
            "dogstatsd_client_telemetry_bytes_sent",
            "unknown",
            "unknown"
        )),
        Some(0)
    );
    assert_eq!(recorder.counter("dogstatsd_client_telemetry_bytes_sent"), None);
    assert_eq!(recorder.counter("dogstatsd_client_telemetry_metrics"), None);
}

#[test]
fn accumulates_client_telemetry_rate_buckets_as_counter_deltas() {
    let recorder = TestRecorder::default();
    let _recorder_guard = set_default_local_recorder(&recorder);
    let mut telemetry = DogStatsDClientTelemetry::new(MetricsBuilder::default());

    telemetry.record_metric(&Metric::rate(
        "datadog.dogstatsd.client.bytes_sent",
        [(100, 7.0), (110, 3.0)],
        Duration::from_secs(10),
    ));

    assert_eq!(
        recorder.counter(client_counter_key(
            "dogstatsd_client_telemetry_bytes_sent",
            "unknown",
            "unknown"
        )),
        Some(10)
    );
}

#[test]
fn keeps_client_telemetry_tag_contexts_separate() {
    let recorder = TestRecorder::default();
    let _recorder_guard = set_default_local_recorder(&recorder);
    let mut telemetry = DogStatsDClientTelemetry::new(MetricsBuilder::default());

    telemetry.record_metric(&Metric::rate(
        Context::from_static_parts(
            "datadog.dogstatsd.client.bytes_sent",
            &["client:go", "client_transport:uds"],
        ),
        [(100, 7.0)],
        Duration::from_secs(10),
    ));
    telemetry.record_metric(&Metric::rate(
        Context::from_static_parts(
            "datadog.dogstatsd.client.bytes_sent",
            &["client:java", "client_transport:udp"],
        ),
        [(100, 3.0)],
        Duration::from_secs(10),
    ));

    assert_eq!(
        recorder.counter(client_counter_key("dogstatsd_client_telemetry_bytes_sent", "go", "uds")),
        Some(7)
    );
    assert_eq!(
        recorder.counter(client_counter_key(
            "dogstatsd_client_telemetry_bytes_sent",
            "java",
            "udp"
        )),
        Some(3)
    );
}

#[test]
fn preserves_supported_named_pipe_transport_values() {
    for transport in ["pipe", "namedpipe", "named_pipe"] {
        assert_eq!(normalize_client_transport(Some(transport)), transport);
    }
}
