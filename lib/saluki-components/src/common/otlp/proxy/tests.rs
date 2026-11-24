use std::convert::Infallible;
use std::io;
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::sync::Mutex;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::Router;
use futures::StreamExt as _;
use http::StatusCode;
use http_body::Frame;
use http_body_util::{BodyExt as _, Full, StreamBody};
use otlp_protos::opentelemetry::proto::collector::metrics::v1::metrics_service_client::MetricsServiceClient;
use otlp_protos::opentelemetry::proto::collector::metrics::v1::{
    ExportMetricsPartialSuccess, ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use prost::Message as _;
use saluki_core::components::ComponentContext;
use saluki_core::observability::ComponentMetricsExt as _;
use saluki_metrics::test::TestRecorder;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpSocket};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use super::*;
use crate::common::otlp::test_util::{
    grpc_frame, grpc_response, header_map, http_ok, FakeUpstream, RecordingHandler, TestClient, TestServers,
};
use crate::common::otlp::{OtlpRoute, OtlpRoutes};

const ALL_PROXIED: OtlpRoutes = OtlpRoutes {
    metrics: OtlpRoute::Proxied,
    logs: OtlpRoute::Proxied,
    traces: OtlpRoute::Proxied,
};

const METRICS_EXPORT_PATH: &str = "/opentelemetry.proto.collector.metrics.v1.MetricsService/Export";

const COMPONENT_ID: &str = "otlp_proxy_test";

/// Starts OTLP servers that proxy every signal to `grpc` and `http`.
///
/// Set a local metrics recorder before the call to see the proxy's counters.
async fn start_proxy(id: &str, grpc: &Destination, http: &Destination, connect_timeout: Duration) -> TestServers {
    let destinations = OtlpProxyDestinations {
        grpc: Some(grpc.clone()),
        http: Some(http.clone()),
    };
    let metrics_builder = MetricsBuilder::from_component_context(&ComponentContext::test_source(COMPONENT_ID));
    let proxy =
        OtlpProxy::with_connect_timeout(&destinations, metrics_builder, connect_timeout).expect("proxy should build");
    TestServers::start(
        id,
        TestServers::configuration().with_routes(ALL_PROXIED).with_proxy(proxy),
        RecordingHandler::default(),
    )
    .await
}

/// Returns the value of a proxy counter.
fn counter(recorder: &TestRecorder, name: &'static str, signal: &'static str, protocol: &'static str) -> Option<u64> {
    recorder.counter((
        name,
        &[
            ("component_id", COMPONENT_ID),
            ("component_type", "source"),
            ("signal", signal),
            ("protocol", protocol),
        ],
    ))
}

#[tokio::test]
async fn http_request_and_response_pass_through() {
    let recorder = TestRecorder::default();
    let _recorder_guard = metrics::set_default_local_recorder(&recorder);

    let upstream = FakeUpstream::tcp(|_| {
        let mut response = http::Response::new(Body::from("upstream body"));
        *response.status_mut() = StatusCode::ACCEPTED;
        *response.headers_mut() = header_map(&[
            ("content-type", "application/x-protobuf"),
            ("x-upstream", "yes"),
            ("connection", "x-upstream-hop"),
            ("x-upstream-hop", "drop me"),
        ]);
        response
    })
    .await;
    let servers = start_proxy(
        "proxy-http-pass-through",
        &upstream.destination,
        &upstream.destination,
        CONNECT_TIMEOUT,
    )
    .await;

    let body = Bytes::from_static(&[0x1f, 0x8b, 0x08, 0x00, 1, 2, 3]);
    let response = TestClient::new(HttpProtocol::Http1)
        .send(
            "POST",
            &format!("http://127.0.0.1:{}/v1/logs?tenant=a&x=1", servers.http_port),
            &[
                ("content-type", "application/x-protobuf"),
                ("content-encoding", "gzip"),
                ("x-custom", "kept"),
                ("connection", "x-hop"),
                ("x-hop", "drop me"),
                ("proxy-authorization", "secret"),
            ],
            body.clone(),
        )
        .await
        .expect("proxy should answer");

    assert_eq!(response.status, StatusCode::ACCEPTED);
    assert_eq!(response.headers["content-type"], "application/x-protobuf");
    assert_eq!(response.headers["x-upstream"], "yes");
    assert!(!response.headers.contains_key("x-upstream-hop"));
    assert!(!response.headers.contains_key("connection"));
    assert_eq!(response.body, "upstream body");

    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    let seen = &requests[0];
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.path_and_query, "/v1/logs?tenant=a&x=1");
    assert_eq!(seen.body, body);
    assert_eq!(seen.headers["content-type"], "application/x-protobuf");
    assert_eq!(seen.headers["content-encoding"], "gzip");
    assert_eq!(seen.headers["x-custom"], "kept");
    for dropped in ["x-hop", "proxy-authorization", "connection", "via"] {
        assert!(!seen.headers.contains_key(dropped), "{dropped} should not be forwarded");
    }
    let upstream_uri = upstream.destination.base_uri().expect("destination should have a URI");
    assert_eq!(
        seen.headers["host"],
        upstream_uri.authority().expect("URI should have an authority").as_str()
    );

    assert_eq!(
        counter(&recorder, "component_requests_relayed_total", "logs", "http"),
        Some(1)
    );
    assert_eq!(
        counter(&recorder, "component_requests_failed_total", "logs", "http"),
        Some(0)
    );

    servers.shutdown().await;
}

