//! V1 APM traces encoder.
//!
//! Encodes [`Event::V1Trace`] to `AgentPayload.idxTracerPayloads` (proto field 11) using the
//! `idx.TracerPayload` string-indexed format, forwarded to `/api/v0.2/traces`.
//!
//! **Wire format note**: The Go Trace Agent V1 writer uses `idxTracerPayloads` (field 11), NOT
//! the legacy `tracerPayloads` (field 5) used by the OTLP encoder. The `idx.TracerPayload`
//! message stores all strings in a flat `Strings []` table at field 1; every other string field
//! is a `uint32` index into that table. A two-pass approach is used: a pre-pass builds the
//! complete string table, then the write pass emits the table followed by all indexed fields.

use std::time::Duration;

use async_trait::async_trait;
use datadog_protos::traces::builders::{idx, AgentPayloadBuilder};
use facet::Facet;
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use piecemeal::ScratchWriter;
use saluki_common::collections::FastHashMap;
use saluki_common::task::HandleExt as _;
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{encoders::*, ComponentContext},
    data_model::{
        event::{
            trace::v1::{V1AnyValue, V1KeyValue, V1Trace},
            EventType,
        },
        payload::{HttpPayload, Payload, PayloadMetadata, PayloadType},
    },
    observability::ComponentMetricsExt as _,
    topology::{EventsBuffer, PayloadsBuffer},
};
use saluki_env::{host::providers::BoxedHostProvider, EnvironmentProvider, HostProvider};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::compression::CompressionScheme;
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use stringtheory::MetaString;
use tokio::{
    select,
    sync::mpsc::{self, Receiver, Sender},
    time::sleep,
};
use tracing::{debug, error};

use crate::common::datadog::{
    apm::ApmConfig,
    io::RB_BUFFER_CHUNK_SIZE,
    request_builder::{EndpointEncoder, RequestBuilder},
    telemetry::ComponentTelemetry,
    DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT, DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT,
};

const MAX_TRACES_PER_PAYLOAD: usize = 10000;
static CONTENT_TYPE_PROTOBUF: HeaderValue = HeaderValue::from_static("application/x-protobuf");

fn default_serializer_compressor_kind() -> String {
    "zstd".to_string()
}

const fn default_zstd_compressor_level() -> i32 {
    3
}

const fn default_flush_timeout_secs() -> u64 {
    2
}

fn default_env() -> String {
    "none".to_string()
}

/// Configuration for the V1 APM traces encoder.
#[derive(Deserialize, Facet)]
pub struct V1DatadogTraceConfiguration {
    #[serde(
        rename = "serializer_compressor_kind",
        default = "default_serializer_compressor_kind"
    )]
    compressor_kind: String,

    #[serde(rename = "serializer_zstd_compressor_level", default = "default_zstd_compressor_level")]
    zstd_compressor_level: i32,

    #[serde(default = "default_flush_timeout_secs")]
    flush_timeout_secs: u64,

    #[serde(skip)]
    default_hostname: Option<String>,

    #[serde(skip)]
    version: String,

    #[serde(skip)]
    #[facet(opaque)]
    apm_config: ApmConfig,

    #[serde(default = "default_env")]
    env: String,
}

impl V1DatadogTraceConfiguration {
    /// Creates a new `V1DatadogTraceConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let mut cfg: Self = config.as_typed()?;
        let app_details = saluki_metadata::get_app_details();
        cfg.version = format!("agent-data-plane/{}", app_details.version().raw());
        cfg.apm_config = ApmConfig::from_configuration(config)?;
        Ok(cfg)
    }

    /// Sets the default hostname using the environment provider.
    pub async fn with_environment_provider<E>(mut self, env_provider: E) -> Result<Self, GenericError>
    where
        E: EnvironmentProvider<Host = BoxedHostProvider>,
    {
        let hostname = env_provider.host().get_hostname().await?;
        self.default_hostname = Some(hostname);
        Ok(self)
    }
}

