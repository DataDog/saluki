//! APM traces encoder (idx format).
//!
//! Encodes `Event::Trace` events from the APM pipeline to `AgentPayload.idxTracerPayloads`
//! (proto field 11) using the `idx.TracerPayload` string-indexed format, forwarded to
//! `/api/v0.2/traces`.
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
            trace::{AttributeValue, EventAttributeScalarValue, EventAttributeValue, Span, Trace},
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
/// Sentinel priority value matching Go's `PriorityNone = math.MinInt8`.
const PRIORITY_NONE: i32 = i8::MIN as i32;
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
        EventType::Trace
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
            .with_array::<Trace>("traces split re-encode buffer", MAX_TRACES_PER_PAYLOAD);
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
                    let trace = match event.try_into_trace() {
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
/// over the entire `Trace`, ensuring the `Strings` proto field can be written
/// before any `*_ref` field references an index.
struct IdxStringTable {
    map: FastHashMap<MetaString, u32>,
    /// Ordered list of all strings; `strings[0]` is always the empty string.
    strings: Vec<MetaString>,
}

impl IdxStringTable {
    fn new() -> Self {
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

    fn get_str(&self, s: &str) -> u32 {
        if s.is_empty() {
            return 0;
        }
        *self.map.get(s).unwrap_or(&0)
    }
}

/// Build the complete string table from a `Trace` in a single pre-pass.
fn collect_strings(trace: &Trace) -> IdxStringTable {
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

    // Trace-level attributes (merged payload + chunk attributes).
    intern_attribute_map(&mut st, &trace.attributes);

    // Chunk-level strings.
    st.intern(&trace.origin);

    // Per-span strings.
    for span in trace.spans() {
        st.intern(&MetaString::from(span.service()));
        st.intern(&MetaString::from(span.name()));
        st.intern(&MetaString::from(span.resource()));
        st.intern(&MetaString::from(span.span_type()));
        st.intern(&span.env);
        st.intern(&span.version);
        st.intern(&span.component);

        // Span attributes from the three legacy maps.
        for (k, v) in span.meta() {
            st.intern(k);
            st.intern(v);
        }
        for k in span.metrics().keys() {
            st.intern(k);
        }
        for k in span.meta_struct().keys() {
            st.intern(k);
        }

        for link in span.span_links() {
            st.intern(&MetaString::from(link.tracestate()));
            for (k, v) in link.attributes() {
                st.intern(k);
                st.intern(v);
            }
        }
        for event in span.span_events() {
            st.intern(&MetaString::from(event.name()));
            for (k, v) in event.attributes() {
                st.intern(k);
                intern_event_attribute_value_strings(&mut st, v);
            }
        }
    }

    st
}

fn intern_attribute_map(st: &mut IdxStringTable, attrs: &FastHashMap<MetaString, AttributeValue>) {
    for (k, v) in attrs {
        st.intern(k);
        if let AttributeValue::String(s) = v {
            st.intern(s);
        }
    }
}

fn intern_event_attribute_value_strings(st: &mut IdxStringTable, v: &EventAttributeValue) {
    match v {
        EventAttributeValue::String(s) => {
            st.intern(s);
        }
        EventAttributeValue::Array(arr) => {
            for elem in arr {
                if let EventAttributeScalarValue::String(s) = elem {
                    st.intern(s);
                }
            }
        }
        _ => {}
    }
}

// ── Encoding helpers ──────────────────────────────────────────────────────────

/// Pack a 128-bit trace ID into a 16-byte big-endian representation.
fn trace_id_bytes(high: u64, low: u64) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&high.to_be_bytes());
    b[8..].copy_from_slice(&low.to_be_bytes());
    b
}

/// Map a span kind integer to the `idx.SpanKind` enum.
///
/// V1 wire format: 0=unspecified, 1=server, 2=client, 3=producer, 4=consumer, 5=internal.
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

/// Write an `AttributeValue` into an `idx.ValueOneOfBuilder`.
fn encode_attribute_value<S: piecemeal::ScratchBuffer + 'static>(
    v: &mut idx::ValueOneOfBuilder<'_, S>, value: &AttributeValue, st: &IdxStringTable,
) -> std::io::Result<()> {
    match value {
        AttributeValue::String(s) => v.string_value_ref(st.get(s)),
        AttributeValue::Float(f) => v.double_value(*f),
        AttributeValue::Bytes(b) => v.bytes_value(b.as_slice()),
    }
}

