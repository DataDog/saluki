use std::time::Duration;

use metrics::set_default_local_recorder;
use saluki_core::data_model::event::metric::Metric;
use saluki_metrics::{test::TestRecorder, MetricsBuilder};

use super::dogstatsd_client_telemetry::DogStatsDClientTelemetry;

#[test]
fn records_all_supported_client_telemetry_rates_without_client_dimensions() {
    let recorder = TestRecorder::default();
    let _recorder_guard = set_default_local_recorder(&recorder);
    let telemetry = DogStatsDClientTelemetry::new(MetricsBuilder::default());

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
        let metric = Metric::rate(metric_name, [(100, 7.0)], Duration::from_secs(10));
        telemetry.record_metric(&metric);

        assert_eq!(recorder.gauge(telemetry_name), Some(7.0), "{metric_name}");
    }
}

#[test]
fn ignores_unknown_client_telemetry_names_and_non_rate_values() {
    let recorder = TestRecorder::default();
    let _recorder_guard = set_default_local_recorder(&recorder);
    let telemetry = DogStatsDClientTelemetry::new(MetricsBuilder::default());

    telemetry.record_metric(&Metric::rate(
        "datadog.dogstatsd.client.unrecognized",
        [(100, 7.0)],
        Duration::from_secs(10),
    ));
    telemetry.record_metric(&Metric::counter("datadog.dogstatsd.client.bytes_sent", 11.0));
    telemetry.record_metric(&Metric::rate(
        "datadog.dogstatsd.client.metrics",
        [(100, 11.0)],
        Duration::from_secs(10),
    ));

    assert_eq!(recorder.gauge("dogstatsd_client_telemetry_bytes_sent"), Some(0.0));
    assert_eq!(recorder.gauge("dogstatsd_client_telemetry_metrics"), None);
}