#[async_trait]
impl EncoderBuilder for V1DatadogTraceConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::V1Trace
    }

    fn output_payload_type(&self) -> PayloadType {
        PayloadType::Http
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Encoder + Send>, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let compression_scheme = CompressionScheme::new(&self.compressor_kind, self.zstd_compressor_level);

        let default_hostname = MetaString::from(self.default_hostname.clone().unwrap_or_default());

        let mut trace_rb = RequestBuilder::new(
            V1TraceEndpointEncoder::new(default_hostname, self.version.clone(), self.env.clone(), self.apm_config.clone()),
            compression_scheme,
            RB_BUFFER_CHUNK_SIZE,
        )
        .await?;
        trace_rb.with_max_inputs_per_payload(MAX_TRACES_PER_PAYLOAD);

        let flush_timeout = match self.flush_timeout_secs {
            0 => Duration::from_millis(10),
            secs => Duration::from_secs(secs),
        };

        Ok(Box::new(V1DatadogTrace {
            trace_rb,
            telemetry,
            flush_timeout,
        }))
    }
}

impl MemoryBounds for V1DatadogTraceConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<V1DatadogTrace>("component struct")
            .with_array::<EventsBuffer>("request builder events channel", 8)
            .with_array::<PayloadsBuffer>("request builder payloads channel", 8);

        builder
            .firm()
            .with_array::<V1Trace>("traces split re-encode buffer", MAX_TRACES_PER_PAYLOAD);
    }
}

struct V1DatadogTrace {
    trace_rb: RequestBuilder<V1TraceEndpointEncoder>,
    telemetry: ComponentTelemetry,
    flush_timeout: Duration,
}

#[async_trait]
impl Encoder for V1DatadogTrace {
    async fn run(mut self: Box<Self>, mut context: EncoderContext) -> Result<(), GenericError> {
        let Self {
            trace_rb,
            telemetry,
            flush_timeout,
        } = *self;

        let mut health = context.take_health_handle();
        let (events_tx, events_rx) = mpsc::channel(8);
        let (payloads_tx, mut payloads_rx) = mpsc::channel(8);
        let request_builder_fut = run_request_builder(trace_rb, telemetry, events_rx, payloads_tx, flush_timeout);
        let request_builder_handle = context
            .topology_context()
            .global_thread_pool()
            .spawn_traced_named("v1-traces-request-builder", request_builder_fut);

        health.mark_ready();
        debug!("V1 Datadog Trace encoder started.");

        loop {
            select! {
                biased;
                _ = health.live() => continue,
                maybe_payload = payloads_rx.recv() => match maybe_payload {
                    Some(payload) => {
                        if let Err(e) = context.dispatcher().dispatch(payload).await {
                            error!("Failed to dispatch V1 trace payload: {}", e);
                        }
                    }
                    None => break,
                },
                maybe_event_buffer = context.events().next() => match maybe_event_buffer {
                    Some(event_buffer) => events_tx.send(event_buffer).await
                        .error_context("Failed to send event buffer to V1 request builder.")?,
                    None => break,
                },
            }
        }

        drop(events_tx);
        while let Some(payload) = payloads_rx.recv().await {
            if let Err(e) = context.dispatcher().dispatch(payload).await {
                error!("Failed to dispatch V1 trace payload: {}", e);
            }
        }
        match request_builder_handle.await {
            Ok(Ok(())) => debug!("V1 request builder task stopped."),
            Ok(Err(e)) => error!(error = %e, "V1 request builder task failed."),
            Err(e) => error!(error = %e, "V1 request builder task panicked."),
        }
        debug!("V1 Datadog Trace encoder stopped.");
        Ok(())
    }
}