#[tokio::test]
async fn http_error_responses_pass_through() {
    // The fake upstream answers with the status in the query, so each case also shows the method reaching it.
    let upstream = FakeUpstream::tcp(|request| {
        let status = request
            .path_and_query
            .split_once("status=")
            .and_then(|(_, status)| status.parse::<u16>().ok())
            .expect("request should carry a status");
        let mut response = http::Response::new(Body::from(format!("{} rejected", request.method)));
        *response.status_mut() = StatusCode::from_u16(status).expect("status should be valid");
        response
    })
    .await;
    let servers = start_proxy(
        "proxy-http-errors",
        &upstream.destination,
        &upstream.destination,
        CONNECT_TIMEOUT,
    )
    .await;

    let mut client = TestClient::new(HttpProtocol::Http1);
    for (method, signal, status) in [("POST", "metrics", 400), ("PUT", "logs", 415), ("GET", "traces", 503)] {
        let response = client
            .send(
                method,
                &format!("http://127.0.0.1:{}/v1/{signal}?status={status}", servers.http_port),
                &[],
                Bytes::new(),
            )
            .await
            .expect("proxy should answer");
        assert_eq!(response.status.as_u16(), status, "{method} {signal}");
        assert_eq!(response.body, format!("{method} rejected"), "{method} {signal}");
    }

    servers.shutdown().await;
}

#[tokio::test]
async fn http2_request_is_forwarded_over_http1() {
    let upstream = FakeUpstream::tcp(http_ok).await;
    let servers = start_proxy(
        "proxy-http2-to-http1",
        &upstream.destination,
        &upstream.destination,
        CONNECT_TIMEOUT,
    )
    .await;

    let response = TestClient::new(HttpProtocol::Http2)
        .send(
            "POST",
            &format!("http://127.0.0.1:{}/v1/metrics", servers.http_port),
            &[("content-type", "application/x-protobuf")],
            "payload",
        )
        .await
        .expect("proxy should answer");

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.version, Version::HTTP_2);
    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].version, Version::HTTP_11);
    assert_eq!(requests[0].body, "payload");

    servers.shutdown().await;
}

