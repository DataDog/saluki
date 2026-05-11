use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::LazyLock;

use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, post},
    Router,
};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{
        sources::{Source, SourceBuilder, SourceContext},
        ComponentContext,
    },
    data_model::event::{
        trace::{
            EventAttributeScalarValue, EventAttributeValue, Span, SpanEvent, SpanLink, Trace, TraceSampling,
        },
        Event, EventType,
    },
    topology::OutputDefinition,
};

mod v1_types;
use self::v1_types::{V1AnyValue, V1KeyValue, V1Span, V1SpanEvent, V1SpanLink, V1Trace, V1TraceChunk};
use saluki_common::collections::FastHashMap;
use saluki_error::{generic_error, GenericError};
use stringtheory::{interning::GenericMapInterner, MetaString};
use tokio::{net::TcpListener, sync::mpsc};
use tracing::{debug, error, info, warn};

pub mod sampling_rates;
use self::sampling_rates::{RateResponse, V1SamplingRatesHandle};

mod deserialize;
use self::deserialize::{
    decode_tracer_payload, DeserializeError, RawAnyValue, RawKeyValue, RawSpan, RawSpanEvent, RawSpanLink,
    RawTraceChunk, RawTracerPayload,
};

const DEFAULT_LISTEN_ADDRESS: &str = "0.0.0.0:8126";

/// Header sent by tracers reporting how many P0 (AutoDrop) traces were dropped client-side.
const HEADER_CLIENT_DROPPED_P0: &str = "Datadog-Client-Dropped-P0-Traces";
/// Header used by tracers to report (and the agent to set) the current rates payload version.
const HEADER_RATES_VERSION: &str = "Datadog-Rates-Payload-Version";

/// Sentinel value used by the V1 wire format to indicate no priority was set.
/// Matches Go's `PriorityNone = math.MinInt8`.
const V1_PRIORITY_NONE: i32 = i8::MIN as i32;

/// Configuration for the APM receiver source.
pub struct ApmReceiverConfiguration {
    listen_address: SocketAddr,
    sampling_rates: V1SamplingRatesHandle,
}

impl ApmReceiverConfiguration {
    /// Creates a new `ApmReceiverConfiguration` from the given configuration.
    ///
    /// Reads `data_plane.apm.listen_address` (default: `0.0.0.0:8126`).
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let addr_str = config
            .try_get_typed::<String>("data_plane.apm.listen_address")?
            .unwrap_or_else(|| DEFAULT_LISTEN_ADDRESS.to_owned());

        let listen_address = addr_str
            .parse::<SocketAddr>()
            .map_err(|e| generic_error!("Invalid APM listen address '{}': {}", addr_str, e))?;

        Ok(Self {
            listen_address,
            sampling_rates: V1SamplingRatesHandle::new(),
        })
    }

    /// Attaches a shared [`V1SamplingRatesHandle`] so the receiver can include current
    /// per-service sampling rates in every HTTP response.
    pub fn with_sampling_rates(mut self, handle: V1SamplingRatesHandle) -> Self {
        self.sampling_rates = handle;
        self
    }
}

impl Default for ApmReceiverConfiguration {
    fn default() -> Self {
        Self {
            listen_address: DEFAULT_LISTEN_ADDRESS.parse().expect("default listen address is valid"),
            sampling_rates: V1SamplingRatesHandle::new(),
        }
    }
}

#[async_trait]
impl SourceBuilder for ApmReceiverConfiguration {
    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition<EventType>>> =
            LazyLock::new(|| vec![OutputDefinition::named_output("traces", EventType::Trace)]);
        &OUTPUTS
    }

    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(ApmReceiver {
            listen_address: self.listen_address,
            sampling_rates: self.sampling_rates.clone(),
        }))
    }
}

impl MemoryBounds for ApmReceiverConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<ApmReceiver>("component struct");
    }
}

struct ApmReceiver {
    listen_address: SocketAddr,
    sampling_rates: V1SamplingRatesHandle,
}