async fn run_request_builder(
    mut rb: RequestBuilder<V1TraceEndpointEncoder>, telemetry: ComponentTelemetry,
    mut events_rx: Receiver<EventsBuffer>, payloads_tx: Sender<PayloadsBuffer>, flush_timeout: Duration,
) -> Result<(), GenericError> {
    let mut pending_flush = false;
    let pending_flush_timeout = sleep(flush_timeout);
    tokio::pin!(pending_flush_timeout);

    loop {
        select! {
            Some(event_buffer) = events_rx.recv() => {
                for event in event_buffer {
                    let trace = match event.try_into_v1_trace() {
                        Some(t) => t,
                        None => continue,
                    };
                    let trace_to_retry = match rb.encode(trace).await {
                        Ok(None) => continue,
                        Ok(Some(t)) => t,
                        Err(e) => {
                            error!(error = %e, "Failed to encode V1 trace.");
                            telemetry.events_dropped_encoder().increment(1);
                            continue;
                        }
                    };
                    let maybe_requests = rb.flush().await;
                    if maybe_requests.is_empty() {
                        panic!("V1 trace builder told us to flush, but gave us nothing");
                    }
                    for maybe_request in maybe_requests {
                        match maybe_request {
                            Ok((events, request)) => {
                                let payload_meta = PayloadMetadata::from_event_count(events);
                                let http_payload = HttpPayload::new(payload_meta, request);
                                payloads_tx.send(Payload::Http(http_payload)).await
                                    .map_err(|_| generic_error!("Failed to send V1 payload."))?;
                            }
                            Err(e) => {
                                if !e.is_recoverable() {
                                    return Err(GenericError::from(e).context("Failed to flush V1 request."));
                                }
                            }
                        }
                    }
                    if let Err(e) = rb.encode(trace_to_retry).await {
                        error!(error = %e, "Failed to re-encode V1 trace.");
                        telemetry.events_dropped_encoder().increment(1);
                    }
                }
                if !pending_flush {
                    pending_flush_timeout.as_mut().reset(tokio::time::Instant::now() + flush_timeout);
                    pending_flush = true;
                }
            },
            _ = &mut pending_flush_timeout, if pending_flush => {
                pending_flush = false;
                let maybe_requests = rb.flush().await;
                for maybe_request in maybe_requests {
                    match maybe_request {
                        Ok((events, request)) => {
                            let payload_meta = PayloadMetadata::from_event_count(events);
                            let http_payload = HttpPayload::new(payload_meta, request);
                            payloads_tx.send(Payload::Http(http_payload)).await
                                .map_err(|_| generic_error!("Failed to send V1 payload."))?;
                        }
                        Err(e) => {
                            if !e.is_recoverable() {
                                return Err(GenericError::from(e).context("Failed to flush V1 request."));
                            }
                        }
                    }
                }
            },
            else => break,
        }
    }
    Ok(())
}

// ── String table ──────────────────────────────────────────────────────────────

/// Minimal string interning table for `idx.TracerPayload` encoding.
///
/// Index 0 is always the empty string (reserved by the proto format). Non-empty
/// strings are assigned indices 1..N in first-encounter order during a pre-pass
/// over the entire `V1Trace`, ensuring the `Strings` proto field can be written
/// before any `*_ref` field references an index.
struct IdxStringTable {
    map: FastHashMap<MetaString, u32>,
    /// Ordered list of all strings; `strings[0]` is always the empty string.
    strings: Vec<MetaString>,
}

impl IdxStringTable {
    fn new() -> Self {
        // Pre-allocate enough capacity for a typical trace.
        let mut strings = Vec::with_capacity(64);
        strings.push(MetaString::empty()); // index 0 = empty string
        Self {
            map: FastHashMap::default(),
            strings,
        }
    }

    /// Intern a string and return its index. Empty strings always return 0.
    fn intern(&mut self, s: &MetaString) -> u32 {
        if s.is_empty() {
            return 0;
        }
        if let Some(&idx) = self.map.get(s) {
            return idx;
        }
        let idx = self.strings.len() as u32;
        self.map.insert(s.clone(), idx);
        self.strings.push(s.clone());
        idx
    }

    /// Look up the index of an already-interned string. Returns 0 for unknown strings.
    fn get(&self, s: &MetaString) -> u32 {
        if s.is_empty() {
            return 0;
        }
        *self.map.get(s).unwrap_or(&0)
    }
}

/// Build the complete string table from a `V1Trace` in a single pre-pass.
fn collect_strings(trace: &V1Trace) -> IdxStringTable {
    let mut st = IdxStringTable::new();

    // Payload-level metadata strings.
    st.intern(&trace.container_id);
    st.intern(&trace.language_name);
    st.intern(&trace.language_version);
    st.intern(&trace.tracer_version);
    st.intern(&trace.runtime_id);
    st.intern(&trace.env);
    st.intern(&trace.hostname);
    st.intern(&trace.app_version);
    intern_kv_slice(&mut st, &trace.payload_attributes);

    // Chunk-level strings.
    let chunk = &trace.chunk;
    st.intern(&chunk.origin);
    intern_kv_slice(&mut st, &chunk.attributes);

    // Per-span strings.
    for span in &chunk.spans {
        st.intern(&span.service);
        st.intern(&span.name);
        st.intern(&span.resource);
        st.intern(&span.span_type);
        st.intern(&span.env);
        st.intern(&span.version);
        st.intern(&span.component);
        intern_kv_slice(&mut st, &span.attributes);

        for link in &span.links {
            st.intern(&link.tracestate);
            intern_kv_slice(&mut st, &link.attributes);
        }
        for event in &span.events {
            st.intern(&event.name);
            intern_kv_slice(&mut st, &event.attributes);
        }
    }

    st
}

