use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::LazyLock;

use async_trait::async_trait;
use axum::{body::Bytes, extract::State, http::StatusCode, routing::post, Router};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{
        sources::{Source, SourceBuilder, SourceContext},
        ComponentContext,
    },
    data_model::event::{
        trace::v1::{V1AnyValue, V1KeyValue, V1Span, V1SpanEvent, V1SpanLink, V1Trace, V1TraceChunk},
        Event, EventType,
    },
    topology::OutputDefinition,
};
use saluki_error::{generic_error, GenericError};
use stringtheory::{interning::GenericMapInterner, MetaString};
use tokio::{net::TcpListener, sync::mpsc};
use tracing::{debug, error, warn};

mod deserialize;
use self::deserialize::{
    decode_tracer_payload, DeserializeError, RawAnyValue, RawKeyValue, RawSpan, RawSpanEvent, RawSpanLink,
    RawTraceChunk, RawTracerPayload,
};

const DEFAULT_LISTEN_ADDRESS: &str = "0.0.0.0:8126";

/// Configuration for the APM receiver source.
pub struct ApmReceiverConfiguration {
    listen_address: SocketAddr,
}

impl ApmReceiverConfiguration {
    /// Creates a new `ApmReceiverConfiguration` from the given configuration.
    ///
    /// Reads `data_plane.apm.listen_address` (default: `0.0.0.0:8126`).
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let addr_str = config
            .try_get_typed::<String>("data_plane.apm.listen_address")?
            .unwrap_or_else(|| DEFAULT_LISTEN_ADDRESS.to_owned());

        let listen_address = addr_str.parse::<SocketAddr>().map_err(|e| {
            generic_error!("Invalid APM listen address '{}': {}", addr_str, e)
        })?;

        Ok(Self { listen_address })
    }
}

impl Default for ApmReceiverConfiguration {
    fn default() -> Self {
        Self {
            listen_address: DEFAULT_LISTEN_ADDRESS
                .parse()
                .expect("default listen address is valid"),
        }
    }
}

#[async_trait]
impl SourceBuilder for ApmReceiverConfiguration {
    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition<EventType>>> =
            LazyLock::new(|| vec![OutputDefinition::named_output("traces", EventType::V1Trace)]);
        &OUTPUTS
    }

    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(ApmReceiver { listen_address: self.listen_address }))
    }
}

impl MemoryBounds for ApmReceiverConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<ApmReceiver>("component struct");
    }
}

struct ApmReceiver {
    listen_address: SocketAddr,
}

/// Shared state for the axum request handler.
#[derive(Clone)]
struct HandlerState {
    tx: mpsc::Sender<Vec<V1Trace>>,
}

async fn handle_v1_traces(State(state): State<HandlerState>, body: Bytes) -> StatusCode {
    match decode_tracer_payload(&mut body.as_ref()) {
        Ok(raw) => {
            let traces = resolve_payload(raw);
            if !traces.is_empty() {
                if let Err(e) = state.tx.try_send(traces) {
                    warn!(error = %e, "APM receiver channel full; dropping payload.");
                }
            }
            StatusCode::OK
        }
        Err(DeserializeError::UnexpectedEof) | Err(DeserializeError::UnexpectedMarker(_)) => {
            warn!("Malformed v1 trace payload (parse error).");
            StatusCode::BAD_REQUEST
        }
        Err(e) => {
            warn!(error = ?e, "Failed to deserialize v1 trace payload.");
            StatusCode::BAD_REQUEST
        }
    }
}

#[async_trait]
impl Source for ApmReceiver {
    async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let mut shutdown = context.take_shutdown_handle();
        let mut health = context.take_health_handle();

        let (tx, mut rx) = mpsc::channel::<Vec<V1Trace>>(256);

        let listener = TcpListener::bind(self.listen_address).await.map_err(|e| {
            generic_error!("Failed to bind APM receiver on {}: {}", self.listen_address, e)
        })?;

        let app = Router::new()
            .route("/v1.0/traces", post(handle_v1_traces))
            .with_state(HandlerState { tx });

        let (server_shutdown_tx, server_shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            let serve = axum::serve(listener, app)
                .with_graceful_shutdown(async move { let _ = server_shutdown_rx.await; });
            if let Err(e) = serve.await {
                error!(error = %e, "APM HTTP server error.");
            }
        });

        health.mark_ready();
        debug!("APM receiver source started on {}.", self.listen_address);

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
                    if let Err(e) = dispatcher.send_all(traces.into_iter().map(Event::V1Trace)).await {
                        error!(error = %e, "Failed to dispatch V1Trace events.");
                    }
                }
                _ = health.live() => continue,
            }
        }

        debug!("APM receiver source stopped.");
        Ok(())
    }
}

// ── Resolution pass: RawTracerPayload → Vec<V1Trace> ───────────────────────

fn resolve_payload(raw: RawTracerPayload) -> Vec<V1Trace> {
    // Size the interner generously: ~64 bytes per string entry + a 1 KB baseline.
    let capacity_bytes = raw.string_table.len().saturating_mul(64).saturating_add(1024);
    let capacity = NonZeroUsize::new(capacity_bytes).unwrap_or(NonZeroUsize::MIN);
    let interner = GenericMapInterner::new(capacity);

    // Build a flat MetaString index map, one entry per string-table slot.
    let resolved: Vec<MetaString> = raw
        .string_table
        .iter()
        .map(|s| MetaString::from_interner(s, &interner))
        .collect();

    let r = |idx: u32| -> MetaString {
        resolved.get(idx as usize).cloned().unwrap_or_default()
    };

    // Resolve payload-level attributes once; they are shared across all chunks.
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
        .map(|raw_chunk| V1Trace {
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
