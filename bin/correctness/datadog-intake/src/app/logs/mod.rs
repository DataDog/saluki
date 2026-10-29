use axum::{
    routing::{get, post},
    Router,
};

mod handlers;
use self::handlers::*;

mod state;
pub use self::state::LogsState;

pub fn build_logs_router(state: LogsState) -> Router {
    Router::new()
        .route("/logs/dump", get(handle_logs_dump))
        .route("/api/v2/logs", post(handle_logs))
        // The Agent sends to `/v1/input` when `logs_config.use_v2_api` is false.
        .route("/v1/input", post(handle_logs))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use async_compression::tokio::write::{GzipEncoder, ZstdEncoder};
    use axum::{
        body::Body,
        http::{Request, StatusCode},
        Router,
    };
    use http_body_util::BodyExt as _;
    use serde_json::json;
    use stele::LogsRequest;
    use tokio::io::AsyncWriteExt as _;
    use tower::ServiceExt as _;

    use crate::app::{initialize_app_router, DogStatsDForwardingState};

    // Shaped like the Agent's logs agent output for an OTLP log.
    const AGENT_PAYLOAD: &[u8] = br#"[{"message":"{\"message\":\"hello\",\"status\":\"info\"}","status":"info","timestamp":1793268000123,"hostname":"host-a","service":"svc","ddsource":"otlp_log_ingestion","ddtags":"otel_source:datadog_agent"}]"#;

    // Shaped like ADP's current logs encoder output.
    const ADP_PAYLOAD: &[u8] = br#"[{"message":"{\"message\":\"hello\",\"service\":\"svc\"}","status":"Info","hostname":"host-a","service":"svc","ddsource":"otlp_log_ingestion","ddtags":"otel_source:datadog_agent","@timestamp":"2026-10-29T10:00:00.000Z"},{"message":"{\"message\":\"second\"}"}]"#;

    async fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = GzipEncoder::new(Vec::new());
        encoder.write_all(data).await.unwrap();
        encoder.shutdown().await.unwrap();
        encoder.into_inner()
    }

    async fn zstd(data: &[u8]) -> Vec<u8> {
        let mut encoder = ZstdEncoder::new(Vec::new());
        encoder.write_all(data).await.unwrap();
        encoder.shutdown().await.unwrap();
        encoder.into_inner()
    }

    fn router() -> Router {
        initialize_app_router(DogStatsDForwardingState::new())
    }

    async fn post(router: &Router, path: &str, encoding: Option<&str>, body: Vec<u8>) -> StatusCode {
        let mut request = Request::post(path).header("content-type", "application/json");
        if let Some(encoding) = encoding {
            request = request.header("content-encoding", encoding);
        }
        let request = request
            .header("dd-protocol", "agent-json")
            .body(Body::from(body))
            .unwrap();

        router.clone().oneshot(request).await.unwrap().status()
    }

    async fn dump(router: &Router) -> Vec<LogsRequest> {
        let request = Request::get("/logs/dump").body(Body::empty()).unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn accepts_logs_with_each_encoding_and_records_original_headers() {
        let router = router();

        assert_eq!(
            post(&router, "/api/v2/logs", None, AGENT_PAYLOAD.to_vec()).await,
            StatusCode::ACCEPTED
        );
        assert_eq!(
            post(&router, "/api/v2/logs", Some("gzip"), gzip(AGENT_PAYLOAD).await).await,
            StatusCode::ACCEPTED
        );
        assert_eq!(
            post(&router, "/api/v2/logs", Some("zstd"), zstd(ADP_PAYLOAD).await).await,
            StatusCode::ACCEPTED
        );

        let requests = dump(&router).await;
        assert_eq!(requests.len(), 3);
        for request in &requests {
            assert_eq!(request.path(), "/api/v2/logs");
            assert_eq!(request.header("dd-protocol"), Some("agent-json"));
        }
        assert_eq!(requests[0].header("content-encoding"), None);
        assert_eq!(requests[1].header("content-encoding"), Some("gzip"));
        assert_eq!(requests[2].header("content-encoding"), Some("zstd"));

        // Decompressed payloads parse to the same logs as the uncompressed one.
        assert_eq!(requests[0].logs(), requests[1].logs());
        assert_eq!(requests[0].logs().len(), 1);
        assert_eq!(requests[0].logs()[0].timestamp(), Some(&json!(1793268000123u64)));

        let adp_logs = requests[2].logs();
        assert_eq!(adp_logs.len(), 2);
        assert_eq!(
            adp_logs[0].envelope_properties().get("@timestamp"),
            Some(&json!("2026-10-29T10:00:00.000Z"))
        );
        assert_eq!(adp_logs[1].display_message(), Some(&json!("second")));
    }

    #[tokio::test]
    async fn accepts_logs_on_the_v1_input_path() {
        let router = router();

        assert_eq!(
            post(&router, "/v1/input", Some("zstd"), zstd(AGENT_PAYLOAD).await).await,
            StatusCode::ACCEPTED
        );

        let requests = dump(&router).await;
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path(), "/v1/input");
        assert_eq!(requests[0].logs().len(), 1);
    }

    #[tokio::test]
    async fn rejects_malformed_payloads_without_recording_them() {
        let router = router();

        let mut corrupt_gzip = gzip(AGENT_PAYLOAD).await;
        corrupt_gzip.truncate(corrupt_gzip.len() / 2);
        let corrupt_zstd = b"\x28\xb5\x2f\xfdnot zstd".to_vec();

        let cases = [
            ("invalid JSON", None, b"[{".to_vec()),
            ("object instead of array", None, br#"{"message":"x"}"#.to_vec()),
            ("corrupt gzip", Some("gzip"), corrupt_gzip),
            ("corrupt zstd", Some("zstd"), corrupt_zstd),
        ];
        for (name, encoding, body) in cases {
            let status = post(&router, "/api/v2/logs", encoding, body).await;
            assert!(status.is_client_error(), "{} should be rejected, got {}", name, status);
        }

        assert!(dump(&router).await.is_empty());
    }
}