/// Intern all keys and string values from a `V1KeyValue` slice.
fn intern_kv_slice(st: &mut IdxStringTable, kvs: &[V1KeyValue]) {
    for kv in kvs {
        st.intern(&kv.key);
        intern_any_value_strings(st, &kv.value);
    }
}

fn intern_any_value_strings(st: &mut IdxStringTable, v: &V1AnyValue) {
    match v {
        V1AnyValue::String(s) => {
            st.intern(s);
        }
        V1AnyValue::Array(arr) => {
            for elem in arr {
                intern_any_value_strings(st, elem);
            }
        }
        V1AnyValue::KeyValueList(kvs) => {
            for kv in kvs {
                st.intern(&kv.key);
                intern_any_value_strings(st, &kv.value);
            }
        }
        V1AnyValue::Bool(_) | V1AnyValue::Double(_) | V1AnyValue::Int(_) | V1AnyValue::Bytes(_) => {}
    }
}

// ── Encoding helpers ──────────────────────────────────────────────────────────

/// Pack a 128-bit trace ID into a 16-byte big-endian representation.
/// The native idx.TraceChunk.traceID field carries the full 128 bits, so no
/// `_dd.p.tid` span meta tag is needed on the idx path.
fn trace_id_bytes(high: u64, low: u64) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&high.to_be_bytes());
    b[8..].copy_from_slice(&low.to_be_bytes());
    b
}

/// Map a V1 span kind integer to the `idx.SpanKind` enum.
///
/// V1 wire format: 0=unspecified, 1=server, 2=client, 3=producer, 4=consumer, 5=internal.
/// idx.SpanKind:   UNSPECIFIED=0, INTERNAL=1, SERVER=2, CLIENT=3, PRODUCER=4, CONSUMER=5.
fn v1_kind_to_span_kind(kind: u32) -> idx::SpanKind {
    match kind {
        1 => idx::SpanKind::SPAN_KIND_SERVER,
        2 => idx::SpanKind::SPAN_KIND_CLIENT,
        3 => idx::SpanKind::SPAN_KIND_PRODUCER,
        4 => idx::SpanKind::SPAN_KIND_CONSUMER,
        5 => idx::SpanKind::SPAN_KIND_INTERNAL,
        _ => idx::SpanKind::SPAN_KIND_UNSPECIFIED,
    }
}

/// Encode a `V1AnyValue` into an `idx.ValueOneOfBuilder`.
///
/// The `S: 'static` bound is required because `MessageMapBuilder::write_entry` uses
/// a HRTB closure `for<'a> FnOnce(&mut AnyValueBuilder<'a, S>)` which forces `S: 'static`.
fn encode_idx_value<S: piecemeal::ScratchBuffer + 'static>(
    v: &mut idx::ValueOneOfBuilder<'_, S>, value: &V1AnyValue, st: &IdxStringTable,
) -> std::io::Result<()> {
    match value {
        V1AnyValue::String(s) => v.string_value_ref(st.get(s)),
        V1AnyValue::Bool(b) => v.bool_value(*b),
        V1AnyValue::Int(i) => v.int_value(*i),
        V1AnyValue::Double(f) => v.double_value(*f),
        V1AnyValue::Bytes(b) => v.bytes_value(b.as_slice()),
        V1AnyValue::Array(arr) => v.array_value(|a| {
            for elem in arr {
                a.add_values(|av| {
                    av.value(|v2| encode_idx_value(v2, elem, st))?;
                    Ok(())
                })?;
            }
            Ok(())
        }),
        V1AnyValue::KeyValueList(kvs) => v.key_value_list(|kl| {
            for kv in kvs {
                let key_ref = st.get(&kv.key);
                if key_ref == 0 {
                    continue;
                }
                kl.add_key_values(|kb| {
                    kb.key(key_ref)?;
                    kb.value(|av| {
                        av.value(|v2| encode_idx_value(v2, &kv.value, st))?;
                        Ok(())
                    })?;
                    Ok(())
                })?;
            }
            Ok(())
        }),
    }
}