#[tokio::test]
async fn grpc_request_and_response_pass_through() {
    let partial_success = ExportMetricsServiceResponse {
        partial_success: Some(ExportMetricsPartialSuccess {
            rejected_data_points: 3,
            error_message: "3 data points rejected".to_string(),
        }),
    }
    .encode_to_vec();
    let response_frame = grpc_frame(&partial_success);
    let upstream = FakeUpstream::tcp({
        let response_frame = response_frame.clone();
        move |_| {
            grpc_response(
                &[("x-upstream-metadata", "yes")],
                Some(response_frame.clone()),
                header_map(&[("grpc-status", "0"), ("x-upstream-trailer", "kept")]),
            )
        }
    })
    .await;
    let servers = start_proxy(
        "proxy-grpc-pass-through",
        &upstream.destination,
        &upstream.destination,
        CONNECT_TIMEOUT,
    )
    .await;

    // The proxy doesn't decode the message, so any bytes do.
    let request_frame = grpc_frame(b"not decoded by the proxy");
    let response = TestClient::new(HttpProtocol::Http2)
        .send(
            "POST",
            &format!("http://127.0.0.1:{}{METRICS_EXPORT_PATH}", servers.grpc_port),
            &[
                ("content-type", "application/grpc"),
                ("te", "trailers"),
                ("grpc-timeout", "5S"),
                ("grpc-encoding", "gzip"),
                ("grpc-accept-encoding", "gzip"),
                ("user-agent", "test-exporter/1.0"),
                ("x-tenant-bin", "AAEC"),
            ],
            request_frame.clone(),
        )
        .await
        .expect("proxy should answer");

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.headers["content-type"], "application/grpc");
    assert_eq!(response.headers["x-upstream-metadata"], "yes");
    assert_eq!(response.body, response_frame);
    assert_eq!(
        response.trailers,
        Some(header_map(&[("grpc-status", "0"), ("x-upstream-trailer", "kept")]))
    );

    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    let seen = &requests[0];
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.path_and_query, METRICS_EXPORT_PATH);
    assert_eq!(seen.version, Version::HTTP_2);
    assert_eq!(seen.body, request_frame);
    for (name, value) in [
        ("content-type", "application/grpc"),
        ("te", "trailers"),
        ("grpc-timeout", "5S"),
        ("grpc-encoding", "gzip"),
        ("grpc-accept-encoding", "gzip"),
        ("user-agent", "test-exporter/1.0"),
        ("x-tenant-bin", "AAEC"),
    ] {
        assert_eq!(seen.headers[name], value, "{name}");
    }

    // A real client decodes the upstream's partial success.
    let channel = tonic::transport::Endpoint::from_shared(format!("http://127.0.0.1:{}", servers.grpc_port))
        .expect("endpoint should parse")
        .connect()
        .await
        .expect("should connect to the proxy");
    let response = MetricsServiceClient::new(channel)
        .export(ExportMetricsServiceRequest::default())
        .await
        .expect("export should succeed");
    assert_eq!(
        response.into_inner().encode_to_vec(),
        partial_success,
        "the client should see the upstream's partial success"
    );

    servers.shutdown().await;
}

#[tokio::test]
async fn grpc_error_statuses_pass_through() {
    // The upstream sends one status as a trailers-only response, and the other in trailers after the headers.
    let details = Bytes::from_static(b"status details");
    let upstream = FakeUpstream::tcp({
        let details = details.clone();
        move |request| match request.headers["x-respond"].to_str() {
            Ok("trailers-only") => {
                Status::with_details(tonic::Code::ResourceExhausted, "too large", details.clone()).into_http()
            }
            Ok("trailers") => {
                // The status fields of a trailers-only response go in the trailers instead.
                let status = Status::with_details(tonic::Code::InvalidArgument, "bad data", details.clone());
                let mut trailers = status.into_http::<Body>().into_parts().0.headers;
                trailers.remove("content-type");
                trailers.insert("x-upstream-trailer", HeaderValue::from_static("kept"));
                grpc_response(&[], None, trailers)
            }
            other => panic!("unexpected `x-respond` {other:?}"),
        }
    })
    .await;
    let servers = start_proxy(
        "proxy-grpc-errors",
        &upstream.destination,
        &upstream.destination,
        CONNECT_TIMEOUT,
    )
    .await;

    let mut client = TestClient::new(HttpProtocol::Http2);
    let uri = format!("http://127.0.0.1:{}{METRICS_EXPORT_PATH}", servers.grpc_port);
    let headers = |respond| {
        [
            ("content-type", "application/grpc"),
            ("te", "trailers"),
            ("x-respond", respond),
        ]
    };

    let trailers_only = client
        .send("POST", &uri, &headers("trailers-only"), grpc_frame(&[]))
        .await
        .expect("proxy should answer");
    assert_eq!(trailers_only.status, StatusCode::OK);
    assert_eq!(trailers_only.headers["grpc-status"], "8");
    assert_eq!(trailers_only.headers["grpc-message"], "too%20large");
    assert!(trailers_only.headers.contains_key("grpc-status-details-bin"));
    assert!(trailers_only.body.is_empty());
    assert_eq!(trailers_only.trailers, None);

    let in_trailers = client
        .send("POST", &uri, &headers("trailers"), grpc_frame(&[]))
        .await
        .expect("proxy should answer");
    assert_eq!(in_trailers.status, StatusCode::OK);
    assert!(!in_trailers.headers.contains_key("grpc-status"));
    let trailers = in_trailers.trailers.expect("response should have trailers");
    assert_eq!(trailers["grpc-status"], "3");
    assert_eq!(trailers["grpc-message"], "bad%20data");
    assert!(trailers.contains_key("grpc-status-details-bin"));
    assert_eq!(trailers["x-upstream-trailer"], "kept");

    // A real client sees the exact upstream status of each form.
    let channel = tonic::transport::Endpoint::from_shared(format!("http://127.0.0.1:{}", servers.grpc_port))
        .expect("endpoint should parse")
        .connect()
        .await
        .expect("should connect to the proxy");
    for (respond, code, message) in [
        ("trailers-only", tonic::Code::ResourceExhausted, "too large"),
        ("trailers", tonic::Code::InvalidArgument, "bad data"),
    ] {
        let mut request = tonic::Request::new(ExportMetricsServiceRequest::default());
        request
            .metadata_mut()
            .insert("x-respond", respond.parse().expect("metadata should parse"));
        let status = MetricsServiceClient::new(channel.clone())
            .export(request)
            .await
            .expect_err("export should fail");
        assert_eq!(status.code(), code, "{respond}");
        assert_eq!(status.message(), message, "{respond}");
        assert_eq!(status.details(), details, "{respond}");
    }

    servers.shutdown().await;
}

