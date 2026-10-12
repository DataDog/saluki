use axum::{body::Body, http::Request};
use datadog_protos::metrics::{
    metric_payload::{MetricPoint, MetricSeries, MetricType, Resource},
    sketch_payload::{sketch::Dogsketch, Sketch},
    MetricPayload, SketchPayload,
};
use tower::ServiceExt as _;

use super::{
    recorder::{family_of, FamilySummary, FamilyUnit, SeriesKind, HISTORY_SECS, MAX_API_KEYS, MAX_FAMILIES},
    *,
};
use crate::settings::DemoPorts;

fn primary() -> IntakeRecorder {
    IntakeRecorder::new(DemoPorts::default().recipients()[0])
}

fn series(name: &str, kind: MetricType, tags: &[&str], points: &[(i64, f64)]) -> MetricSeries {
    let mut series = MetricSeries::new();
    series.set_metric(name.into());
    series.set_type(kind);
    series.tags = tags.iter().map(|tag| (*tag).into()).collect();
    let mut host = Resource::new();
    host.set_type("host".into());
    host.set_name("adp-demo".into());
    series.resources.push(host);
    for (timestamp, value) in points {
        let mut point = MetricPoint::new();
        point.timestamp = *timestamp;
        point.value = *value;
        series.points.push(point);
    }
    series
}

async fn send(
    recorder: &IntakeRecorder, method: &str, path: &str, encoding: Option<&str>, api_key: Option<&str>, body: Vec<u8>,
) -> StatusCode {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(encoding) = encoding {
        request = request.header("content-encoding", encoding);
    }
    if let Some(api_key) = api_key {
        request = request.header("DD-API-KEY", api_key);
    }
    let response = router(recorder.clone())
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    response.status()
}

#[tokio::test]
async fn zstd_compressed_series_payloads_are_decoded_and_recorded() {
    let recorder = primary();
    let mut payload = MetricPayload::new();
    payload.series.push(series(
        "demo.canary",
        MetricType::COUNT,
        &[],
        &[(1_700_000_000, 20.0), (1_700_000_001, 19.0)],
    ));
    payload.series.push(series(
        "demo.platform.queue_depth",
        MetricType::GAUGE,
        &["team:platform"],
        &[(1_700_000_001, 57.0)],
    ));
    payload
        .series
        .push(series("demo.debug.7", MetricType::RATE, &[], &[(1_700_000_001, 0.5)]));
    payload.series[2].interval = 10;
    let body = zstd::encode_all(&payload.write_to_bytes().unwrap()[..], 3).unwrap();

    let status = send(
        &recorder,
        "POST",
        "/api/v2/series",
        Some("zstd"),
        Some("demo-primary"),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let recent = recorder.recent_series(10);
    assert_eq!(recent.len(), 3);
    // Most recent first.
    assert_eq!(recent[2].name, "demo.canary");
    assert_eq!(recent[2].kind, SeriesKind::Count);
    assert_eq!(recent[2].points, 2);
    assert_eq!(recent[2].total, 39.0);
    assert_eq!(recent[2].tags, vec!["host:adp-demo".to_string()]);
    assert_eq!(recent[2].api_key.as_deref(), Some("demo-primary"));
    assert_eq!(recent[2].path, "/api/v2/series");
    assert_eq!(recent[1].name, "demo.platform.queue_depth");
    assert_eq!(recent[1].kind, SeriesKind::Gauge);
    assert_eq!(
        recent[1].tags,
        vec!["team:platform".to_string(), "host:adp-demo".to_string()]
    );
    assert_eq!(recent[1].total, 57.0);
    assert_eq!(recent[0].kind, SeriesKind::Rate);
    // A rate's total is its value times its interval.
    assert_eq!(recent[0].total, 5.0);

    let summary = recorder.summary(unix_millis());
    assert_eq!(summary.requests_total, 1);
    assert_eq!(summary.refused, 0);
    assert_eq!(summary.points_total, 4);
    assert_eq!(summary.canary_total, 39.0);
    assert_eq!(summary.api_keys.len(), 1);
    assert_eq!(summary.api_keys[0].key, "demo-primary");
    assert!(summary.api_keys[0].expected);
    let families = summary.families.iter().map(|f| f.family.as_str()).collect::<Vec<_>>();
    assert_eq!(families, ["demo.canary", "demo.debug.*", "demo.platform.queue_depth"]);
}

#[tokio::test]
async fn sketch_and_json_payloads_are_decoded_and_counted() {
    let recorder = primary();

    let mut dogsketch = Dogsketch::new();
    dogsketch.ts = 1_700_000_000;
    dogsketch.cnt = 3;
    dogsketch.min = 20.0;
    dogsketch.max = 99.0;
    dogsketch.avg = 50.0;
    dogsketch.sum = 150.0;
    dogsketch.set_k(vec![1338]);
    dogsketch.set_n(vec![3]);
    let mut sketch = Sketch::new();
    sketch.set_metric("demo.checkout.latency".into());
    sketch.set_host("adp-demo".into());
    sketch.tags.push("team:checkout".into());
    sketch.dogsketches.push(dogsketch);
    let mut payload = SketchPayload::new();
    payload.sketches.push(sketch);
    let body = zstd::encode_all(&payload.write_to_bytes().unwrap()[..], 3).unwrap();
    let status = send(
        &recorder,
        "POST",
        "/api/beta/sketches",
        Some("zstd"),
        Some("demo-primary"),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let json = br#"{"series": [{"metric": "demo.canary", "points": [[1700000000, 0.5]], "type": "rate",
                    "interval": 10, "host": "adp-demo", "tags": []}]}"#;
    let status = send(
        &recorder,
        "POST",
        "/api/v1/series",
        None,
        Some("demo-primary"),
        json.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let recent = recorder.recent_series(10);
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[1].name, "demo.checkout.latency");
    assert_eq!(recent[1].kind, SeriesKind::Sketch);
    assert_eq!(recent[1].points, 1);
    assert_eq!(recent[1].total, 3.0);
    assert_eq!(
        recent[1].tags,
        vec!["team:checkout".to_string(), "host:adp-demo".to_string()]
    );
    assert_eq!(recent[0].kind, SeriesKind::Rate);
    assert_eq!(recorder.summary(unix_millis()).canary_total, 5.0);
}

