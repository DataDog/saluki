use axum::{
    routing::{get, post},
    Router,
};

mod handlers;
use self::handlers::*;

mod state;
use self::state::MetricsState;

pub fn build_metrics_router() -> Router {
    Router::new()
        .route("/metrics/dump", get(handle_metrics_dump))
        .route("/metrics/dump_requests", get(handle_metrics_dump_requests))
        .route("/api/v1/series", post(handle_series_v1))
        .route("/api/v2/series", post(handle_series_v2))
        .route("/api/intake/metrics/v3/series", post(handle_metrics_v3))
        .route("/api/intake/metrics/v3beta/series", post(handle_metrics_v3))
        .route("/api/beta/sketches", post(handle_sketch_beta))
        .route("/api/intake/metrics/v3/sketches", post(handle_metrics_v3))
        .with_state(MetricsState::new())
}

#[cfg(test)]
mod tests {
    use axum::{http::StatusCode, Router};
    use datadog_protos::metrics::{
        metric_payload::{MetricPoint, MetricSeries, MetricType, Resource},
        sketch_payload::Sketch,
        v3::{MetricData, Payload as V3Payload},
        Dogsketch, MetricPayload, SketchPayload,
    };
    use protobuf::Message as _;
    use stele::{IntakeRequest, Metric};

    use crate::app::test_util::{
        encode, get_bytes, get_json, post, post_in_each_encoding, router, ENCODINGS, USER_AGENT,
    };

    const SERIES_V1_BODY: &[u8] =
        br#"{"series":[{"metric":"v1.gauge","points":[[100,5.0]],"tags":["env:prod"],"host":"h","type":"gauge","interval":0}]}"#;

    fn series_v2_body() -> Vec<u8> {
        let mut resource = Resource::new();
        resource.set_type("host".into());
        resource.set_name("host-a".into());

        let mut point = MetricPoint::new();
        point.value = 3.0;
        point.timestamp = 200;

        let mut series = MetricSeries::new();
        series.set_metric("v2.count".into());
        series.set_type(MetricType::COUNT);
        series.tags.push("env:prod".into());
        series.resources.push(resource);
        series.points.push(point);

        let mut payload = MetricPayload::new();
        payload.series.push(series);
        payload.write_to_bytes().unwrap()
    }

    fn sketch_body() -> Vec<u8> {
        let mut dogsketch = Dogsketch::new();
        dogsketch.ts = 300;
        dogsketch.cnt = 4;
        dogsketch.min = 1.0;
        dogsketch.max = 1.0;
        dogsketch.avg = 1.0;
        dogsketch.sum = 4.0;
        dogsketch.set_k(vec![0]);
        dogsketch.set_n(vec![4]);

        let mut sketch = Sketch::new();
        sketch.set_metric("sketch.distribution".into());
        sketch.set_host("host-a".into());
        sketch.dogsketches.push(dogsketch);

        let mut payload = SketchPayload::new();
        payload.sketches.push(sketch);
        payload.write_to_bytes().unwrap()
    }

    fn v3_body() -> Vec<u8> {
        // Dictionary strings are length-prefixed. The value type 0x30 reads the point from `valsFloat64`.
        let mut data = MetricData::new();
        data.dictNameStr = [&[b"v3.gauge".len() as u8][..], b"v3.gauge"].concat();
        data.types = vec![3 | 0x30];
        data.nameRefs = vec![1];
        data.tagsetRefs = vec![0];
        data.resourcesRefs = vec![0];
        data.intervals = vec![0];
        data.numPoints = vec![1];
        data.timestamps = vec![400];
        data.valsFloat64 = vec![2.25];

        let mut payload = V3Payload::new();
        payload.metricData = Some(data).into();
        payload.write_to_bytes().unwrap()
    }

    async fn dump_requests(router: &Router) -> Vec<IntakeRequest<Vec<Metric>>> {
        get_json(router, "/metrics/dump_requests").await
    }

