//! Recording intakes: loopback HTTP listeners that stand in, in tests, for the endpoints the delivery hosts and the
//! native forwarders send to.
//!
//! An intake records every request it answers: its path, the API key it carries, its body once its `zstd` content
//! encoding is undone (the tests' configurations set `serializer_compressor_kind: zstd`, so every payload is compressed
//! so), and, for a series or sketches payload, the points of each context it holds. It answers `GET /api/v1/validate`
//! with `200 OK` and every other request with `202 Accepted`; a paused intake answers every request with `503 Service
//! Unavailable`, unrecorded, as an intake that is down does, which the forwarders retry.

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    Router,
};
use datadog_protos::metrics::{v3::Payload as V3Payload, MetricPayload, SketchPayload};
use protobuf::Message as _;
use stele::Metric;
use tokio::{net::TcpListener, task::JoinHandle};

/// The configuration layer every test that runs recording intakes adds: every payload compressed with `zstd`, which
/// the intakes undo.
pub(super) const ZSTD: &str = "serializer_compressor_kind: zstd";

/// How long a test waits for intakes to receive what it expects.
pub(super) const PATIENCE: Duration = Duration::from_secs(30);

/// A context: a metric's name and its sorted tags.
pub(super) type Context = (String, Vec<String>);

/// One request an intake answered.
#[derive(Clone, Debug)]
pub(super) struct Request {
    /// The request's path.
    pub path: String,

    /// The API key the request carries, in its `DD-API-KEY` header or its `api_key` query parameter.
    pub api_key: Option<String>,

    /// The body, decompressed.
    pub body: Vec<u8>,

    /// The points of each context of a metrics payload: none for any other request.
    pub points: Vec<(Context, usize)>,
}

#[derive(Clone, Default)]
struct IntakeState {
    requests: Arc<Mutex<Vec<Request>>>,
    paused: Arc<AtomicBool>,
    unavailable: Arc<Mutex<BTreeMap<String, u64>>>,
}

/// A recording intake, listening until it is dropped.
pub(super) struct Intake {
    address: SocketAddr,
    state: IntakeState,
    server: JoinHandle<()>,
}

impl Intake {
    /// Starts an intake on a loopback port of its own.
    pub(super) async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a loopback port is free");
        let address = listener.local_addr().expect("the listener has an address");
        let state = IntakeState::default();
        let router = Router::new().fallback(handle).with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.expect("the intake serves");
        });
        Self { address, state, server }
    }

    /// Returns the intake's base URL.
    pub(super) fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Returns every request the intake recorded so far, in the order it answered them.
    pub(super) fn requests(&self) -> Vec<Request> {
        self.state.requests.lock().unwrap().clone()
    }

    /// Returns the points of each context the intake received so far, over every metrics payload.
    pub(super) fn points(&self) -> BTreeMap<Context, usize> {
        let mut points = BTreeMap::new();
        for request in self.state.requests.lock().unwrap().iter() {
            for (context, count) in &request.points {
                *points.entry(context.clone()).or_default() += count;
            }
        }
        points
    }

    /// Returns the points of each context the intake received so far in the metrics payloads sent to `path`.
    pub(super) fn points_at(&self, path: &str) -> BTreeMap<Context, usize> {
        let mut points = BTreeMap::new();
        for request in self
            .state
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.path == path)
        {
            for (context, count) in &request.points {
                *points.entry(context.clone()).or_default() += count;
            }
        }
        points
    }

    /// Returns how many times `marker` appears in the bodies of the requests to `path` the intake received so far.
    pub(super) fn occurrences(&self, path: &str, marker: &str) -> usize {
        self.state
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.path == path)
            .map(|request| {
                request
                    .body
                    .windows(marker.len())
                    .filter(|window| *window == marker.as_bytes())
                    .count()
            })
            .sum()
    }

    /// Pauses the intake, which then answers every request with `503 Service Unavailable`, or resumes it.
    pub(super) fn set_paused(&self, paused: bool) {
        self.state.paused.store(paused, Ordering::SeqCst);
    }

    /// Returns how many requests to `path` the intake answered `503 Service Unavailable` while paused.
    pub(super) fn unavailable(&self, path: &str) -> u64 {
        self.state
            .unavailable
            .lock()
            .unwrap()
            .get(path)
            .copied()
            .unwrap_or_default()
    }

    /// Waits until `done` holds of the intake, polling, for at most [`PATIENCE`].
    ///
    /// # Panics
    ///
    /// Panics, naming `what`, if it does not hold in time.
    pub(super) async fn wait_for(&self, what: &str, done: impl Fn(&Self) -> bool) {
        let waited = tokio::time::timeout(PATIENCE, async {
            while !done(self) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(
            waited.is_ok(),
            "{} never held at {}: {} requests, points {:?}",
            what,
            self.url(),
            self.requests().len(),
            self.points()
        );
    }
}

impl Drop for Intake {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn handle(
    State(state): State<IntakeState>, method: Method, uri: Uri, headers: HeaderMap, body: Bytes,
) -> StatusCode {
    if state.paused.load(Ordering::SeqCst) {
        *state
            .unavailable
            .lock()
            .unwrap()
            .entry(uri.path().to_string())
            .or_default() += 1;
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    let path = uri.path().to_string();
    let api_key = headers
        .get("dd-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .or_else(|| {
            uri.query()?
                .split('&')
                .find_map(|pair| pair.strip_prefix("api_key="))
                .map(str::to_string)
        });
    let encoding = headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let body = match encoding {
        "zstd" => match zstd::stream::decode_all(body.as_ref()) {
            Ok(body) => body,
            Err(_) => return StatusCode::BAD_REQUEST,
        },
        "" | "identity" => body.to_vec(),
        _ => return StatusCode::UNSUPPORTED_MEDIA_TYPE,
    };
    let points = match decode(&path, &body) {
        Ok(points) => points,
        Err(()) => return StatusCode::BAD_REQUEST,
    };
    let status = if method == Method::GET && path == "/api/v1/validate" {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    state.requests.lock().unwrap().push(Request {
        path,
        api_key,
        body,
        points,
    });
    status
}

/// Returns the points of each context of a metrics payload (series V1, V2, or V3, or sketches V2 or V3), its host tag
/// left out, or nothing for any other request; the forwarders probe the series endpoints with `{}`, which holds no
/// point.
fn decode(path: &str, body: &[u8]) -> Result<Vec<(Context, usize)>, ()> {
    if body == b"{}" {
        return Ok(Vec::new());
    }
    let metrics = match path {
        "/api/v1/series" => Metric::try_from_series_v1(body).map_err(|_| ())?,
        "/api/v2/series" => {
            Metric::try_from_series_v2(MetricPayload::parse_from_bytes(body).map_err(|_| ())?).map_err(|_| ())?
        }
        "/api/beta/sketches" => {
            Metric::try_from_sketch(SketchPayload::parse_from_bytes(body).map_err(|_| ())?).map_err(|_| ())?
        }
        "/api/intake/metrics/v3/series" | "/api/intake/metrics/v3beta/series" | "/api/intake/metrics/v3/sketches" => {
            Metric::try_from_v3(V3Payload::parse_from_bytes(body).map_err(|_| ())?).map_err(|_| ())?
        }
        _ => return Ok(Vec::new()),
    };
    Ok(metrics
        .into_iter()
        .map(|metric| {
            let points = metric.values().len();
            let (name, tags) = metric.context().clone().into_parts();
            let mut tags = tags
                .into_iter()
                .filter(|tag| !tag.starts_with("host:"))
                .collect::<Vec<_>>();
            tags.sort();
            ((name, tags), points)
        })
        .collect())
}