/// Write an `EventAttributeValue` into an `idx.ValueOneOfBuilder`.
fn encode_event_attribute_value<S: piecemeal::ScratchBuffer + 'static>(
    v: &mut idx::ValueOneOfBuilder<'_, S>, value: &EventAttributeValue, st: &IdxStringTable,
) -> std::io::Result<()> {
    match value {
        EventAttributeValue::String(s) => v.string_value_ref(st.get(s)),
        EventAttributeValue::Bool(b) => v.bool_value(*b),
        EventAttributeValue::Int(i) => v.int_value(*i),
        EventAttributeValue::Double(f) => v.double_value(*f),
        EventAttributeValue::Array(arr) => v.array_value(|a| {
            for elem in arr {
                a.add_values(|av| {
                    av.value(|v2| {
                        match elem {
                            EventAttributeScalarValue::String(s) => v2.string_value_ref(st.get(s)),
                            EventAttributeScalarValue::Bool(b) => v2.bool_value(*b),
                            EventAttributeScalarValue::Int(i) => v2.int_value(*i),
                            EventAttributeScalarValue::Double(f) => v2.double_value(*f),
                        }
                    })?;
                    Ok(())
                })?;
            }
            Ok(())
        }),
    }
}

/// Write a `FastHashMap<MetaString, AttributeValue>` into an `idx` attribute map.
fn write_idx_attribute_map<S: piecemeal::ScratchBuffer + 'static>(
    map: &mut piecemeal::MessageMapBuilder<'_, S, piecemeal::types::protobuf::Varint<u32>, idx::AnyValue>,
    attrs: &FastHashMap<MetaString, AttributeValue>,
    st: &IdxStringTable,
) -> std::io::Result<()> {
    for (k, v) in attrs {
        let key_ref = st.get(k);
        if key_ref == 0 {
            continue;
        }
        map.write_entry(key_ref, |av| {
            av.value(|vb| encode_attribute_value(vb, v, st))?;
            Ok(())
        })?;
    }
    Ok(())
}

/// Write a `FastHashMap<MetaString, MetaString>` (link attributes) into an `idx` attribute map.
fn write_idx_string_map<S: piecemeal::ScratchBuffer + 'static>(
    map: &mut piecemeal::MessageMapBuilder<'_, S, piecemeal::types::protobuf::Varint<u32>, idx::AnyValue>,
    attrs: &FastHashMap<MetaString, MetaString>,
    st: &IdxStringTable,
) -> std::io::Result<()> {
    for (k, v) in attrs {
        let key_ref = st.get(k);
        if key_ref == 0 {
            continue;
        }
        let val_ref = st.get(v);
        map.write_entry(key_ref, |av| {
            av.value(|vb| vb.string_value_ref(val_ref))?;
            Ok(())
        })?;
    }
    Ok(())
}

/// Write span attributes from `meta`/`metrics`/`meta_struct` into an `idx` attribute map.
fn write_idx_span_attrs<S: piecemeal::ScratchBuffer + 'static>(
    map: &mut piecemeal::MessageMapBuilder<'_, S, piecemeal::types::protobuf::Varint<u32>, idx::AnyValue>,
    span: &Span,
    st: &IdxStringTable,
) -> std::io::Result<()> {
    for (k, v) in span.meta() {
        let key_ref = st.get(k);
        if key_ref == 0 {
            continue;
        }
        let val_ref = st.get(v);
        map.write_entry(key_ref, |av| {
            av.value(|vb| vb.string_value_ref(val_ref))?;
            Ok(())
        })?;
    }
    for (k, v) in span.metrics() {
        let key_ref = st.get(k);
        if key_ref == 0 {
            continue;
        }
        map.write_entry(key_ref, |av| {
            av.value(|vb| vb.double_value(*v))?;
            Ok(())
        })?;
    }
    for (k, v) in span.meta_struct() {
        let key_ref = st.get(k);
        if key_ref == 0 {
            continue;
        }
        map.write_entry(key_ref, |av| {
            av.value(|vb| vb.bytes_value(v.as_slice()))?;
            Ok(())
        })?;
    }
    Ok(())
}