#[tokio::test]
async fn unreachable_upstream_is_bad_gateway_or_unavailable() {
    let recorder = TestRecorder::default();
    let _recorder_guard = metrics::set_default_local_recorder(&recorder);

    // A bound socket that doesn't listen refuses connections, and keeps its port from other tests.
    let socket = TcpSocket::new_v4().expect("socket should open");
    socket
        .bind("127.0.0.1:0".parse().expect("address should parse"))
        .expect("socket should bind");
    let refused = Destination::Tcp {
        host: "127.0.0.1".to_string(),
        port: socket.local_addr().expect("socket should have an address").port(),
    };
    let servers = start_proxy("proxy-unreachable", &refused, &refused, CONNECT_TIMEOUT).await;

    let http = TestClient::new(HttpProtocol::Http1)
        .send(
            "POST",
            &format!("http://127.0.0.1:{}/v1/traces", servers.http_port),
            &[],
            "payload",
        )
        .await
        .expect("proxy should answer");
    assert_eq!(http.status, StatusCode::BAD_GATEWAY);

    let grpc = TestClient::new(HttpProtocol::Http2)
        .send(
            "POST",
            &format!(
                "http://127.0.0.1:{}/opentelemetry.proto.collector.trace.v1.TraceService/Export",
                servers.grpc_port
            ),
            &[("content-type", "application/grpc"), ("te", "trailers")],
            grpc_frame(&[]),
        )
        .await
        .expect("proxy should answer");
    // A trailers-only response: the status is in the headers.
    assert_eq!(grpc.status, StatusCode::OK);
    assert_eq!(grpc.headers["grpc-status"], "14");
    assert!(grpc.body.is_empty());
    assert_eq!(grpc.trailers, None);

    for protocol in ["http", "grpc"] {
        assert_eq!(
            counter(&recorder, "component_requests_failed_total", "traces", protocol),
            Some(1),
            "{protocol}"
        );
        assert_eq!(
            counter(&recorder, "component_requests_relayed_total", "traces", protocol),
            Some(0),
            "{protocol}"
        );
    }

    servers.shutdown().await;
}

/// How a raw HTTP upstream fails a request after reading it.
#[derive(Clone, Copy, Debug)]
enum RawFailure {
    /// Closes the connection without an answer.
    CloseBeforeResponse,

    /// Sends the headers and part of the body, then closes the connection.
    CloseMidResponse,
}

