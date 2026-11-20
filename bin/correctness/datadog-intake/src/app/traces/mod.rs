use axum::{
    routing::{get, post},
    Router,
};

mod handlers;
use self::handlers::*;

mod state;
use self::state::TracesState;

pub fn build_traces_router() -> Router {
    Router::new()
        .route("/traces/dump_spans", get(handle_trace_spans_dump))
        .route("/traces/dump_stats", get(handle_trace_stats_dump))
        .route("/traces/dump_requests", get(handle_trace_span_requests_dump))
        .route("/traces/dump_stats_requests", get(handle_trace_stats_requests_dump))
        .route("/api/v0.2/traces", post(handle_v02_traces))
        .route("/api/v0.2/stats", post(handle_v02_stats))
        .with_state(TracesState::new())
}

#[cfg(test)]
mod tests {
    use axum::{http::StatusCode, Router};
    use datadog_protos::sketches::{DDSketch, IndexMapping};
    use datadog_protos::traces::{
        AgentPayload, ClientGroupedStats, ClientStatsBucket, ClientStatsPayload, Span as ProtoSpan, StatsPayload,
        TraceChunk, TracerPayload,
    };
    use protobuf::Message as _;
    use serde_json::Value;
    use stele::{ClientStatisticsAggregator, IntakeRequest, Span};

    use crate::app::test_util::{
        encode, get_bytes, get_json, post, post_in_each_encoding, router, ENCODINGS, USER_AGENT,
    };

    const TRACES_PATH: &str = "/api/v0.2/traces";
    const STATS_PATH: &str = "/api/v0.2/stats";

    fn agent_payload() -> AgentPayload {
        let mut span = ProtoSpan::new();
        span.set_service("svc".into());
        span.set_name("op".into());
        span.set_resource("res".into());
        span.set_traceID(7);
        span.set_spanID(8);

        let mut chunk = TraceChunk::new();
        chunk.set_spans(vec![span]);

        let mut tracer_payload = TracerPayload::new();
        tracer_payload.set_chunks(vec![chunk]);

        let mut payload = AgentPayload::new();
        payload.set_hostName("host-a".into());
        payload.set_tracerPayloads(vec![tracer_payload]);
        payload
    }

    /// Returns an empty sketch with the mapping that the APM stats sketches use: 1% relative accuracy.
    fn empty_sketch() -> Vec<u8> {
        let mut mapping = IndexMapping::new();
        mapping.gamma = 1.01 / 0.99;

        let mut sketch = DDSketch::new();
        sketch.set_mapping(mapping);
        sketch.write_to_bytes().unwrap()
    }

    /// Builds a stats payload whose single group is the same for every `agent_version`.
    fn stats_payload(agent_version: &str) -> StatsPayload {
        let mut grouped = ClientGroupedStats::new();
        grouped.set_okSummary(empty_sketch());
        grouped.set_errorSummary(empty_sketch());
        grouped.set_service("svc".into());
        grouped.set_name("op".into());
        grouped.set_resource("res".into());
        grouped.set_hits(1);

        let mut bucket = ClientStatsBucket::new();
        bucket.set_start(0);
        bucket.set_duration(10);
        bucket.set_stats(vec![grouped]);

        let mut client = ClientStatsPayload::new();
        client.set_hostname("host-a".into());
        client.set_stats(vec![bucket]);

        let mut payload = StatsPayload::new();
        payload.set_agentHostname("agent-host".into());
        payload.set_agentVersion(agent_version.into());
        payload.set_stats(vec![client]);
        payload
    }

    fn stats_body(payload: &StatsPayload) -> Vec<u8> {
        rmp_serde::to_vec_named(payload).unwrap()
    }