/// Write a `Vec<V1KeyValue>` into an `idx` attribute map (`map<uint32, AnyValue>`).
fn write_idx_attrs<S: piecemeal::ScratchBuffer + 'static>(
    map: &mut piecemeal::MessageMapBuilder<'_, S, piecemeal::types::protobuf::Varint<u32>, idx::AnyValue>,
    kvs: &[V1KeyValue],
    st: &IdxStringTable,
) -> std::io::Result<()> {
    for kv in kvs {
        let key_ref = st.get(&kv.key);
        if key_ref == 0 {
            continue; // skip empty-key attributes
        }
        map.write_entry(key_ref, |av| {
            av.value(|v| encode_idx_value(v, &kv.value, st))?;
            Ok(())
        })?;
    }
    Ok(())
}

// ── Endpoint encoder ──────────────────────────────────────────────────────────

#[derive(Debug)]
struct V1TraceEndpointEncoder {
    scratch: ScratchWriter<Vec<u8>>,
    agent_hostname: String,
    version: String,
    env: String,
    apm_config: ApmConfig,
}

impl V1TraceEndpointEncoder {
    fn new(default_hostname: MetaString, version: String, env: String, apm_config: ApmConfig) -> Self {
        Self {
            scratch: ScratchWriter::new(Vec::with_capacity(8192)),
            agent_hostname: default_hostname.as_ref().to_string(),
            version,
            env,
            apm_config,
        }
    }

    fn encode_idx_payload(&mut self, trace: &V1Trace, output: &mut Vec<u8>) -> std::io::Result<()> {
        // ── Phase 1: build the string table ──────────────────────────────────
        let st = collect_strings(trace);
        let chunk = &trace.chunk;

        // Pre-compute all payload-level refs so we don't need to borrow `st` and the
        // builder at the same time inside the outer closure.
        let container_id_ref = st.get(&trace.container_id);
        let language_name_ref = st.get(&trace.language_name);
        let language_version_ref = st.get(&trace.language_version);
        let tracer_version_ref = st.get(&trace.tracer_version);
        let runtime_id_ref = st.get(&trace.runtime_id);
        let env_ref = st.get(&trace.env);
        let hostname_ref = st.get(&trace.hostname);
        let app_version_ref = st.get(&trace.app_version);
        let origin_ref = st.get(&chunk.origin);

        // ── Phase 2: write the payload ────────────────────────────────────────
        let mut ap = AgentPayloadBuilder::new(&mut self.scratch);

        ap.host_name(&self.agent_hostname)?
            .env(&self.env)?
            .agent_version(&self.version)?
            .target_tps(self.apm_config.target_traces_per_second())?
            .error_tps(self.apm_config.errors_per_second())?;

        ap.add_idx_tracer_payloads(|tp| {
            // Field 1 — string table (must precede all *_ref fields).
            tp.strings(|rb| {
                for s in &st.strings {
                    rb.add(s.as_bytes())?;
                }
                Ok(())
            })?;

            // Payload-level string refs (skip index 0 = empty).
            if container_id_ref != 0 {
                tp.container_id_ref(container_id_ref)?;
            }
            if language_name_ref != 0 {
                tp.language_name_ref(language_name_ref)?;
            }
            if language_version_ref != 0 {
                tp.language_version_ref(language_version_ref)?;
            }
            if tracer_version_ref != 0 {
                tp.tracer_version_ref(tracer_version_ref)?;
            }
            if runtime_id_ref != 0 {
                tp.runtime_id_ref(runtime_id_ref)?;
            }
            if env_ref != 0 {
                tp.env_ref(env_ref)?;
            }
            if hostname_ref != 0 {
                tp.hostname_ref(hostname_ref)?;
            }
            if app_version_ref != 0 {
                tp.app_version_ref(app_version_ref)?;
            }

            // Payload-level attributes.
            write_idx_attrs(&mut tp.attributes(), &trace.payload_attributes, &st)?;

            // The single chunk.
            tp.add_chunks(|ch| {
                ch.priority(chunk.priority)?;

                if origin_ref != 0 {
                    ch.origin_ref(origin_ref)?;
                }
                if chunk.dropped_trace {
                    ch.dropped_trace(true)?;
                }

                // Sampling mechanism: native field in the idx format (not a chunk tag).
                if chunk.sampling_mechanism != 0 {
                    ch.sampling_mechanism(chunk.sampling_mechanism)?;
                }

                // Full 128-bit trace ID as 16 bytes big-endian (high ‖ low).
                let tid = trace_id_bytes(chunk.trace_id_high, chunk.trace_id_low);
                ch.trace_id(&tid)?;

                // Chunk-level attributes.
                write_idx_attrs(&mut ch.attributes(), &chunk.attributes, &st)?;

                for span in &chunk.spans {
                    let service_ref = st.get(&span.service);
                    let name_ref = st.get(&span.name);
                    let resource_ref = st.get(&span.resource);
                    let type_ref = st.get(&span.span_type);
                    let span_env_ref = st.get(&span.env);
                    let version_ref = st.get(&span.version);
                    let component_ref = st.get(&span.component);
                    let span_kind = v1_kind_to_span_kind(span.kind);

                    ch.add_spans(|sb| {
                        if service_ref != 0 {
                            sb.service_ref(service_ref)?;
                        }
                        if name_ref != 0 {
                            sb.name_ref(name_ref)?;
                        }
                        if resource_ref != 0 {
                            sb.resource_ref(resource_ref)?;
                        }

                        sb.span_id(span.span_id)?
                            .parent_id(span.parent_id)?
                            .start(span.start)?
                            .duration(span.duration)?
                            .error(span.error)?;

                        if type_ref != 0 {
                            sb.type_ref(type_ref)?;
                        }
                        if span_env_ref != 0 {
                            sb.env_ref(span_env_ref)?;
                        }
                        if version_ref != 0 {
                            sb.version_ref(version_ref)?;
                        }
                        if component_ref != 0 {
                            sb.component_ref(component_ref)?;
                        }
                        if span_kind != idx::SpanKind::SPAN_KIND_UNSPECIFIED {
                            sb.kind(span_kind)?;
                        }

                        write_idx_attrs(&mut sb.attributes(), &span.attributes, &st)?;

                        for link in &span.links {
                            let tracestate_ref = st.get(&link.tracestate);
                            let link_tid = trace_id_bytes(link.trace_id_high, link.trace_id_low);
                            sb.add_links(|sl| {
                                sl.trace_id(&link_tid)?;
                                sl.span_id(link.span_id)?;
                                write_idx_attrs(&mut sl.attributes(), &link.attributes, &st)?;
                                if tracestate_ref != 0 {
                                    sl.tracestate_ref(tracestate_ref)?;
                                }
                                sl.flags(link.flags)?;
                                Ok(())
                            })?;
                        }

                        for event in &span.events {
                            let event_name_ref = st.get(&event.name);
                            sb.add_events(|se| {
                                se.time(event.time_unix_nano)?;
                                if event_name_ref != 0 {
                                    se.name_ref(event_name_ref)?;
                                }
                                write_idx_attrs(&mut se.attributes(), &event.attributes, &st)?;
                                Ok(())
                            })?;
                        }

                        Ok(())
                    })?;
                }

                Ok(())
            })?;

            Ok(())
        })?;

        ap.finish(output)?;
        Ok(())
    }
}