/// Starts an HTTP/1.1 upstream that fails every request with `failure`, and counts the requests.
async fn spawn_failing_http_upstream(failure: RawFailure) -> (Destination, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("should bind an ephemeral port");
    let port = listener.local_addr().expect("listener should have an address").port();
    let requests = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&requests);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => request.extend_from_slice(&buffer[..read]),
                }
            }
            counted.fetch_add(1, Ordering::SeqCst);
            if let RawFailure::CloseMidResponse = failure {
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\npartial")
                    .await;
            }
        }
    });
    let destination = Destination::Tcp {
        host: "127.0.0.1".to_string(),
        port,
    };
    (destination, requests)
}

#[tokio::test]
async fn http_upstream_failure_is_not_retried() {
    for failure in [RawFailure::CloseBeforeResponse, RawFailure::CloseMidResponse] {
        let (destination, requests) = spawn_failing_http_upstream(failure).await;
        let servers = start_proxy(
            &format!("proxy-http-no-retry-{failure:?}"),
            &destination,
            &destination,
            CONNECT_TIMEOUT,
        )
        .await;

        let result = TestClient::new(HttpProtocol::Http1)
            .send(
                "POST",
                &format!("http://127.0.0.1:{}/v1/metrics", servers.http_port),
                &[],
                "payload",
            )
            .await;
        match failure {
            RawFailure::CloseBeforeResponse => {
                let response = result.expect("proxy should answer");
                assert_eq!(response.status, StatusCode::BAD_GATEWAY);
            }
            RawFailure::CloseMidResponse => {
                assert!(result.is_err(), "the client should see the failed body, got {result:?}");
            }
        }
        // The upstream may have seen the request, so the proxy must not send it again.
        assert_eq!(requests.load(Ordering::SeqCst), 1, "{failure:?}");

        servers.shutdown().await;
    }
}

#[tokio::test]
async fn grpc_upstream_failure_mid_response_is_not_retried() {
    let upstream = FakeUpstream::tcp(|_| {
        // The body fails only after the headers and the first data frame reach the proxy.
        let frames = futures::stream::iter([true, false]).then(|first| async move {
            if first {
                Ok(Frame::data(Bytes::from_static(b"\0\0\0\0\x10part")))
            } else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Err(io::Error::other("upstream failed mid-response"))
            }
        });
        let mut response = http::Response::new(Body::new(StreamBody::new(frames)));
        response
            .headers_mut()
            .insert("content-type", HeaderValue::from_static("application/grpc"));
        response
    })
    .await;
    let servers = start_proxy(
        "proxy-grpc-no-retry",
        &upstream.destination,
        &upstream.destination,
        CONNECT_TIMEOUT,
    )
    .await;

    let result = TestClient::new(HttpProtocol::Http2)
        .send(
            "POST",
            &format!("http://127.0.0.1:{}{METRICS_EXPORT_PATH}", servers.grpc_port),
            &[("content-type", "application/grpc"), ("te", "trailers")],
            grpc_frame(&[]),
        )
        .await;

    assert!(result.is_err(), "the client should see the failed body, got {result:?}");
    assert_eq!(upstream.requests().len(), 1);

    servers.shutdown().await;
}