/// Write event attributes into an `idx` attribute map.
fn write_idx_event_attrs<S: piecemeal::ScratchBuffer + 'static>(
    map: &mut piecemeal::MessageMapBuilder<'_, S, piecemeal::types::protobuf::Varint<u32>, idx::AnyValue>,
    attrs: &FastHashMap<MetaString, EventAttributeValue>,
    st: &IdxStringTable,
) -> std::io::Result<()> {
    for (k, v) in attrs {
        let key_ref = st.get(k);
        if key_ref == 0 {
            continue;
        }
        map.write_entry(key_ref, |av| {
            av.value(|vb| encode_event_attribute_value(vb, v, st))?;
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

    fn encode_idx_payload(&mut self, trace: &Trace, output: &mut Vec<u8>) -> std::io::Result<()> {
        let root_service = trace
            .spans()
            .iter()
            .find(|s| s.parent_id() == 0)
            .or_else(|| trace.spans().first())
            .map(|s| s.service())
            .unwrap_or("");
        debug!(
            spans = trace.spans().len(),
            env = trace.env.as_ref(),
            service = root_service,
            "Encoding V1 trace."
        );

        // ── Phase 1: build the string table ──────────────────────────────────
        let st = collect_strings(trace);

        let container_id_ref = st.get(&trace.container_id);
        let language_name_ref = st.get(&trace.language_name);
        let language_version_ref = st.get(&trace.language_version);
        let tracer_version_ref = st.get(&trace.tracer_version);
        let runtime_id_ref = st.get(&trace.runtime_id);
        let env_ref = st.get(&trace.env);
        let hostname_ref = st.get(&trace.hostname);
        let app_version_ref = st.get(&trace.app_version);
        let origin_ref = st.get(&trace.origin);
        let priority = trace.priority.unwrap_or(PRIORITY_NONE);

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

            // Payload-level attributes (merged from payload_attributes + chunk attributes).
            write_idx_attribute_map(&mut tp.attributes(), &trace.attributes, &st)?;

            // The single chunk.
            tp.add_chunks(|ch| {
                ch.priority(priority)?;

                if origin_ref != 0 {
                    ch.origin_ref(origin_ref)?;
                }
                if trace.dropped_trace {
                    ch.dropped_trace(true)?;
                }

                if trace.sampling_mechanism != 0 {
                    ch.sampling_mechanism(trace.sampling_mechanism)?;
                }

                let tid = trace_id_bytes(trace.trace_id_high, trace.trace_id_low);
                ch.trace_id(&tid)?;

                // Chunk-level attributes: written at payload level above; leave chunk attrs empty.

                for span in trace.spans() {
                    let service_ref = st.get_str(span.service());
                    let name_ref = st.get_str(span.name());
                    let resource_ref = st.get_str(span.resource());
                    let type_ref = st.get_str(span.span_type());
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

                        sb.span_id(span.span_id())?
                            .parent_id(span.parent_id())?
                            .start(span.start())?
                            .duration(span.duration())?
                            .error(span.error() != 0)?;

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

                        write_idx_span_attrs(&mut sb.attributes(), span, &st)?;

                        for link in span.span_links() {
                            let tracestate_ref = st.get_str(link.tracestate());
                            let link_tid = trace_id_bytes(link.trace_id_high(), link.trace_id());
                            sb.add_links(|sl| {
                                sl.trace_id(&link_tid)?;
                                sl.span_id(link.span_id())?;
                                write_idx_string_map(&mut sl.attributes(), link.attributes(), &st)?;
                                if tracestate_ref != 0 {
                                    sl.tracestate_ref(tracestate_ref)?;
                                }
                                sl.flags(link.flags())?;
                                Ok(())
                            })?;
                        }

                        for event in span.span_events() {
                            let event_name_ref = st.get_str(event.name());
                            sb.add_events(|se| {
                                se.time(event.time_unix_nano())?;
                                if event_name_ref != 0 {
                                    se.name_ref(event_name_ref)?;
                                }
                                write_idx_event_attrs(&mut se.attributes(), event.attributes(), &st)?;
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
    type Input = Trace;
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
    use saluki_common::collections::FastHashMap;
    use saluki_config::ConfigurationLoader;
    use saluki_context::tags::TagSet;
    use saluki_core::data_model::event::trace::{
        EventAttributeValue, Span, SpanEvent, SpanLink, Trace,
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

    fn make_span(service: &str, name: &str, resource: &str, span_id: u64, parent_id: u64) -> Span {
        Span::new(service, name, resource, "web", 0, span_id, parent_id, 1_000_000_000, 5_000_000, 0)
            .with_kind(1) // server
    }

    fn make_trace(spans: Vec<Span>) -> Trace {
        let mut trace = Trace::new(spans, TagSet::default());
        trace.priority = Some(1);
        trace.trace_id_high = 0x0102030405060708;
        trace.trace_id_low = 0x090a0b0c0d0e0f10;
        trace.sampling_mechanism = 4;
        trace.container_id = MetaString::from("abc123");
        trace.language_name = MetaString::from("python");
        trace.language_version = MetaString::from("3.11");
        trace.tracer_version = MetaString::from("1.2.3");
        trace.runtime_id = MetaString::from("runtime-uuid");
        trace.env = MetaString::from("prod");
        trace.hostname = MetaString::from("web-01");
        trace.app_version = MetaString::from("2.0.0");
        trace.client_dropped_p0s_weight = 0.5; // internal — must NOT appear in output
        trace
    }

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
        let span1 = make_span("shared-service", "op1", "res1", 1, 0);
        let span2 = make_span("shared-service", "op2", "res2", 2, 1);
        let trace = make_trace(vec![span1, span2]);

        let st = collect_strings(&trace);
        let idx1 = st.get(&MetaString::from("shared-service"));
        let idx2 = st.get(&MetaString::from("shared-service"));
        assert_eq!(idx1, idx2, "same string must get the same index");
        assert_ne!(idx1, 0, "non-empty string must not get index 0");

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
        let mut meta = FastHashMap::default();
        meta.insert(MetaString::from("http.method"), MetaString::from("GET"));
        meta.insert(MetaString::from("cache_hit"), MetaString::from("true"));
        let mut metrics = FastHashMap::default();
        metrics.insert(MetaString::from("http.status_code"), 200.0f64);
        metrics.insert(MetaString::from("latency_ms"), 3.14f64);
        let span = make_span("svc", "op", "res", 1, 0)
            .with_meta(Some(meta))
            .with_metrics(Some(metrics));
        let trace = make_trace(vec![span]);
        let mut buf = Vec::new();
        enc.encode(&trace, &mut buf).expect("encode with attributes should succeed");
        assert!(!buf.is_empty());
    }

    #[tokio::test]
    async fn encode_succeeds_with_span_links_and_events() {
        let mut enc = make_encoder().await;
        let mut link_attrs = FastHashMap::default();
        link_attrs.insert(MetaString::from("link.type"), MetaString::from("follows_from"));
        let link = SpanLink::new(0xBBBBBBBBBBBBBBBB, 42)
            .with_trace_id_high(0xAAAAAAAAAAAAAAAA)
            .with_attributes(Some(link_attrs))
            .with_tracestate(MetaString::from("dd=t.dm:-4"))
            .with_flags(1);

        let mut event_attrs = FastHashMap::default();
        event_attrs.insert(
            MetaString::from("exception.message"),
            EventAttributeValue::String(MetaString::from("oops")),
        );
        let event = SpanEvent::new(999_000_000, "exception").with_attributes(Some(event_attrs));

        let span = make_span("svc", "op", "res", 1, 0)
            .with_span_links(Some(vec![link]))
            .with_span_events(Some(vec![event]));
        let trace = make_trace(vec![span]);
        let mut buf = Vec::new();
        enc.encode(&trace, &mut buf).expect("encode with links and events should succeed");
        assert!(!buf.is_empty());
    }

    #[tokio::test]
    async fn dropped_trace_flag_propagates() {
        let mut enc = make_encoder().await;
        let mut trace = make_trace(vec![make_span("svc", "op", "res", 1, 0)]);
        trace.dropped_trace = true;
        let mut buf = Vec::new();
        enc.encode(&trace, &mut buf).unwrap();
        let payload = parse_outer(&buf);
        assert!(!payload.idxTracerPayloads.is_empty());
    }

    #[tokio::test]
    async fn empty_optional_metadata_does_not_panic() {
        let mut enc = make_encoder().await;
        let trace = make_trace(vec![make_span("svc", "op", "res", 1, 0)]);
        let mut buf = Vec::new();
        enc.encode(&trace, &mut buf).expect("empty metadata should not panic");
        assert!(!buf.is_empty());
    }
}