/// Shared state for the axum request handler.
#[derive(Clone)]
struct HandlerState {
    tx: mpsc::Sender<Vec<Trace>>,
    sampling_rates: V1SamplingRatesHandle,
}

async fn handle_info() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(axum::body::Body::from(r#"{"endpoints":["/v1.0/traces"]}"#))
        .unwrap()
}

async fn handle_v1_traces(
    State(state): State<HandlerState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Read the client-dropped-P0 count for rate-computation weight adjustment.
    let client_dropped_p0s = headers
        .get(HEADER_CLIENT_DROPPED_P0)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);

    // Read the tracer's current rates version for idempotent response optimization.
    let client_version = headers
        .get(HEADER_RATES_VERSION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();

    match decode_tracer_payload(&mut body.as_ref()) {
        Ok(raw) => {
            let chunk_count = raw.chunks.len().max(1);
            let total_spans: usize = raw.chunks.iter().map(|c| c.spans.len()).sum();
            debug!(
                chunks = raw.chunks.len(),
                spans = total_spans,
                client_dropped_p0s,
                "Received V1 tracer payload."
            );
            let per_chunk_weight = client_dropped_p0s as f64 / chunk_count as f64;
            let traces = resolve_payload(raw, per_chunk_weight);
            if !traces.is_empty() {
                debug!(traces = traces.len(), "Dispatching trace events to topology.");
                if let Err(e) = state.tx.try_send(traces) {
                    warn!(error = %e, "APM receiver channel full; dropping payload.");
                }
            }

            let client_sent_version = !client_version.is_empty();
            let rate_response = state.sampling_rates.get_response(&client_version);
            build_rate_response(rate_response, client_sent_version)
        }
        Err(DeserializeError::UnexpectedEof) | Err(DeserializeError::UnexpectedMarker(_)) => {
            warn!("Malformed v1 trace payload (parse error).");
            Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(axum::body::Body::empty())
                .unwrap()
        }
        Err(e) => {
            warn!(error = ?e, "Failed to deserialize v1 trace payload.");
            Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(axum::body::Body::empty())
                .unwrap()
        }
    }
}

fn build_rate_response(response: RateResponse, client_sent_version: bool) -> Response {
    let (body_bytes, version) = match response {
        RateResponse::Unchanged { version } => (b"{}".to_vec(), version),
        RateResponse::Updated { rates, version } => {
            let json = serde_json::to_vec(&serde_json::json!({ "rate_by_service": rates }))
                .unwrap_or_else(|_| b"{}".to_vec());
            (json, version)
        }
    };

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json");

    if client_sent_version && !version.is_empty() {
        builder = builder.header(HEADER_RATES_VERSION, version.as_str());
    }

    builder
        .body(axum::body::Body::from(body_bytes))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(axum::body::Body::empty())
                .unwrap()
        })
}

#[async_trait]
impl Source for ApmReceiver {
    async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let mut shutdown = context.take_shutdown_handle();
        let mut health = context.take_health_handle();

        let (tx, mut rx) = mpsc::channel::<Vec<Trace>>(256);

        let listener = TcpListener::bind(self.listen_address)
            .await
            .map_err(|e| generic_error!("Failed to bind APM receiver on {}: {}", self.listen_address, e))?;

        let app = Router::new()
            .route("/info", get(handle_info))
            .route("/v1.0/traces", post(handle_v1_traces))
            .with_state(HandlerState {
                tx,
                sampling_rates: self.sampling_rates,
            });

        let (server_shutdown_tx, server_shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
                let _ = server_shutdown_rx.await;
            });
            if let Err(e) = serve.await {
                error!(error = %e, "APM HTTP server error.");
            }
        });

        health.mark_ready();
        info!("APM receiver source started on {}.", self.listen_address);

        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    debug!("APM receiver source shutting down.");
                    let _ = server_shutdown_tx.send(());
                    break;
                }
                Some(traces) = rx.recv() => {
                    let dispatcher = context
                        .dispatcher()
                        .buffered_named("traces")
                        .map_err(|e| generic_error!("Failed to get traces dispatcher: {}", e))?;
                    if let Err(e) = dispatcher.send_all(traces.into_iter().map(Event::Trace)).await {
                        error!(error = %e, "Failed to dispatch trace events.");
                    }
                }
                _ = health.live() => continue,
            }
        }

        debug!("APM receiver source stopped.");
        Ok(())
    }
}