// The test relies on Linux dropping connection attempts to a listener whose accept queue is full.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn connect_timeout_bounds_an_unanswered_connection_attempt() {
    use std::time::Instant;

    const TEST_CONNECT_TIMEOUT: Duration = Duration::from_millis(200);

    let socket = TcpSocket::new_v4().expect("socket should open");
    socket
        .bind("127.0.0.1:0".parse().expect("address should parse"))
        .expect("socket should bind");
    let listener = socket.listen(0).expect("socket should listen");
    let address = listener.local_addr().expect("listener should have an address");
    // This connection fills the accept queue, so the kernel drops later connection attempts unanswered.
    let _queued = tokio::net::TcpStream::connect(address)
        .await
        .expect("first connection should complete");
    let black_hole = Destination::Tcp {
        host: "127.0.0.1".to_string(),
        port: address.port(),
    };
    let servers = start_proxy("proxy-connect-timeout", &black_hole, &black_hole, TEST_CONNECT_TIMEOUT).await;

    let started = Instant::now();
    let http = TestClient::new(HttpProtocol::Http1)
        .send(
            "POST",
            &format!("http://127.0.0.1:{}/v1/logs", servers.http_port),
            &[],
            "payload",
        )
        .await
        .expect("proxy should answer");
    let http_elapsed = started.elapsed();
    assert_eq!(http.status, StatusCode::BAD_GATEWAY);

    let started = Instant::now();
    let grpc = TestClient::new(HttpProtocol::Http2)
        .send(
            "POST",
            &format!("http://127.0.0.1:{}{METRICS_EXPORT_PATH}", servers.grpc_port),
            &[("content-type", "application/grpc"), ("te", "trailers")],
            grpc_frame(&[]),
        )
        .await
        .expect("proxy should answer");
    let grpc_elapsed = started.elapsed();
    assert_eq!(grpc.headers["grpc-status"], "14");

    for elapsed in [http_elapsed, grpc_elapsed] {
        assert!(
            elapsed >= TEST_CONNECT_TIMEOUT && elapsed < Duration::from_secs(2),
            "the connection attempt should fail at the connect timeout, took {elapsed:?}"
        );
    }

    servers.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn forwards_to_unix_socket_destinations() {
    use crate::common::otlp::test_util::grpc_ok;

    let dir = tempfile::tempdir().expect("temp dir should be created");
    let grpc_upstream = FakeUpstream::unix(&dir.path().join("grpc.sock"), grpc_ok).await;
    let http_upstream = FakeUpstream::unix(&dir.path().join("http.sock"), http_ok).await;
    let servers = start_proxy(
        "proxy-unix",
        &grpc_upstream.destination,
        &http_upstream.destination,
        CONNECT_TIMEOUT,
    )
    .await;

    let channel = tonic::transport::Endpoint::from_shared(format!("http://127.0.0.1:{}", servers.grpc_port))
        .expect("endpoint should parse")
        .connect()
        .await
        .expect("should connect to the proxy");
    MetricsServiceClient::new(channel)
        .export(ExportMetricsServiceRequest::default())
        .await
        .expect("export should succeed");

    let response = TestClient::new(HttpProtocol::Http1)
        .send(
            "POST",
            &format!("http://127.0.0.1:{}/v1/metrics", servers.http_port),
            &[],
            "payload",
        )
        .await
        .expect("proxy should answer");
    assert_eq!(response.status, StatusCode::OK);

    let grpc_requests = grpc_upstream.requests();
    assert_eq!(grpc_requests.len(), 1);
    assert_eq!(grpc_requests[0].path_and_query, METRICS_EXPORT_PATH);
    assert_eq!(grpc_requests[0].version, Version::HTTP_2);
    let http_requests = http_upstream.requests();
    assert_eq!(http_requests.len(), 1);
    assert_eq!(http_requests[0].path_and_query, "/v1/metrics");
    assert_eq!(http_requests[0].body, "payload");

    servers.shutdown().await;
}

#[test]
fn accepts_trailers_finds_the_trailers_coding() {
    assert!(accepts_trailers(&header_map(&[("te", "trailers")])));
    assert!(accepts_trailers(&header_map(&[("te", "gzip, Trailers")])));
    assert!(!accepts_trailers(&header_map(&[("te", "gzip")])));
    assert!(!accepts_trailers(&HeaderMap::new()));
}

/// How long a streaming test waits for a chunk that the proxy should pass on at once.
const CHUNK_WAIT: Duration = Duration::from_secs(5);

/// Serves `router` on an ephemeral loopback port, over HTTP/1.1 and HTTP/2 with prior knowledge.
async fn serve(router: Router) -> Destination {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("should bind an ephemeral port");
    let port = listener.local_addr().expect("listener should have an address").port();
    tokio::spawn(async move { axum::serve(listener, router).await });
    Destination::Tcp {
        host: "127.0.0.1".to_string(),
        port,
    }
}

/// A request body that sends each chunk as soon as the test hands it over.
struct ChannelBody(mpsc::Receiver<Bytes>);

impl http_body::Body for ChannelBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.0
            .poll_recv(cx)
            .map(|chunk| chunk.map(|chunk| Ok(Frame::data(chunk))))
    }
}