    /// Each payload with the metrics that it decodes to and the paths that accept it.
    fn payloads() -> Vec<(&'static [&'static str], Vec<u8>, Vec<Metric>)> {
        let series_v2 = series_v2_body();
        let sketch = sketch_body();
        let v3 = v3_body();

        vec![
            (
                &["/api/v1/series"],
                SERIES_V1_BODY.to_vec(),
                Metric::try_from_series_v1(SERIES_V1_BODY).unwrap(),
            ),
            (
                &["/api/v2/series"],
                series_v2.clone(),
                Metric::try_from_series_v2(MetricPayload::parse_from_bytes(&series_v2).unwrap()).unwrap(),
            ),
            (
                &["/api/beta/sketches"],
                sketch.clone(),
                Metric::try_from_sketch(SketchPayload::parse_from_bytes(&sketch).unwrap()).unwrap(),
            ),
            (
                &[
                    "/api/intake/metrics/v3/series",
                    "/api/intake/metrics/v3beta/series",
                    "/api/intake/metrics/v3/sketches",
                ],
                v3.clone(),
                Metric::try_from_v3(V3Payload::parse_from_bytes(&v3).unwrap()).unwrap(),
            ),
        ]
    }

    #[tokio::test]
    async fn records_requests_for_each_metrics_endpoint() {
        for (paths, payload, expected) in payloads() {
            assert_eq!(expected.len(), 1);

            for path in paths {
                let router = router();
                post_in_each_encoding(&router, path, &payload).await;

                let requests = dump_requests(&router).await;
                assert_eq!(requests.len(), ENCODINGS.len(), "{path}");
                for (request, encoding) in requests.iter().zip(ENCODINGS) {
                    assert_eq!(request.path(), *path);
                    assert_eq!(request.header("user-agent"), Some(USER_AGENT), "{path}");
                    assert_eq!(request.header("content-encoding"), encoding, "{path}");
                    assert_eq!(request.items(), &expected, "{path}");
                }
            }
        }
    }

    #[tokio::test]
    async fn flat_metrics_dump_lists_the_metrics_of_every_request() {
        let router = router();
        let mut expected = Vec::new();
        for (paths, payload, metrics) in payloads() {
            post_in_each_encoding(&router, paths[0], &payload).await;
            for _ in ENCODINGS {
                expected.extend(metrics.clone());
            }
        }

        assert_eq!(
            get_bytes(&router, "/metrics/dump").await,
            serde_json::to_vec(&expected).unwrap()
        );
    }

    #[tokio::test]
    async fn rejects_malformed_payloads_without_recording_them() {
        let router = router();

        let mut series = MetricSeries::new();
        series.set_metric("unspecified.type".into());
        let mut unspecified_type = MetricPayload::new();
        unspecified_type.series.push(series);

        let mut corrupt_gzip = encode(Some("gzip"), &series_v2_body()).await;
        corrupt_gzip.truncate(corrupt_gzip.len() / 2);

        let cases = [
            (
                "invalid series v1 JSON",
                "/api/v1/series",
                None,
                b"{\"series\":".to_vec(),
            ),
            ("invalid series v2 protobuf", "/api/v2/series", None, vec![0xff; 8]),
            (
                "series v2 with unspecified type",
                "/api/v2/series",
                None,
                unspecified_type.write_to_bytes().unwrap(),
            ),
            ("invalid sketch protobuf", "/api/beta/sketches", None, vec![0xff; 8]),
            (
                "invalid v3 protobuf",
                "/api/intake/metrics/v3/series",
                None,
                vec![0xff; 8],
            ),
            (
                "v3 without metric data",
                "/api/intake/metrics/v3/series",
                None,
                Vec::new(),
            ),
            ("corrupt gzip", "/api/v2/series", Some("gzip"), corrupt_gzip),
        ];
        for (name, path, encoding, body) in cases {
            let status = post(&router, path, encoding, body).await;
            assert!(status.is_client_error(), "{name} should be rejected, got {status}");
        }

        assert!(dump_requests(&router).await.is_empty());
        assert_eq!(get_bytes(&router, "/metrics/dump").await, b"[]");
    }

    #[tokio::test]
    async fn diagnostic_requests_are_not_recorded() {
        let router = router();

        for path in ["/api/v1/series", "/api/v2/series"] {
            assert_eq!(post(&router, path, None, b"{}".to_vec()).await, StatusCode::ACCEPTED);
        }

        assert!(dump_requests(&router).await.is_empty());
    }
}