// ── Resolution pass: RawTracerPayload → Vec<V1Trace> (internal) ────────────

fn resolve_payload(raw: RawTracerPayload, per_chunk_weight: f64) -> Vec<Trace> {
    let capacity_bytes = raw.string_table.len().saturating_mul(64).saturating_add(1024);
    let capacity = NonZeroUsize::new(capacity_bytes).unwrap_or(NonZeroUsize::MIN);
    let interner = GenericMapInterner::new(capacity);

    let resolved: Vec<MetaString> = raw
        .string_table
        .iter()
        .map(|s| MetaString::from_interner(s, &interner))
        .collect();

    let r = |idx: u32| -> MetaString { resolved.get(idx as usize).cloned().unwrap_or_default() };

    let payload_attributes = resolve_kvs(raw.attributes, &r);
    let container_id = r(raw.container_id);
    let language_name = r(raw.language_name);
    let language_version = r(raw.language_version);
    let tracer_version = r(raw.tracer_version);
    let runtime_id = r(raw.runtime_id);
    let env = r(raw.env);
    let hostname = r(raw.hostname);
    let app_version = r(raw.app_version);

    raw.chunks
        .into_iter()
        .map(|raw_chunk| {
            let v1 = V1Trace {
                chunk: resolve_chunk(raw_chunk, &r),
                container_id: container_id.clone(),
                language_name: language_name.clone(),
                language_version: language_version.clone(),
                tracer_version: tracer_version.clone(),
                runtime_id: runtime_id.clone(),
                env: env.clone(),
                hostname: hostname.clone(),
                app_version: app_version.clone(),
                payload_attributes: payload_attributes.clone(),
                client_dropped_p0s_weight: per_chunk_weight,
            };
            v1_trace_to_trace(v1)
        })
        .collect()
}

fn resolve_chunk(raw: RawTraceChunk, r: &impl Fn(u32) -> MetaString) -> V1TraceChunk {
    V1TraceChunk {
        priority: raw.priority,
        origin: r(raw.origin),
        attributes: resolve_kvs(raw.attributes, r),
        spans: raw.spans.into_iter().map(|s| resolve_span(s, r)).collect(),
        dropped_trace: raw.dropped_trace,
        trace_id_high: raw.trace_id_high,
        trace_id_low: raw.trace_id_low,
        sampling_mechanism: raw.sampling_mechanism,
    }
}

fn resolve_span(raw: RawSpan, r: &impl Fn(u32) -> MetaString) -> V1Span {
    V1Span {
        service: r(raw.service),
        name: r(raw.name),
        resource: r(raw.resource),
        span_id: raw.span_id,
        parent_id: raw.parent_id,
        start: raw.start,
        duration: raw.duration,
        error: raw.error,
        attributes: resolve_kvs(raw.attributes, r),
        span_type: r(raw.span_type),
        links: raw.links.into_iter().map(|l| resolve_link(l, r)).collect(),
        events: raw.events.into_iter().map(|e| resolve_event(e, r)).collect(),
        env: r(raw.env),
        version: r(raw.version),
        component: r(raw.component),
        kind: raw.kind,
    }
}

fn resolve_link(raw: RawSpanLink, r: &impl Fn(u32) -> MetaString) -> V1SpanLink {
    V1SpanLink {
        trace_id_high: raw.trace_id_high,
        trace_id_low: raw.trace_id_low,
        span_id: raw.span_id,
        attributes: resolve_kvs(raw.attributes, r),
        tracestate: r(raw.tracestate),
        flags: raw.flags,
    }
}

fn resolve_event(raw: RawSpanEvent, r: &impl Fn(u32) -> MetaString) -> V1SpanEvent {
    V1SpanEvent {
        time_unix_nano: raw.time_unix_nano,
        name: r(raw.name),
        attributes: resolve_kvs(raw.attributes, r),
    }
}