/// Returns the request that streaming tests send for `protocol`, with `body`.
fn streaming_request<B>(protocol: Protocol, servers: &TestServers, body: B) -> http::Request<B> {
    let (port, path, content_type) = match protocol {
        Protocol::Grpc => (servers.grpc_port, METRICS_EXPORT_PATH, "application/grpc"),
        Protocol::Http => (servers.http_port, "/v1/metrics", "application/x-protobuf"),
    };
    http::Request::builder()
        .method("POST")
        .uri(format!("http://127.0.0.1:{port}{path}"))
        .header("content-type", content_type)
        .header("te", "trailers")
        .body(body)
        .expect("request should build")
}

fn streaming_client(protocol: Protocol) -> TestClient {
    TestClient::new(match protocol {
        Protocol::Grpc => HttpProtocol::Http2,
        Protocol::Http => HttpProtocol::Http1,
    })
}

#[tokio::test]
async fn request_body_streams_to_the_upstream() {
    // A proxy that buffered the request would hold the first chunk until the client sent the rest.
    for protocol in [Protocol::Http, Protocol::Grpc] {
        let (seen_tx, mut seen_rx) = mpsc::unbounded_channel::<Bytes>();
        let upstream = serve(Router::new().fallback(move |request: Request| {
            let seen_tx = seen_tx.clone();
            async move {
                let mut body = request.into_body();
                while let Some(frame) = body.frame().await {
                    if let Ok(data) = frame.expect("request body should be readable").into_data() {
                        let _ = seen_tx.send(data);
                    }
                }
                Response::new(Body::empty())
            }
        }))
        .await;
        let servers = start_proxy(
            &format!("proxy-streams-request-{}", protocol.as_str()),
            &upstream,
            &upstream,
            CONNECT_TIMEOUT,
        )
        .await;

        let (chunks_tx, chunks_rx) = mpsc::channel(2);
        let mut client = streaming_client(protocol);
        let request = streaming_request(protocol, &servers, ChannelBody(chunks_rx));
        let response = tokio::spawn(async move { client.send_request(request).await });

        timeout(CHUNK_WAIT, chunks_tx.send(Bytes::from_static(b"first")))
            .await
            .expect("body should accept a chunk in time")
            .expect("body should accept a chunk");
        let mut seen = Vec::new();
        while seen.len() < b"first".len() {
            let chunk = timeout(CHUNK_WAIT, seen_rx.recv())
                .await
                .unwrap_or_else(|_| panic!("{protocol:?}: the upstream should see the first chunk before the rest"))
                .expect("upstream should report chunks");
            seen.extend_from_slice(&chunk);
        }
        assert_eq!(seen, b"first", "{protocol:?}");

        timeout(CHUNK_WAIT, chunks_tx.send(Bytes::from_static(b"second")))
            .await
            .expect("body should accept a chunk in time")
            .expect("body should accept a chunk");
        drop(chunks_tx);
        let response = timeout(CHUNK_WAIT, response)
            .await
            .unwrap_or_else(|_| panic!("{protocol:?}: the proxy should answer"))
            .expect("client task should not panic")
            .expect("proxy should answer");
        assert_eq!(response.status(), StatusCode::OK, "{protocol:?}");
        // The upstream reads the whole body before it answers, so every chunk is already reported.
        while let Ok(chunk) = seen_rx.try_recv() {
            seen.extend_from_slice(&chunk);
        }
        assert_eq!(seen, b"firstsecond", "{protocol:?}");

        servers.shutdown().await;
    }
}

