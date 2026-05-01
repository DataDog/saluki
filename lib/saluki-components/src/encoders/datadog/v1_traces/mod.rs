//! V1 APM traces encoder.
//!
//! Encodes [`Event::V1Trace`] events to the same `AgentPayload` protobuf that the OTLP traces
//! encoder produces, forwarded to `/api/v0.2/traces`. The V1 path is simpler because all metadata
//! is promoted from the tracer payload and already lives as [`MetaString`] fields on [`V1Trace`] —
//! there is no OTLP resource-tag resolution and no string-table lookup.

use std::{fmt::Write, time::Duration};

use async_trait::async_trait;
use datadog_protos::traces::builders::{
    attribute_any_value::AttributeAnyValueType, attribute_array_value::AttributeArrayValueType, AgentPayloadBuilder,
    AttributeAnyValueBuilder, AttributeArrayValueBuilder,
};
use facet::Facet;
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use piecemeal::{ScratchBuffer, ScratchWriter};
use saluki_common::strings::StringBuilder;
use saluki_common::task::HandleExt as _;
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{encoders::*, ComponentContext},
    data_model::{
        event::{
            trace::v1::{V1AnyValue, V1Trace},
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
    DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT, DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT, TAG_DECISION_MAKER,
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
///
/// Encodes `Event::V1Trace` events into the `AgentPayload` protobuf and dispatches them to
/// `/api/v0.2/traces`. Metadata (env, hostname, container_id, language, tracer version) comes
/// directly from the promoted [`V1Trace`] fields — no OTLP resource-tag resolution is required.
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

#[derive(Debug)]
struct V1TraceEndpointEncoder {
    scratch: ScratchWriter<Vec<u8>>,
    agent_hostname: String,
    version: String,
    env: String,
    apm_config: ApmConfig,
    string_builder: StringBuilder,
}

impl V1TraceEndpointEncoder {
    fn new(default_hostname: MetaString, version: String, env: String, apm_config: ApmConfig) -> Self {
        Self {
            scratch: ScratchWriter::new(Vec::with_capacity(8192)),
            agent_hostname: default_hostname.as_ref().to_string(),
            version,
            env,
            apm_config,
            string_builder: StringBuilder::new(),
        }
    }

    fn encode_v1_tracer_payload(&mut self, trace: &V1Trace, output: &mut Vec<u8>) -> std::io::Result<()> {
        let chunk = &trace.chunk;

        let mut ap_builder = AgentPayloadBuilder::new(&mut self.scratch);

        ap_builder
            .host_name(&self.agent_hostname)?
            .env(&self.env)?
            .agent_version(&self.version)?
            .target_tps(self.apm_config.target_traces_per_second())?
            .error_tps(self.apm_config.errors_per_second())?;

        ap_builder.add_tracer_payloads(|tp| {
            if !trace.container_id.is_empty() {
                tp.container_id(trace.container_id.as_ref())?;
            }
            if !trace.language_name.is_empty() {
                tp.language_name(trace.language_name.as_ref())?;
            }
            if !trace.language_version.is_empty() {
                tp.language_version(trace.language_version.as_ref())?;
            }
            if !trace.tracer_version.is_empty() {
                tp.tracer_version(trace.tracer_version.as_ref())?;
            }
            if !trace.runtime_id.is_empty() {
                tp.runtime_id(trace.runtime_id.as_ref())?;
            }
            if !trace.env.is_empty() {
                tp.env(trace.env.as_ref())?;
            }
            if !trace.hostname.is_empty() {
                tp.hostname(trace.hostname.as_ref())?;
            }
            if !trace.app_version.is_empty() {
                tp.app_version(trace.app_version.as_ref())?;
            }

            // Payload-level attributes become TracerPayload tags (string-only values).
            {
                let mut tags = tp.tags();
                for kv in &trace.payload_attributes {
                    if let V1AnyValue::String(v) = &kv.value {
                        tags.write_entry(kv.key.as_ref(), v.as_ref())?;
                    }
                }
            }

            tp.add_chunks(|chunk_builder| {
                chunk_builder.priority(chunk.priority)?;

                if !chunk.origin.is_empty() {
                    chunk_builder.origin(chunk.origin.as_ref())?;
                }

                if chunk.dropped_trace {
                    chunk_builder.dropped_trace(true)?;
                }

                // Chunk tags: sampling mechanism + chunk-level string attributes.
                {
                    let mut tags = chunk_builder.tags();
                    if chunk.sampling_mechanism != 0 {
                        self.string_builder.clear();
                        write!(&mut self.string_builder, "{}", chunk.sampling_mechanism)
                            .expect("formatting u32 never fails");
                        tags.write_entry(TAG_DECISION_MAKER, self.string_builder.as_str())?;
                    }
                    for kv in &chunk.attributes {
                        if let V1AnyValue::String(v) = &kv.value {
                            tags.write_entry(kv.key.as_ref(), v.as_ref())?;
                        }
                    }
                }

                // Write _dd.p.tid on the first span when the trace ID has a non-zero high half.
                // The legacy Span proto only has a 64-bit trace_id field; the high 64 bits are
                // conveyed as a hex string in this meta tag, matching the Go converter at
                // pkg/trace/api/converter.go.
                let tid_tag = if chunk.trace_id_high != 0 {
                    self.string_builder.clear();
                    write!(&mut self.string_builder, "{:016x}", chunk.trace_id_high)
                        .expect("formatting u64 as hex never fails");
                    Some(self.string_builder.as_str().to_owned())
                } else {
                    None
                };
                let mut first_span = true;

                for span in &chunk.spans {
                    let is_first = first_span;
                    first_span = false;
                    chunk_builder.add_spans(|s| {
                        s.service(span.service.as_ref())?
                            .name(span.name.as_ref())?
                            .resource(span.resource.as_ref())?
                            .trace_id(chunk.trace_id_low)?
                            .span_id(span.span_id)?
                            .parent_id(span.parent_id)?
                            .start(span.start as i64)?
                            .duration(span.duration as i64)?
                            .error(span.error as i32)?
                            .type_(span.span_type.as_ref())?;

                        // meta: string + bool attributes, plus span-level string fields.
                        {
                            let mut meta = s.meta();
                            if is_first {
                                if let Some(ref tid) = tid_tag {
                                    meta.write_entry("_dd.p.tid", tid.as_str())?;
                                }
                            }
                            let kind_str = v1_kind_to_str(span.kind);
                            if !kind_str.is_empty() {
                                meta.write_entry("span.kind", kind_str)?;
                            }
                            if !span.env.is_empty() {
                                meta.write_entry("env", span.env.as_ref())?;
                            }
                            if !span.version.is_empty() {
                                meta.write_entry("version", span.version.as_ref())?;
                            }
                            if !span.component.is_empty() {
                                meta.write_entry("component", span.component.as_ref())?;
                            }
                            for kv in &span.attributes {
                                match &kv.value {
                                    V1AnyValue::String(v) => meta.write_entry(kv.key.as_ref(), v.as_ref())?,
                                    V1AnyValue::Bool(b) => {
                                        meta.write_entry(kv.key.as_ref(), if *b { "true" } else { "false" })?
                                    }
                                    _ => {}
                                }
                            }
                        }

                        // metrics: numeric attributes.
                        {
                            let mut metrics = s.metrics();
                            for kv in &span.attributes {
                                match &kv.value {
                                    V1AnyValue::Int(i) => metrics.write_entry(kv.key.as_ref(), *i as f64)?,
                                    V1AnyValue::Double(f) => metrics.write_entry(kv.key.as_ref(), *f)?,
                                    _ => {}
                                }
                            }
                        }

                        // Span links.
                        for link in &span.links {
                            s.add_span_links(|sl| {
                                sl.trace_id(link.trace_id_low)?
                                    .trace_id_high(link.trace_id_high)?
                                    .span_id(link.span_id)?;
                                {
                                    let mut attrs = sl.attributes();
                                    for kv in &link.attributes {
                                        if let V1AnyValue::String(v) = &kv.value {
                                            attrs.write_entry(kv.key.as_ref(), v.as_ref())?;
                                        }
                                    }
                                }
                                sl.tracestate(link.tracestate.as_ref())?.flags(link.flags)?;
                                Ok(())
                            })?;
                        }

                        // Span events.
                        for event in &span.events {
                            s.add_span_events(|se| {
                                se.time_unix_nano(event.time_unix_nano)?.name(event.name.as_ref())?;
                                {
                                    let mut attrs = se.attributes();
                                    for kv in &event.attributes {
                                        attrs.write_entry(kv.key.as_ref(), |av| {
                                            encode_v1_attribute_value(av, &kv.value)
                                        })?;
                                    }
                                }
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

        ap_builder.finish(output)?;
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
        self.encode_v1_tracer_payload(trace, buffer)
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

/// Maps a V1 span kind integer to its string representation for the `span.kind` meta tag.
fn v1_kind_to_str(kind: u32) -> &'static str {
    match kind {
        1 => "server",
        2 => "client",
        3 => "producer",
        4 => "consumer",
        5 => "internal",
        _ => "",
    }
}

fn encode_v1_attribute_value<S: ScratchBuffer>(
    builder: &mut AttributeAnyValueBuilder<'_, S>, value: &V1AnyValue,
) -> std::io::Result<()> {
    match value {
        V1AnyValue::String(v) => {
            builder.type_(AttributeAnyValueType::STRING_VALUE)?.string_value(v.as_ref())?;
        }
        V1AnyValue::Bool(b) => {
            builder.type_(AttributeAnyValueType::BOOL_VALUE)?.bool_value(*b)?;
        }
        V1AnyValue::Int(i) => {
            builder.type_(AttributeAnyValueType::INT_VALUE)?.int_value(*i)?;
        }
        V1AnyValue::Double(f) => {
            builder.type_(AttributeAnyValueType::DOUBLE_VALUE)?.double_value(*f)?;
        }
        V1AnyValue::Array(values) => {
            builder.type_(AttributeAnyValueType::ARRAY_VALUE)?.array_value(|arr| {
                for val in values {
                    arr.add_values(|av| encode_v1_attribute_array_value(av, val))?;
                }
                Ok(())
            })?;
        }
        V1AnyValue::Bytes(_) | V1AnyValue::KeyValueList(_) => {
            // These types have no direct protobuf attribute equivalent; skip them.
        }
    }
    Ok(())
}

fn encode_v1_attribute_array_value<S: ScratchBuffer>(
    builder: &mut AttributeArrayValueBuilder<'_, S>, value: &V1AnyValue,
) -> std::io::Result<()> {
    match value {
        V1AnyValue::String(v) => {
            builder.type_(AttributeArrayValueType::STRING_VALUE)?.string_value(v.as_ref())?;
        }
        V1AnyValue::Bool(b) => {
            builder.type_(AttributeArrayValueType::BOOL_VALUE)?.bool_value(*b)?;
        }
        V1AnyValue::Int(i) => {
            builder.type_(AttributeArrayValueType::INT_VALUE)?.int_value(*i)?;
        }
        V1AnyValue::Double(f) => {
            builder.type_(AttributeArrayValueType::DOUBLE_VALUE)?.double_value(*f)?;
        }
        V1AnyValue::Array(_) | V1AnyValue::Bytes(_) | V1AnyValue::KeyValueList(_) => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use datadog_protos::traces::AgentPayload;
    use protobuf::Message as _;
    use saluki_config::ConfigurationLoader;
    use saluki_core::data_model::event::trace::v1::{
        V1AnyValue, V1KeyValue, V1Span, V1Trace, V1TraceChunk,
    };
    use stringtheory::MetaString;

    use super::*;
    use crate::common::datadog::apm::ApmConfig;

    async fn make_encoder() -> V1TraceEndpointEncoder {
        let (cfg, _) = ConfigurationLoader::for_tests(None, None, false).await;
        let apm_config = ApmConfig::from_configuration(&cfg).expect("ApmConfig should deserialize");
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
                trace_id_high: 0,
                trace_id_low: 0xdeadbeef,
                sampling_mechanism: 4,
            },
            container_id: MetaString::from("abc123"),
            language_name: MetaString::from("python"),
            language_version: MetaString::from("3.11"),
            tracer_version: MetaString::from("1.2.3"),
            runtime_id: MetaString::default(),
            env: MetaString::from("prod"),
            hostname: MetaString::from("web-01"),
            app_version: MetaString::from("2.0.0"),
            payload_attributes: vec![],
            client_dropped_p0s_weight: 0.5, // must NOT appear in output
        }
    }

    #[tokio::test]
    async fn basic_encode_produces_valid_agent_payload() {
        let mut enc = make_encoder().await;
        let trace = make_trace(vec![make_span("svc", "op", "GET /", 1, 0)]);
        let mut buf = Vec::new();
        enc.encode(&trace, &mut buf).expect("encode should succeed");

        let payload = AgentPayload::parse_from_bytes(&buf).expect("should parse AgentPayload");
        assert_eq!(payload.tracerPayloads.len(), 1);

        let tp = &payload.tracerPayloads[0];
        assert_eq!(tp.containerID, "abc123");
        assert_eq!(tp.languageName, "python");
        assert_eq!(tp.tracerVersion, "1.2.3");
        assert_eq!(tp.env, "prod");
        assert_eq!(tp.hostname, "web-01");
        assert_eq!(tp.appVersion, "2.0.0");

        assert_eq!(tp.chunks.len(), 1);
        let chunk = &tp.chunks[0];
        assert_eq!(chunk.priority, 1);
        assert!(!chunk.droppedTrace);

        assert_eq!(chunk.spans.len(), 1);
        let span = &chunk.spans[0];
        assert_eq!(span.service, "svc");
        assert_eq!(span.name, "op");
        assert_eq!(span.resource, "GET /");
        assert_eq!(span.traceID, 0xdeadbeef);
        assert_eq!(span.spanID, 1);
        assert_eq!(span.parentID, 0);
        assert_eq!(span.type_, "web");
        assert_eq!(span.meta.get("span.kind").map(|s| s.as_str()), Some("server"));
    }

    #[tokio::test]
    async fn sampling_mechanism_written_as_decision_maker_tag() {
        let mut enc = make_encoder().await;
        let trace = make_trace(vec![make_span("svc", "op", "res", 1, 0)]);
        let mut buf = Vec::new();
        enc.encode(&trace, &mut buf).unwrap();

        let payload = AgentPayload::parse_from_bytes(&buf).unwrap();
        let chunk = &payload.tracerPayloads[0].chunks[0];
        // sampling_mechanism=4 → "_dd.p.dm" = "4" (decimal, no leading dash)
        assert_eq!(chunk.tags.get("_dd.p.dm").map(|s| s.as_str()), Some("4"));
    }

    #[tokio::test]
    async fn client_dropped_p0s_weight_not_forwarded() {
        let mut enc = make_encoder().await;
        let mut trace = make_trace(vec![make_span("svc", "op", "res", 1, 0)]);
        trace.client_dropped_p0s_weight = 0.99;
        let mut buf = Vec::new();
        enc.encode(&trace, &mut buf).unwrap();

        let payload = AgentPayload::parse_from_bytes(&buf).unwrap();
        let tp = &payload.tracerPayloads[0];
        let span = &tp.chunks[0].spans[0];
        // client_dropped_p0s_weight is internal rate-computation metadata.
        // It must not appear anywhere in the forwarded payload.
        assert!(
            !span.meta.contains_key("client_dropped_p0s_weight"),
            "internal field must not appear in span meta"
        );
        assert!(
            !span.metrics.contains_key("client_dropped_p0s_weight"),
            "internal field must not appear in span metrics"
        );
        assert!(
            !tp.tags.contains_key("client_dropped_p0s_weight"),
            "internal field must not appear in TracerPayload tags"
        );
    }

    #[tokio::test]
    async fn span_attributes_split_into_meta_and_metrics() {
        let mut enc = make_encoder().await;
        let mut span = make_span("svc", "op", "res", 1, 0);
        span.attributes = vec![
            V1KeyValue { key: MetaString::from("http.method"), value: V1AnyValue::String(MetaString::from("GET")) },
            V1KeyValue { key: MetaString::from("http.status_code"), value: V1AnyValue::Int(200) },
            V1KeyValue { key: MetaString::from("duration_ms"), value: V1AnyValue::Double(3.14) },
            V1KeyValue { key: MetaString::from("error"), value: V1AnyValue::Bool(false) },
        ];
        let trace = make_trace(vec![span]);
        let mut buf = Vec::new();
        enc.encode(&trace, &mut buf).unwrap();

        let payload = AgentPayload::parse_from_bytes(&buf).unwrap();
        let pb_span = &payload.tracerPayloads[0].chunks[0].spans[0];

        assert_eq!(pb_span.meta.get("http.method").map(|s| s.as_str()), Some("GET"));
        assert_eq!(pb_span.meta.get("error").map(|s| s.as_str()), Some("false"));
        assert_eq!(pb_span.metrics.get("http.status_code").copied(), Some(200.0));
        assert!((pb_span.metrics.get("duration_ms").copied().unwrap_or(0.0) - 3.14).abs() < 1e-9);
    }

    #[tokio::test]
    async fn v1_kind_to_str_all_variants() {
        // Each numeric kind maps to the correct string written into span meta.
        let cases: &[(u32, Option<&str>)] = &[
            (0, None),          // unspecified → tag absent
            (1, Some("server")),
            (2, Some("client")),
            (3, Some("producer")),
            (4, Some("consumer")),
            (5, Some("internal")),
            (99, None),         // unknown → tag absent
        ];

        for &(kind, expected_meta) in cases {
            let mut enc = make_encoder().await;
            let mut span = make_span("svc", "op", "res", 1, 0);
            span.kind = kind;
            let trace = make_trace(vec![span]);
            let mut buf = Vec::new();
            enc.encode(&trace, &mut buf).unwrap();

            let payload = AgentPayload::parse_from_bytes(&buf).unwrap();
            let pb_span = &payload.tracerPayloads[0].chunks[0].spans[0];
            assert_eq!(
                pb_span.meta.get("span.kind").map(|s| s.as_str()),
                expected_meta,
                "kind={} should produce span.kind={:?}",
                kind,
                expected_meta
            );
        }
    }

    #[tokio::test]
    async fn sampling_mechanism_zero_omits_decision_maker_tag() {
        let mut enc = make_encoder().await;
        let mut trace = make_trace(vec![make_span("svc", "op", "res", 1, 0)]);
        trace.chunk.sampling_mechanism = 0;
        let mut buf = Vec::new();
        enc.encode(&trace, &mut buf).unwrap();

        let payload = AgentPayload::parse_from_bytes(&buf).unwrap();
        let chunk = &payload.tracerPayloads[0].chunks[0];
        assert!(
            !chunk.tags.contains_key("_dd.p.dm"),
            "_dd.p.dm tag should be absent when sampling_mechanism=0"
        );
    }

    #[tokio::test]
    async fn empty_optional_trace_fields_produce_no_spurious_output() {
        // A trace with all optional string fields empty should encode without panic and
        // must not write empty-string fields to the tracer payload.
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
        enc.encode(&trace, &mut buf).expect("encode with all-empty fields should succeed");

        let payload = AgentPayload::parse_from_bytes(&buf).expect("should parse");
        let tp = &payload.tracerPayloads[0];
        assert!(tp.containerID.is_empty(), "empty container_id should not be written");
        assert!(tp.languageName.is_empty(), "empty language_name should not be written");
        assert!(tp.tracerVersion.is_empty(), "empty tracer_version should not be written");
        assert!(tp.env.is_empty(), "empty env should not be written");
        assert!(tp.hostname.is_empty(), "empty hostname should not be written");
        assert!(tp.appVersion.is_empty(), "empty app_version should not be written");
    }

    #[tokio::test]
    async fn dropped_trace_flag_is_forwarded() {
        let mut enc = make_encoder().await;
        let mut trace = make_trace(vec![make_span("svc", "op", "res", 1, 0)]);
        trace.chunk.dropped_trace = true;
        let mut buf = Vec::new();
        enc.encode(&trace, &mut buf).unwrap();

        let payload = AgentPayload::parse_from_bytes(&buf).unwrap();
        assert!(payload.tracerPayloads[0].chunks[0].droppedTrace);
    }
}