fn resolve_kvs(raw: Vec<RawKeyValue>, r: &impl Fn(u32) -> MetaString) -> Vec<V1KeyValue> {
    raw.into_iter()
        .map(|kv| V1KeyValue {
            key: r(kv.key),
            value: resolve_any_value(kv.value, r),
        })
        .collect()
}

fn resolve_any_value(raw: RawAnyValue, r: &impl Fn(u32) -> MetaString) -> V1AnyValue {
    match raw {
        RawAnyValue::String(idx) => V1AnyValue::String(r(idx)),
        RawAnyValue::Bool(v) => V1AnyValue::Bool(v),
        RawAnyValue::Double(v) => V1AnyValue::Double(v),
        RawAnyValue::Int(v) => V1AnyValue::Int(v),
        RawAnyValue::Bytes(v) => V1AnyValue::Bytes(v),
        RawAnyValue::Array(items) => {
            V1AnyValue::Array(items.into_iter().map(|item| resolve_any_value(item, r)).collect())
        }
        RawAnyValue::KeyValueList(kvs) => V1AnyValue::KeyValueList(resolve_kvs(kvs, r)),
    }
}

// ── V1Trace → unified Trace conversion ────────────────────────────────────────

/// Convert a resolved `V1Trace` into the unified `Trace` event type.
///
/// The V1 types are wire-format intermediates produced by the APM source's deserialization pass.
/// After this conversion they are no longer referenced; all downstream pipeline components work
/// with the unified `Trace` and `Span` types.
fn v1_trace_to_trace(v1: V1Trace) -> Trace {
    // `V1_PRIORITY_NONE` (i8::MIN) sentinel → None; any other value → Some(value).
    let priority = if v1.chunk.priority == V1_PRIORITY_NONE {
        None
    } else {
        Some(v1.chunk.priority)
    };

    // Convert chunk-level attributes (process tags, etc.) and merge payload-level attributes.
    let mut attributes = v1_kvs_to_attribute_map(v1.chunk.attributes);
    for kv in v1.payload_attributes {
        if let Some(av) = v1_anyvalue_to_attribute_value(kv.value) {
            attributes.insert(kv.key, av);
        }
    }

    let spans = v1.chunk.spans.into_iter().map(v1_span_to_span).collect();

    let mut trace = Trace::new(spans);

    // Unified trace-level fields.
    trace.trace_id_high = v1.chunk.trace_id_high;
    trace.trace_id_low = v1.chunk.trace_id_low;
    trace.origin = v1.chunk.origin;
    trace.priority = priority;
    trace.dropped_trace = v1.chunk.dropped_trace;
    trace.sampling_mechanism = v1.chunk.sampling_mechanism;
    trace.container_id = v1.container_id;
    trace.language_name = v1.language_name;
    trace.language_version = v1.language_version;
    trace.tracer_version = v1.tracer_version;
    trace.runtime_id = v1.runtime_id;
    trace.env = v1.env;
    trace.hostname = v1.hostname;
    trace.app_version = v1.app_version;
    trace.client_dropped_p0s_weight = v1.client_dropped_p0s_weight;
    trace.attributes = attributes;

    // Populate legacy sampling for compat with transforms that still read `trace.sampling()`.
    if let Some(p) = priority {
        trace.set_sampling(Some(TraceSampling::new(false, Some(p), None, None)));
    }

    trace
}

fn v1_span_to_span(v1: V1Span) -> Span {
    let span_links = v1.links.into_iter().map(v1_span_link_to_span_link).collect();
    let span_events = v1.events.into_iter().map(v1_span_event_to_span_event).collect();

    let mut span = Span::new(
        v1.service,
        v1.name,
        v1.resource,
        v1.span_type,
        0, // trace_id is now on Trace; leave 0 on Span
        v1.span_id,
        v1.parent_id,
        v1.start,
        v1.duration,
        if v1.error { 1 } else { 0 },
    )
    .with_span_links(Some(span_links))
    .with_span_events(Some(span_events))
    .with_env(v1.env)
    .with_version(v1.version)
    .with_component(v1.component)
    .with_kind(v1.kind);

    for kv in v1.attributes {
        if let Some(av) = v1_anyvalue_to_attribute_value(kv.value) {
            span.attributes.insert(kv.key, av);
        }
    }

    span
}