#[tokio::test]
async fn response_body_streams_to_the_client() {
    // A proxy that buffered the response would hold the first chunk until the upstream sent the rest.
    for protocol in [Protocol::Http, Protocol::Grpc] {
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let release_rx = Arc::new(Mutex::new(Some(release_rx)));
        let upstream = serve(Router::new().fallback(move |_: Request| {
            let release_rx = release_rx
                .lock()
                .expect("lock should not be poisoned")
                .take()
                .expect("the upstream should get one request");
            async move {
                // gRPC ends with trailers, and HTTP with a second data frame.
                let last = match protocol {
                    Protocol::Grpc => Frame::trailers(header_map(&[("grpc-status", "0")])),
                    Protocol::Http => Frame::data(Bytes::from_static(b"second")),
                };
                let frames =
                    futures::stream::once(async { Ok::<_, Infallible>(Frame::data(Bytes::from_static(b"first"))) })
                        .chain(futures::stream::once(async move {
                            let _ = release_rx.await;
                            Ok(last)
                        }));
                Response::new(Body::new(StreamBody::new(frames)))
            }
        }))
        .await;
        let servers = start_proxy(
            &format!("proxy-streams-response-{}", protocol.as_str()),
            &upstream,
            &upstream,
            CONNECT_TIMEOUT,
        )
        .await;

        let mut client = streaming_client(protocol);
        let request = streaming_request(protocol, &servers, Full::new(Bytes::new()));
        let response = timeout(CHUNK_WAIT, client.send_request(request))
            .await
            .unwrap_or_else(|_| panic!("{protocol:?}: the response headers should arrive before the rest"))
            .expect("proxy should answer");
        assert_eq!(response.status(), StatusCode::OK, "{protocol:?}");
        let mut body = response.into_body();

        let mut first = Vec::new();
        while first.len() < b"first".len() {
            let frame = timeout(CHUNK_WAIT, body.frame())
                .await
                .unwrap_or_else(|_| panic!("{protocol:?}: the client should see the first chunk before the rest"))
                .expect("body should not end yet")
                .expect("body should be readable");
            first.extend_from_slice(&frame.into_data().expect("frame should be data"));
        }
        assert_eq!(first, b"first", "{protocol:?}");

        release_tx.send(()).expect("upstream should wait for the release");
        let rest = timeout(CHUNK_WAIT, body.collect())
            .await
            .unwrap_or_else(|_| panic!("{protocol:?}: the rest of the body should arrive after the release"))
            .expect("body should be readable");
        match protocol {
            Protocol::Grpc => {
                assert_eq!(
                    rest.trailers().map(|t| &t["grpc-status"]),
                    Some(&HeaderValue::from_static("0"))
                );
            }
            Protocol::Http => assert_eq!(rest.to_bytes(), "second"),
        }

        servers.shutdown().await;
    }
}

#[tokio::test]
async fn http_request_failed_on_a_reused_connection_is_not_retried() {
    // The upstream answers the first request, then closes the same connection after it reads the second. The second
    // request may have reached the upstream, so the proxy must not resend it. A resent request would get 200.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("should bind an ephemeral port");
    let port = listener.local_addr().expect("listener should have an address").port();
    let requests = Arc::new(AtomicUsize::new(0));
    let connections = Arc::new(AtomicUsize::new(0));
    tokio::spawn({
        let requests = Arc::clone(&requests);
        let connections = Arc::clone(&connections);
        async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                connections.fetch_add(1, Ordering::SeqCst);
                let requests = Arc::clone(&requests);
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut read = [0; 1024];
                    loop {
                        // The requests have no body, so a request ends with its headers.
                        while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                            match stream.read(&mut read).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buffer.extend_from_slice(&read[..n]),
                            }
                        }
                        let end = buffer
                            .windows(4)
                            .position(|window| window == b"\r\n\r\n")
                            .expect("headers should end")
                            + 4;
                        buffer.drain(..end);
                        if requests.fetch_add(1, Ordering::SeqCst) == 1 {
                            return;
                        }
                        let _ = stream
                            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                            .await;
                    }
                });
            }
        }
    });
    let destination = Destination::Tcp {
        host: "127.0.0.1".to_string(),
        port,
    };
    let servers = start_proxy(
        "proxy-http-no-retry-reused",
        &destination,
        &destination,
        CONNECT_TIMEOUT,
    )
    .await;

    let mut client = TestClient::new(HttpProtocol::Http1);
    let uri = format!("http://127.0.0.1:{}/v1/metrics", servers.http_port);
    let first = timeout(CHUNK_WAIT, client.send("POST", &uri, &[], Bytes::new()))
        .await
        .expect("proxy should answer in time")
        .expect("proxy should answer");
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.body, "ok");

    let second = timeout(CHUNK_WAIT, client.send("POST", &uri, &[], Bytes::new()))
        .await
        .expect("proxy should answer in time")
        .expect("proxy should answer");
    assert_eq!(second.status, StatusCode::BAD_GATEWAY);
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "the second request should go out on the reused connection"
    );

    servers.shutdown().await;
}