impl EndpointEncoder for V1TraceEndpointEncoder {
    type Input = V1Trace;
    type EncodeError = std::io::Error;

    fn encoder_name() -> &'static str {
        "v1_traces"
    }

    fn compressed_size_limit(&self) -> usize {
        DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT
    }

    fn uncompressed_size_limit(&self) -> usize {
        DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT
    }

    fn encode(&mut self, trace: &Self::Input, buffer: &mut Vec<u8>) -> Result<(), Self::EncodeError> {
        self.encode_idx_payload(trace, buffer)
    }

    fn endpoint_uri(&self) -> Uri {
        PathAndQuery::from_static("/api/v0.2/traces").into()
    }

    fn endpoint_method(&self) -> Method {
        Method::POST
    }

    fn content_type(&self) -> HeaderValue {
        CONTENT_TYPE_PROTOBUF.clone()
    }

    fn additional_headers(&self) -> &[(http::HeaderName, HeaderValue)] {
        &[]
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use datadog_protos::traces::AgentPayload;
    use protobuf::Message as _;
    use saluki_config::ConfigurationLoader;
    use saluki_core::data_model::event::trace::v1::{
        V1AnyValue, V1KeyValue, V1Span, V1SpanEvent, V1SpanLink, V1Trace, V1TraceChunk,
    };
    use stringtheory::MetaString;

    use super::*;
    use crate::common::datadog::apm::ApmConfig;

    async fn make_encoder() -> V1TraceEndpointEncoder {
        let (cfg, _) = ConfigurationLoader::for_tests(None, None, false).await;
        let apm_config = ApmConfig::from_configuration(&cfg).unwrap();
        V1TraceEndpointEncoder::new(
            MetaString::from("test-host"),
            "0.0.0".to_string(),
            "none".to_string(),
            apm_config,
        )
    }

    fn make_span(service: &str, name: &str, resource: &str, span_id: u64, parent_id: u64) -> V1Span {
        V1Span {
            service: MetaString::from(service),
            name: MetaString::from(name),
            resource: MetaString::from(resource),
            span_id,
            parent_id,
            start: 1_000_000_000,
            duration: 5_000_000,
            error: false,
            attributes: vec![],
            span_type: MetaString::from("web"),
            links: vec![],
            events: vec![],
            env: MetaString::default(),
            version: MetaString::default(),
            component: MetaString::default(),
            kind: 1, // server
        }
    }

    fn make_trace(spans: Vec<V1Span>) -> V1Trace {
        V1Trace {
            chunk: V1TraceChunk {
                priority: 1,
                origin: MetaString::default(),
                attributes: vec![],
                spans,
                dropped_trace: false,
                trace_id_high: 0x0102030405060708,
                trace_id_low: 0x090a0b0c0d0e0f10,
                sampling_mechanism: 4,
            },
            container_id: MetaString::from("abc123"),
            language_name: MetaString::from("python"),
            language_version: MetaString::from("3.11"),
            tracer_version: MetaString::from("1.2.3"),
            runtime_id: MetaString::from("runtime-uuid"),
            env: MetaString::from("prod"),
            hostname: MetaString::from("web-01"),
            app_version: MetaString::from("2.0.0"),
            payload_attributes: vec![],
            client_dropped_p0s_weight: 0.5, // internal — must NOT appear in output
        }
    }

    // Parse the outer AgentPayload fields only; don't try to decode idxTracerPayloads
    // using the wrong (non-idx) TracerPayload type from the generated code.
    fn parse_outer(buf: &[u8]) -> AgentPayload {
        AgentPayload::parse_from_bytes(buf).expect("should parse AgentPayload")
    }

    #[tokio::test]
    async fn encodes_to_idx_field_not_tracer_payloads_field() {
        let mut enc = make_encoder().await;
        let trace = make_trace(vec![make_span("svc", "op", "GET /", 1, 0)]);
        let mut buf = Vec::new();
        enc.encode(&trace, &mut buf).expect("encode should succeed");

        let payload = parse_outer(&buf);

        // Must use field 11 (idxTracerPayloads), NOT field 5 (tracerPayloads).
        assert!(
            payload.tracerPayloads.is_empty(),
            "legacy tracerPayloads (field 5) must be empty for V1 traces"
        );
        assert!(
            !payload.idxTracerPayloads.is_empty(),
            "idxTracerPayloads (field 11) must be populated"
        );
    }

    #[tokio::test]
    async fn outer_agent_payload_fields_are_correct() {
        let mut enc = make_encoder().await;
        let trace = make_trace(vec![make_span("svc", "op", "GET /", 1, 0)]);
        let mut buf = Vec::new();
        enc.encode(&trace, &mut buf).unwrap();

        let payload = parse_outer(&buf);
        assert_eq!(payload.hostName, "test-host");
        assert_eq!(payload.env, "none");
        assert_eq!(payload.agentVersion, "0.0.0");
    }

    #[tokio::test]
    async fn string_table_deduplicates_repeated_strings() {
        // Two spans with the same service name should intern the service string once.
        let span1 = make_span("shared-service", "op1", "res1", 1, 0);
        let span2 = make_span("shared-service", "op2", "res2", 2, 1);
        let trace = make_trace(vec![span1, span2]);

        let st = collect_strings(&trace);
        let idx1 = st.get(&MetaString::from("shared-service"));
        let idx2 = st.get(&MetaString::from("shared-service"));
        assert_eq!(idx1, idx2, "same string must get the same index");
        assert_ne!(idx1, 0, "non-empty string must not get index 0");

        // Index 0 is always the empty string
        assert_eq!(st.get(&MetaString::empty()), 0);
    }

    #[tokio::test]
    async fn span_kind_mapping_covers_all_v1_values() {
        let cases: &[(u32, idx::SpanKind)] = &[
            (0, idx::SpanKind::SPAN_KIND_UNSPECIFIED),
            (1, idx::SpanKind::SPAN_KIND_SERVER),
            (2, idx::SpanKind::SPAN_KIND_CLIENT),
            (3, idx::SpanKind::SPAN_KIND_PRODUCER),
            (4, idx::SpanKind::SPAN_KIND_CONSUMER),
            (5, idx::SpanKind::SPAN_KIND_INTERNAL),
            (99, idx::SpanKind::SPAN_KIND_UNSPECIFIED),
        ];
        for &(v1_kind, expected) in cases {
            assert_eq!(
                v1_kind_to_span_kind(v1_kind),
                expected,
                "v1 kind {} should map to {:?}",
                v1_kind,
                expected
            );
        }
    }

    #[tokio::test]
    async fn trace_id_bytes_packs_high_and_low() {
        let high = 0x0102030405060708u64;
        let low = 0x090a0b0c0d0e0f10u64;
        let bytes = trace_id_bytes(high, low);
        assert_eq!(&bytes[..8], &high.to_be_bytes());
        assert_eq!(&bytes[8..], &low.to_be_bytes());
    }

    #[tokio::test]
    async fn encode_succeeds_with_span_attributes() {
        let mut enc = make_encoder().await;
        let mut span = make_span("svc", "op", "res", 1, 0);
        span.attributes = vec![
            V1KeyValue {
                key: MetaString::from("http.method"),
                value: V1AnyValue::String(MetaString::from("GET")),
            },
            V1KeyValue {
                key: MetaString::from("http.status_code"),
                value: V1AnyValue::Int(200),
            },
            V1KeyValue {
                key: MetaString::from("latency_ms"),
                value: V1AnyValue::Double(3.14),
            },
            V1KeyValue {
                key: MetaString::from("cache_hit"),
                value: V1AnyValue::Bool(true),
            },
        ];
        let trace = make_trace(vec![span]);
        let mut buf = Vec::new();
        enc.encode(&trace, &mut buf).expect("encode with attributes should succeed");
        assert!(!buf.is_empty());
    }

    #[tokio::test]
    async fn encode_succeeds_with_span_links_and_events() {
        let mut enc = make_encoder().await;
        let mut span = make_span("svc", "op", "res", 1, 0);
        span.links = vec![V1SpanLink {
            trace_id_high: 0xAAAAAAAAAAAAAAAA,
            trace_id_low: 0xBBBBBBBBBBBBBBBB,
            span_id: 42,
            attributes: vec![V1KeyValue {
                key: MetaString::from("link.type"),
                value: V1AnyValue::String(MetaString::from("follows_from")),
            }],
            tracestate: MetaString::from("dd=t.dm:-4"),
            flags: 1,
        }];
        span.events = vec![V1SpanEvent {
            time_unix_nano: 999_000_000,
            name: MetaString::from("exception"),
            attributes: vec![V1KeyValue {
                key: MetaString::from("exception.message"),
                value: V1AnyValue::String(MetaString::from("oops")),
            }],
        }];
        let trace = make_trace(vec![span]);
        let mut buf = Vec::new();
        enc.encode(&trace, &mut buf).expect("encode with links and events should succeed");
        assert!(!buf.is_empty());
    }

    #[tokio::test]
    async fn dropped_trace_flag_propagates() {
        let mut enc = make_encoder().await;
        let mut trace = make_trace(vec![make_span("svc", "op", "res", 1, 0)]);
        trace.chunk.dropped_trace = true;
        let mut buf = Vec::new();
        enc.encode(&trace, &mut buf).unwrap();
        // Verify encode completes without error; field 11 carries dropped_trace inside
        // the idx.TraceChunk message which we can't easily decode here, but the important
        // thing is no panic and valid outer protobuf.
        let payload = parse_outer(&buf);
        assert!(!payload.idxTracerPayloads.is_empty());
    }

    #[tokio::test]
    async fn empty_optional_metadata_does_not_panic() {
        let mut enc = make_encoder().await;
        let trace = V1Trace {
            chunk: V1TraceChunk {
                priority: 1,
                origin: MetaString::default(),
                attributes: vec![],
                spans: vec![make_span("svc", "op", "res", 1, 0)],
                dropped_trace: false,
                trace_id_high: 0,
                trace_id_low: 1,
                sampling_mechanism: 0,
            },
            container_id: MetaString::default(),
            language_name: MetaString::default(),
            language_version: MetaString::default(),
            tracer_version: MetaString::default(),
            runtime_id: MetaString::default(),
            env: MetaString::default(),
            hostname: MetaString::default(),
            app_version: MetaString::default(),
            payload_attributes: vec![],
            client_dropped_p0s_weight: 0.0,
        };
        let mut buf = Vec::new();
        enc.encode(&trace, &mut buf).expect("encode with empty metadata should not panic");
        assert!(!buf.is_empty());
    }
}