fn v1_span_link_to_span_link(v1: V1SpanLink) -> SpanLink {
    // SpanLink.attributes is FastHashMap<MetaString, MetaString>: keep only string-valued entries.
    let attrs = v1
        .attributes
        .into_iter()
        .filter_map(|kv| {
            if let V1AnyValue::String(s) = kv.value {
                Some((kv.key, s))
            } else {
                None
            }
        })
        .collect();

    SpanLink::new(v1.trace_id_low, v1.span_id)
        .with_trace_id_high(v1.trace_id_high)
        .with_attributes(Some(attrs))
        .with_tracestate(v1.tracestate)
        .with_flags(v1.flags)
}

fn v1_span_event_to_span_event(v1: V1SpanEvent) -> SpanEvent {
    let attrs = v1
        .attributes
        .into_iter()
        .filter_map(|kv| {
            let ev = v1_anyvalue_to_event_attribute_value(kv.value)?;
            Some((kv.key, ev))
        })
        .collect();

    SpanEvent::new(v1.time_unix_nano, v1.name).with_attributes(Some(attrs))
}

/// Convert a `Vec<V1KeyValue>` into `Trace.attributes` (typed attribute map).
fn v1_kvs_to_attribute_map(
    kvs: Vec<V1KeyValue>,
) -> FastHashMap<MetaString, saluki_core::data_model::event::trace::AttributeValue> {
    let mut map = FastHashMap::default();
    for kv in kvs {
        if let Some(av) = v1_anyvalue_to_attribute_value(kv.value) {
            map.insert(kv.key, av);
        }
    }
    map
}

fn v1_anyvalue_to_attribute_value(
    v: V1AnyValue,
) -> Option<saluki_core::data_model::event::trace::AttributeValue> {
    use saluki_core::data_model::event::trace::AttributeValue;
    match v {
        V1AnyValue::String(s) => Some(AttributeValue::String(s)),
        V1AnyValue::Bool(b) => {
            Some(AttributeValue::String(MetaString::from_static(if b { "true" } else { "false" })))
        }
        V1AnyValue::Double(d) => Some(AttributeValue::Float(d)),
        V1AnyValue::Int(i) => Some(AttributeValue::Float(i as f64)),
        V1AnyValue::Bytes(b) => Some(AttributeValue::Bytes(b)),
        V1AnyValue::Array(_) | V1AnyValue::KeyValueList(_) => None,
    }
}

fn v1_anyvalue_to_event_attribute_value(v: V1AnyValue) -> Option<EventAttributeValue> {
    match v {
        V1AnyValue::String(s) => Some(EventAttributeValue::String(s)),
        V1AnyValue::Bool(b) => Some(EventAttributeValue::Bool(b)),
        V1AnyValue::Int(i) => Some(EventAttributeValue::Int(i)),
        V1AnyValue::Double(d) => Some(EventAttributeValue::Double(d)),
        V1AnyValue::Bytes(_) => None, // no Bytes variant in EventAttributeValue
        V1AnyValue::Array(items) => {
            let scalars = items
                .into_iter()
                .filter_map(|item| match item {
                    V1AnyValue::String(s) => Some(EventAttributeScalarValue::String(s)),
                    V1AnyValue::Bool(b) => Some(EventAttributeScalarValue::Bool(b)),
                    V1AnyValue::Int(i) => Some(EventAttributeScalarValue::Int(i)),
                    V1AnyValue::Double(d) => Some(EventAttributeScalarValue::Double(d)),
                    _ => None,
                })
                .collect();
            Some(EventAttributeValue::Array(scalars))
        }
        V1AnyValue::KeyValueList(_) => None, // no KVList in EventAttributeValue
    }
}