    fn aggregated_stats<'a>(payloads: impl IntoIterator<Item = &'a StatsPayload>) -> ClientStatisticsAggregator {
        let mut aggregator = ClientStatisticsAggregator::default();
        for payload in payloads {
            aggregator.merge_payload(payload).unwrap();
        }
        aggregator
    }

    /// Converts to JSON, so that comparisons ignore the iteration order of the maps that stele types hold.
    fn json(value: &impl serde::Serialize) -> Value {
        serde_json::to_value(value).unwrap()
    }

    async fn dump_span_requests(router: &Router) -> Vec<IntakeRequest<Vec<Span>>> {
        get_json(router, "/traces/dump_requests").await
    }

    async fn dump_stats_requests(router: &Router) -> Vec<IntakeRequest<ClientStatisticsAggregator>> {
        get_json(router, "/traces/dump_stats_requests").await
    }

    #[tokio::test]
    async fn records_trace_requests_with_each_encoding_and_original_headers() {
        let router = router();
        let payload = agent_payload();
        post_in_each_encoding(&router, TRACES_PATH, &payload.write_to_bytes().unwrap()).await;

        let expected = Span::get_spans_from_agent_payload(&payload);
        assert_eq!(expected.len(), 1);

        let requests = dump_span_requests(&router).await;
        assert_eq!(requests.len(), ENCODINGS.len());
        for (request, encoding) in requests.iter().zip(ENCODINGS) {
            assert_eq!(request.path(), TRACES_PATH);
            assert_eq!(request.header("user-agent"), Some(USER_AGENT));
            assert_eq!(request.header("content-encoding"), encoding);
            assert_eq!(request.items(), &expected);
        }
    }

    #[tokio::test]
    async fn flat_spans_dump_lists_the_spans_of_every_request() {
        let router = router();
        let payload = agent_payload();
        post_in_each_encoding(&router, TRACES_PATH, &payload.write_to_bytes().unwrap()).await;

        let spans: Vec<Span> = get_json(&router, "/traces/dump_spans").await;
        let expected = vec![Span::get_spans_from_agent_payload(&payload); ENCODINGS.len()].concat();
        assert_eq!(json(&spans), json(&expected));
    }

    #[tokio::test]
    async fn records_stats_requests_with_each_encoding_and_original_headers() {
        let router = router();
        let payload = stats_payload("7.0.0");
        post_in_each_encoding(&router, STATS_PATH, &stats_body(&payload)).await;

        let expected = aggregated_stats([&payload]);
        assert_eq!(expected.groups().len(), 1);

        let requests = dump_stats_requests(&router).await;
        assert_eq!(requests.len(), ENCODINGS.len());
        for (request, encoding) in requests.iter().zip(ENCODINGS) {
            assert_eq!(request.path(), STATS_PATH);
            assert_eq!(request.header("user-agent"), Some(USER_AGENT));
            assert_eq!(request.header("content-encoding"), encoding);
            assert_eq!(json(request.items()), json(&expected));
        }
    }

    #[tokio::test]
    async fn flat_stats_dump_merges_the_stats_of_every_request() {
        let router = router();
        let payload = stats_payload("7.0.0");
        post_in_each_encoding(&router, STATS_PATH, &stats_body(&payload)).await;

        let stats: ClientStatisticsAggregator = get_json(&router, "/traces/dump_stats").await;
        assert_eq!(json(&stats), json(&aggregated_stats(vec![&payload; ENCODINGS.len()])));
    }

    #[tokio::test]
    async fn records_stats_request_rejected_for_agent_metadata_mismatch() {
        let router = router();
        let accepted = stats_payload("7.0.0");
        let mismatched = stats_payload("7.1.0");

        let body = stats_body(&accepted);
        assert_eq!(post(&router, STATS_PATH, None, body).await, StatusCode::ACCEPTED);
        let body = encode(Some("gzip"), &stats_body(&mismatched)).await;
        assert_eq!(
            post(&router, STATS_PATH, Some("gzip"), body).await,
            StatusCode::BAD_REQUEST
        );

        let requests = dump_stats_requests(&router).await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].header("content-encoding"), Some("gzip"));
        assert_eq!(json(requests[0].items()), json(&aggregated_stats([&accepted])));
        assert_eq!(json(requests[1].items()), json(&aggregated_stats([&mismatched])));

        // The rejected request leaves the merged stats as they were.
        let stats: ClientStatisticsAggregator = get_json(&router, "/traces/dump_stats").await;
        assert_eq!(json(&stats), json(&aggregated_stats([&accepted])));
    }

    #[tokio::test]
    async fn rejects_malformed_payloads_without_recording_them() {
        let router = router();

        let mut corrupt_gzip = encode(Some("gzip"), &agent_payload().write_to_bytes().unwrap()).await;
        corrupt_gzip.truncate(corrupt_gzip.len() / 2);

        let cases = [
            ("invalid traces protobuf", TRACES_PATH, None, vec![0xff; 8]),
            ("corrupt gzip traces", TRACES_PATH, Some("gzip"), corrupt_gzip.clone()),
            ("invalid stats msgpack", STATS_PATH, None, vec![0xc1; 8]),
            ("corrupt gzip stats", STATS_PATH, Some("gzip"), corrupt_gzip),
        ];
        for (name, path, encoding, body) in cases {
            let status = post(&router, path, encoding, body).await;
            assert!(status.is_client_error(), "{name} should be rejected, got {status}");
        }

        assert!(dump_span_requests(&router).await.is_empty());
        assert!(dump_stats_requests(&router).await.is_empty());
        assert_eq!(get_bytes(&router, "/traces/dump_spans").await, b"[]");
    }
}