#[tokio::test]
async fn validation_probes_and_other_paths_are_answered_and_bad_payloads_are_refused() {
    let recorder = primary();
    let cases = [
        ("GET", "/api/v1/validate", b"".to_vec(), StatusCode::OK),
        ("POST", "/api/v1/check_run", b"[]".to_vec(), StatusCode::ACCEPTED),
        ("POST", "/intake/", b"{\"metadata\": 1}".to_vec(), StatusCode::ACCEPTED),
        ("POST", "/api/v2/series", b"{}".to_vec(), StatusCode::ACCEPTED),
        (
            "POST",
            "/api/v2/series",
            b"\xff\xff\xff not protobuf".to_vec(),
            StatusCode::BAD_REQUEST,
        ),
        ("POST", "/api/v1/series", b"[not json".to_vec(), StatusCode::BAD_REQUEST),
    ];
    for (method, path, body, expected) in cases {
        let status = send(&recorder, method, path, None, Some("demo-team-b"), body).await;
        assert_eq!(status, expected, "{} {}", method, path);
    }

    let summary = recorder.summary(unix_millis());
    assert_eq!(summary.requests_total, 6);
    assert_eq!(summary.refused, 2);
    assert_eq!(summary.points_total, 0);
    assert!(summary.families.is_empty());
    // The intake for the primary endpoint saw another endpoint's key: the UI flags it.
    assert_eq!(summary.api_keys[0].key, "demo-team-b");
    assert!(!summary.api_keys[0].expected);
    let requests = recorder.recent_requests(10);
    assert_eq!(requests[0].path, "/api/v1/series");
    assert_eq!(requests[0].status, 400);
    assert!(requests[0].error.is_some());
    assert_eq!(requests[5].path, "/api/v1/validate");
    assert_eq!(requests[5].status, 200);

    // The forwarder's key validation sends the key as a query parameter rather than a header.
    let status = send(
        &recorder,
        "GET",
        "/api/v1/validate?api_key=demo-primary",
        None,
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let validation = &recorder.recent_requests(1)[0];
    assert_eq!(validation.path, "/api/v1/validate");
    assert_eq!(validation.api_key.as_deref(), Some("demo-primary"));
}

#[test]
fn numbered_metrics_form_one_family() {
    let cases = [
        ("demo.debug.7", "demo.debug.*"),
        ("demo.debug.19", "demo.debug.*"),
        ("demo.canary", "demo.canary"),
        ("demo.v2.requests", "demo.v2.requests"),
        ("demo.debug.7a", "demo.debug.7a"),
        ("demo.", "demo."),
        ("1234", "1234"),
    ];
    for (name, family) in cases {
        assert_eq!(family_of(name), family, "{}", name);
    }
}

fn record_points(recorder: &IntakeRecorder, received_ms: u64, name: &str, points: usize) {
    let request = RequestRecord {
        received_ms,
        path: "/api/v2/series".to_string(),
        api_key: Some("demo-primary".to_string()),
        status: 202,
        series: 1,
        error: None,
    };
    let mut payload = MetricPayload::new();
    let values = (0..points).map(|i| (i as i64 + 1, 1.0)).collect::<Vec<_>>();
    payload.series.push(series(name, MetricType::COUNT, &[], &values));
    recorder.record(request, Metric::try_from_series_v2(payload).unwrap());
}

#[test]
fn families_report_events_per_second_over_the_history_window_and_their_age() {
    let recorder = primary();
    // Seconds 1000, 1000, 1002, and 1029 (the current second).
    record_points(&recorder, 1_000_100, "demo.debug.1", 2);
    record_points(&recorder, 1_000_900, "demo.debug.2", 3);
    record_points(&recorder, 1_002_000, "demo.debug.1", 1);
    record_points(&recorder, 1_029_500, "demo.debug.3", 4);

    let summary = recorder.summary(1_029_750);
    let family: &FamilySummary = &summary.families[0];
    assert_eq!(family.family, "demo.debug.*");
    assert_eq!(family.names, 3);
    assert_eq!(family.example, "demo.debug.1");
    assert_eq!(family.points_total, 10);
    assert_eq!(family.last_seen_ms, 1_029_500);
    assert_eq!(family.age_ms, 250);
    assert_eq!(family.unit, FamilyUnit::Events);
    // The family stopped arriving for 27 s, so its current run began with the last request.
    assert_eq!(family.active_since_ms, 1_029_500);
    // The window covers seconds 1000 through 1029, oldest first. Each point here counts 1.
    let mut expected = vec![0.0; HISTORY_SECS as usize];
    expected[0] = 5.0;
    expected[2] = 1.0;
    expected[29] = 4.0;
    assert_eq!(family.per_second, expected);

    // Ten seconds later, the oldest buckets have left the window.
    let later = recorder.summary(1_039_000);
    let mut expected = vec![0.0; HISTORY_SECS as usize];
    expected[19] = 4.0;
    assert_eq!(later.families[0].per_second, expected);
    assert_eq!(later.families[0].age_ms, 9_500);

    // Past the retention period, the family is no longer summarized.
    assert!(recorder.summary(1_029_500 + 120_001).families.is_empty());
}

/// Records a request at `received_ms` carrying one series of each given kind, each a single point at `timestamp`.
fn record_flush(recorder: &IntakeRecorder, received_ms: u64, series: &[(&str, MetricType, f64)], timestamp: i64) {
    let request = RequestRecord {
        received_ms,
        path: "/api/v2/series".to_string(),
        api_key: Some("demo-primary".to_string()),
        status: 202,
        series: series.len(),
        error: None,
    };
    let mut payload = MetricPayload::new();
    for (name, kind, value) in series {
        payload
            .series
            .push(self::series(name, *kind, &[], &[(timestamp, *value)]));
    }
    recorder.record(request, Metric::try_from_series_v2(payload).unwrap());
}

#[test]
fn counts_report_what_was_sent_per_second_whatever_the_flush_and_gauges_report_points() {
    let recorder = primary();
    // After 1-second aggregation, a counter sent 20 times a second arrives as one point of 20 per flush, and one sent
    // 100 times a second as one point of 100: the events per second tell them apart, the points would not.
    for second in 0..4u64 {
        record_flush(
            &recorder,
            1_000_000 + second * 1000,
            &[
                ("demo.checkout.requests", MetricType::COUNT, 20.0),
                ("demo.catalog.views", MetricType::COUNT, 100.0),
                ("demo.platform.queue_depth", MetricType::GAUGE, 57.0),
            ],
            1_000 + second as i64,
        );
    }
    let summary = recorder.summary(1_003_500);
    let family = |name: &str| summary.families.iter().find(|f| f.family == name).unwrap();
    assert_eq!(&family("demo.checkout.requests").per_second[26..], [20.0; 4]);
    assert_eq!(&family("demo.catalog.views").per_second[26..], [100.0; 4]);
    let gauge = family("demo.platform.queue_depth");
    assert_eq!(gauge.unit, FamilyUnit::Points);
    assert_eq!(&gauge.per_second[26..], [1.0; 4]);
    // The families arrived without a gap since their first request.
    assert_eq!(family("demo.catalog.views").active_since_ms, 1_000_000);
}

#[test]
fn canary_counts_are_kept_by_point_timestamp_for_conservation_checks() {
    let recorder = primary();
    let canary = [("demo.canary", MetricType::COUNT, 20.0)];
    // Flushes of seconds 1000 and 1001 arrive a little later, in seconds 1001 and 1002.
    record_flush(&recorder, 1_001_200, &canary, 1_000);
    record_flush(&recorder, 1_002_300, &canary, 1_001);
    assert_eq!(recorder.canaries_between(1_000, 1_001), 20.0);
    assert_eq!(recorder.canaries_between(1_000, 1_002), 40.0);
    assert_eq!(recorder.canaries_between(1_002, 1_010), 0.0);
    // A timestamp far from the time of arrival is not counted, so it cannot push the real ones out.
    record_flush(&recorder, 1_003_000, &canary, 9_000_000);
    record_flush(&recorder, 1_003_000, &canary, 10);
    assert_eq!(recorder.canaries_between(0, u64::MAX), 40.0);
    assert_eq!(recorder.summary(1_003_000).canary_total, 80.0);
}

#[test]
fn families_and_api_keys_are_bounded_whatever_arrives() {
    let recorder = primary();
    for i in 0..(MAX_FAMILIES + 10) {
        let request = RequestRecord {
            received_ms: 1_000_000 + i as u64,
            path: "/api/v2/series".to_string(),
            api_key: Some(format!("key-{}", i)),
            status: 202,
            series: 1,
            error: None,
        };
        let mut payload = MetricPayload::new();
        payload.series.push(series(
            &format!("noise.family_{}", i),
            MetricType::GAUGE,
            &[],
            &[(1, 1.0)],
        ));
        recorder.record(request, Metric::try_from_series_v2(payload).unwrap());
    }
    let summary = recorder.summary(1_001_000);
    assert_eq!(summary.families.len(), MAX_FAMILIES);
    // The families seen longest ago made room for the new ones.
    assert!(!summary.families.iter().any(|f| f.family == "noise.family_0"));
    assert!(summary
        .families
        .iter()
        .any(|f| f.family == format!("noise.family_{}", MAX_FAMILIES + 9)));
    assert_eq!(summary.api_keys.len(), MAX_API_KEYS);
    assert_eq!(
        summary.other_api_key_requests,
        (MAX_FAMILIES + 10 - MAX_API_KEYS) as u64
    );
    assert_eq!(summary.requests_total, (MAX_FAMILIES + 10) as u64);
}

#[tokio::test]
async fn refused_requests_are_answered_and_recorded_with_their_reason() {
    let recorder = primary();
    let payload = {
        let mut payload = MetricPayload::new();
        payload
            .series
            .push(series("demo.canary", MetricType::COUNT, &[], &[(1, 1.0)]));
        payload.write_to_bytes().unwrap()
    };
    // A v3 payload that declares 2^44 points: decoding it must refuse it, not try to reserve memory for them.
    let oversized_v3 = {
        let mut data = datadog_protos::metrics::v3::MetricData::new();
        data.types = vec![0x11]; // A count with integer values.
        data.nameRefs = vec![0];
        data.tagsetRefs = vec![0];
        data.numPoints = vec![1 << 44];
        let mut v3 = datadog_protos::metrics::v3::Payload::new();
        v3.metricData = Some(data).into();
        v3.write_to_bytes().unwrap()
    };
    /// A case's name, path, content encoding, body, expected status, and a fragment of the recorded reason.
    type Case = (
        &'static str,
        &'static str,
        Option<&'static str>,
        Vec<u8>,
        StatusCode,
        &'static str,
    );
    let cases: [Case; 4] = [
        (
            "undecompressable zstd",
            "/api/v2/series",
            Some("zstd"),
            b"not a zstd frame".to_vec(),
            StatusCode::BAD_REQUEST,
            "cannot be read or decompressed",
        ),
        (
            "unsupported encoding",
            "/api/v2/series",
            Some("br"),
            payload.clone(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Unsupported content encoding \"br\"",
        ),
        (
            "oversized v3 point count",
            "/api/intake/metrics/v3/series",
            None,
            oversized_v3,
            StatusCode::BAD_REQUEST,
            "Ran out of timestamps",
        ),
        (
            "too large once decompressed",
            "/api/v2/series",
            Some("zstd"),
            zstd::encode_all(&vec![0u8; 4096][..], 3).unwrap(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "larger than 1024 bytes",
        ),
    ];
    for (name, path, encoding, body, expected, reason) in cases {
        let mut request = Request::post(path).header("DD-API-KEY", "demo-primary");
        if let Some(encoding) = encoding {
            request = request.header("content-encoding", encoding);
        }
        let response = router_with_limit(recorder.clone(), 1024)
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{}", name);
        let recorded = &recorder.recent_requests(1)[0];
        assert_eq!(recorded.status, expected.as_u16(), "{}", name);
        let error = recorded.error.as_deref().unwrap_or_default();
        assert!(error.contains(reason), "{}: {}", name, error);
    }
    let summary = recorder.summary(unix_millis());
    assert_eq!((summary.requests_total, summary.refused), (4, 4));
    assert_eq!(summary.last_refusal.unwrap().status, 413);
    assert!(summary.families.is_empty());

    // Under the limit, the same intake decodes the payload.
    let response = router_with_limit(recorder.clone(), 1024)
        .oneshot(Request::post("/api/v2/series").body(Body::from(payload)).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn a_paused_intake_answers_unavailable_records_nothing_it_was_sent_and_refuses_nothing() {
    let team_d = DemoPorts::default().recipients()[4];
    assert_eq!((team_d.name, team_d.declared), ("team-d", true));
    let recorder = IntakeRecorder::new(team_d);
    let payload = {
        let mut payload = MetricPayload::new();
        payload.series.push(series(
            "demo.platform.queue_depth",
            MetricType::GAUGE,
            &["team:platform"],
            &[(1, 3.0)],
        ));
        payload.write_to_bytes().unwrap()
    };
    let key = Some("demo-team-d-key");

    recorder.set_paused(true);
    for _ in 0..2 {
        let status = send(&recorder, "POST", "/api/v2/series", None, key, payload.clone()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }
    let summary = recorder.summary(unix_millis());
    assert!(summary.paused && summary.declared);
    assert_eq!(
        (summary.requests_total, summary.unavailable, summary.refused),
        (2, 2, 0)
    );
    assert!(summary.families.is_empty() && summary.points_total == 0);
    assert_eq!(recorder.recent_requests(1)[0].status, 503);
    assert!(summary.api_keys[0].expected);

    // Resumed, the next attempt of the same payload is decoded and recorded.
    recorder.set_paused(false);
    let status = send(&recorder, "POST", "/api/v2/series", None, key, payload).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let summary = recorder.summary(unix_millis());
    assert!(!summary.paused);
    assert_eq!((summary.requests_total, summary.unavailable), (3, 2));
    assert_eq!(summary.families[0].family, "demo.platform.queue_depth");
}
