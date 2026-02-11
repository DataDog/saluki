#![allow(dead_code)]

use std::time::Duration;

use async_trait::async_trait;
use datadog_protos::traces::{
    attribute_any_value::AttributeAnyValueType, attribute_array_value::AttributeArrayValueType, AttributeAnyValue,
    AttributeArray, AttributeArrayValue, SpanEvent as ProtoSpanEvent, SpanLink as ProtoSpanLink,
};
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use opentelemetry_semantic_conventions::resource::{
    CONTAINER_ID, DEPLOYMENT_ENVIRONMENT_NAME, K8S_POD_UID, SERVICE_VERSION,
};
use protobuf::{rt::WireType, CodedOutputStream, Message};
use saluki_common::task::HandleExt as _;
use saluki_config::GenericConfiguration;
use saluki_context::tags::{SharedTagSet, TagSet};
use saluki_core::data_model::event::trace::{
    AttributeScalarValue, AttributeValue, Span as DdSpan, SpanEvent as DdSpanEvent, SpanLink as DdSpanLink,
};
use saluki_core::topology::{EventsBuffer, PayloadsBuffer};
use saluki_core::{
    components::{encoders::*, ComponentContext},
    data_model::{
        event::{trace::Trace, EventType},
        payload::{HttpPayload, Payload, PayloadMetadata, PayloadType},
    },
    observability::ComponentMetricsExt as _,
};
use saluki_env::host::providers::BoxedHostProvider;
use saluki_env::{EnvironmentProvider, HostProvider};
use saluki_error::generic_error;
use saluki_error::{ErrorContext as _, GenericError};
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
use crate::common::otlp::config::TracesConfig;
use crate::common::otlp::util::{
    extract_container_tags_from_resource_tagset, tags_to_source, Source as OtlpSource, SourceKind as OtlpSourceKind,
    DEPLOYMENT_ENVIRONMENT_KEY, KEY_DATADOG_CONTAINER_ID, KEY_DATADOG_CONTAINER_TAGS, KEY_DATADOG_ENVIRONMENT,
    KEY_DATADOG_HOST, KEY_DATADOG_VERSION,
};

const CONTAINER_TAGS_META_KEY: &str = "_dd.tags.container";
const MAX_TRACES_PER_PAYLOAD: usize = 10000;
static CONTENT_TYPE_PROTOBUF: HeaderValue = HeaderValue::from_static("application/x-protobuf");

// Sampling metadata keys / values.
const TAG_OTLP_SAMPLING_RATE: &str = "_dd.otlp_sr";
const DEFAULT_CHUNK_PRIORITY: i32 = 1; // PRIORITY_AUTO_KEEP

fn default_serializer_compressor_kind() -> String {
    "zstd".to_string()
}

const fn default_zstd_compressor_level() -> i32 {
    3
}

const fn default_flush_timeout_secs() -> u64 {
    2
}

// Field numbers from lib/protos/datadog/proto/datadog-agent/datadog/trace/tracer_payload.proto. This is is used to construct the format of the tracer payload and trace chunk.
const TRACER_PAYLOAD_CONTAINER_ID_FIELD_NUMBER: u32 = 1;
const TRACER_PAYLOAD_LANGUAGE_NAME_FIELD_NUMBER: u32 = 2;
const TRACER_PAYLOAD_TRACER_VERSION_FIELD_NUMBER: u32 = 4;
const TRACER_PAYLOAD_CHUNKS_FIELD_NUMBER: u32 = 6;
const TRACER_PAYLOAD_TAGS_FIELD_NUMBER: u32 = 7;
const TRACER_PAYLOAD_ENV_FIELD_NUMBER: u32 = 8;
const TRACER_PAYLOAD_HOSTNAME_FIELD_NUMBER: u32 = 9;
const TRACER_PAYLOAD_APP_VERSION_FIELD_NUMBER: u32 = 10;

const AGENT_PAYLOAD_HOSTNAME_FIELD_NUMBER: u32 = 1;
const AGENT_PAYLOAD_ENV_FIELD_NUMBER: u32 = 2;
const AGENT_PAYLOAD_TRACER_PAYLOADS_FIELD_NUMBER: u32 = 5;
const AGENT_PAYLOAD_AGENT_VERSION_FIELD_NUMBER: u32 = 7;
const AGENT_PAYLOAD_TARGET_TPS_FIELD_NUMBER: u32 = 8;
const AGENT_PAYLOAD_ERROR_TPS_FIELD_NUMBER: u32 = 9;

// Field numbers from tracer_payload.proto for TraceChunk.
const TRACE_CHUNK_PRIORITY_FIELD_NUMBER: u32 = 1;
const TRACE_CHUNK_SPANS_FIELD_NUMBER: u32 = 3;
const TRACE_CHUNK_TAGS_FIELD_NUMBER: u32 = 4;
const TRACE_CHUNK_DROPPED_TRACE_FIELD_NUMBER: u32 = 5;

// Field numbers from span.proto for Span.
const SPAN_SERVICE_FIELD_NUMBER: u32 = 1;
const SPAN_NAME_FIELD_NUMBER: u32 = 2;
const SPAN_RESOURCE_FIELD_NUMBER: u32 = 3;
const SPAN_TRACE_ID_FIELD_NUMBER: u32 = 4;
const SPAN_SPAN_ID_FIELD_NUMBER: u32 = 5;
const SPAN_PARENT_ID_FIELD_NUMBER: u32 = 6;
const SPAN_START_FIELD_NUMBER: u32 = 7;
const SPAN_DURATION_FIELD_NUMBER: u32 = 8;
const SPAN_ERROR_FIELD_NUMBER: u32 = 9;
const SPAN_META_FIELD_NUMBER: u32 = 10;
const SPAN_METRICS_FIELD_NUMBER: u32 = 11;
const SPAN_TYPE_FIELD_NUMBER: u32 = 12;
const SPAN_META_STRUCT_FIELD_NUMBER: u32 = 13;
const SPAN_SPAN_LINKS_FIELD_NUMBER: u32 = 14;
const SPAN_SPAN_EVENTS_FIELD_NUMBER: u32 = 15;

fn default_env() -> String {
    "none".to_string()
}

/// Configuration for the Datadog Traces encoder.
///
/// This encoder converts trace events into Datadog's TracerPayload protobuf format and sends them
/// to the Datadog traces intake endpoint (`/api/v0.2/traces`). It handles batching, compression,
/// and enrichment with metadata such as hostname, environment, and container tags.
#[derive(Deserialize)]
pub struct DatadogTraceConfiguration {
    #[serde(
        rename = "serializer_compressor_kind",  // renames the field in the user_configuration from "serializer_compressor_kind" to "compressor_kind".
        default = "default_serializer_compressor_kind"
    )]
    compressor_kind: String,

    #[serde(
        rename = "serializer_zstd_compressor_level",
        default = "default_zstd_compressor_level"
    )]
    zstd_compressor_level: i32,

    /// Flush timeout for pending requests, in seconds.
    ///
    /// When the encoder has written traces to the in-flight request payload, but it has not yet reached the
    /// payload size limits that would force the payload to be flushed, the encoder will wait for a period of time
    /// before flushing the in-flight request payload.
    ///
    /// Defaults to 2 seconds.
    #[serde(default = "default_flush_timeout_secs")]
    flush_timeout_secs: u64,

    #[serde(skip)]
    default_hostname: Option<String>,

    #[serde(skip)]
    version: String,

    #[serde(skip)]
    apm_config: ApmConfig,

    #[serde(skip)]
    otlp_traces: TracesConfig,

    #[serde(default = "default_env")]
    env: String,
}

impl DatadogTraceConfiguration {
    /// Creates a new `DatadogTraceConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let mut trace_config: Self = config.as_typed()?;

        let app_details = saluki_metadata::get_app_details();
        trace_config.version = format!("agent-data-plane/{}", app_details.version().raw());

        trace_config.apm_config = ApmConfig::from_configuration(config)?;
        trace_config.otlp_traces = config.try_get_typed("otlp_config.traces")?.unwrap_or_default();

        Ok(trace_config)
    }
}

impl DatadogTraceConfiguration {
    /// Sets the default_hostname using the environment provider
    pub async fn with_environment_provider<E>(mut self, environment_provider: E) -> Result<Self, GenericError>
    where
        E: EnvironmentProvider<Host = BoxedHostProvider>,
    {
        let host_provider = environment_provider.host();
        let hostname = host_provider.get_hostname().await?;
        self.default_hostname = Some(hostname);
        Ok(self)
    }
}

#[async_trait]
impl EncoderBuilder for DatadogTraceConfiguration {
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

        let default_hostname = self.default_hostname.clone().unwrap_or_default();
        let default_hostname = MetaString::from(default_hostname);

        // Create request builder for traces which is used to generate HTTP requests.

        let mut trace_rb = RequestBuilder::new(
            TraceEndpointEncoder::new(
                default_hostname,
                self.version.clone(),
                self.env.clone(),
                self.apm_config.clone(),
                self.otlp_traces.clone(),
            ),
            compression_scheme,
            RB_BUFFER_CHUNK_SIZE,
        )
        .await?;
        trace_rb.with_max_inputs_per_payload(MAX_TRACES_PER_PAYLOAD);

        let flush_timeout = match self.flush_timeout_secs {
            // We always give ourselves a minimum flush timeout of 10ms to allow for some very minimal amount of
            // batching, while still practically flushing things almost immediately.
            0 => Duration::from_millis(10),
            secs => Duration::from_secs(secs),
        };

        Ok(Box::new(DatadogTrace {
            trace_rb,
            telemetry,
            flush_timeout,
        }))
    }
}

impl MemoryBounds for DatadogTraceConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // TODO: How do we properly represent the requests we can generate that may be sitting around in-flight?
        builder
            .minimum()
            .with_single_value::<DatadogTrace>("component struct")
            .with_array::<EventsBuffer>("request builder events channel", 8)
            .with_array::<PayloadsBuffer>("request builder payloads channel", 8);

        builder
            .firm()
            .with_array::<Trace>("traces split re-encode buffer", MAX_TRACES_PER_PAYLOAD);
    }
}

pub struct DatadogTrace {
    trace_rb: RequestBuilder<TraceEndpointEncoder>,
    telemetry: ComponentTelemetry,
    flush_timeout: Duration,
}

// Encodes Trace events to TracerPayloads.
#[async_trait]
impl Encoder for DatadogTrace {
    async fn run(mut self: Box<Self>, mut context: EncoderContext) -> Result<(), GenericError> {
        let Self {
            trace_rb,
            telemetry,
            flush_timeout,
        } = *self;

        let mut health = context.take_health_handle();

        // The encoder runs two async loops, the main encoder loop and the request builder loop,
        // this channel is used to send events from the main encoder loop to the request builder loop safely.
        let (events_tx, events_rx) = mpsc::channel(8);
        // adds a channel to send payloads to the dispatcher and a channel to receive them.
        let (payloads_tx, mut payloads_rx) = mpsc::channel(8);
        let request_builder_fut = run_request_builder(trace_rb, telemetry, events_rx, payloads_tx, flush_timeout);
        // Spawn the request builder task on the global thread pool, this task is responsible for encoding traces and flushing requests.
        let request_builder_handle = context
            .topology_context()
            .global_thread_pool() // Use the shared Tokio runtime thread pool.
            .spawn_traced_named("dd-traces-request-builder", request_builder_fut);

        health.mark_ready();
        debug!("Datadog Trace encoder started.");

        loop {
            select! {
                biased; // makes the branches of the select statement be evaluated in order.

                _ = health.live() => continue,
                maybe_payload = payloads_rx.recv() => match maybe_payload {
                    Some(payload) => {
                        // Dispatch an HTTP payload to the dispatcher.
                        if let Err(e) = context.dispatcher().dispatch(payload).await {
                            error!("Failed to dispatch payload: {}", e);
                        }
                    }
                    None => break,
                },
                maybe_event_buffer = context.events().next() => match maybe_event_buffer {
                    Some(event_buffer) => events_tx.send(event_buffer).await
                        .error_context("Failed to send event buffer to request builder task.")?,
                    None => break,
                },
            }
        }

        // Drop the events sender, which signals the request builder task to stop.
        drop(events_tx);

        // Continue draining the payloads receiver until it is closed.
        while let Some(payload) = payloads_rx.recv().await {
            if let Err(e) = context.dispatcher().dispatch(payload).await {
                error!("Failed to dispatch payload: {}", e);
            }
        }

        // Request build task should now be stopped.
        match request_builder_handle.await {
            Ok(Ok(())) => debug!("Request builder task stopped."),
            Ok(Err(e)) => error!(error = %e, "Request builder task failed."),
            Err(e) => error!(error = %e, "Request builder task panicked."),
        }

        debug!("Datadog Trace encoder stopped.");

        Ok(())
    }
}

async fn run_request_builder(
    mut trace_request_builder: RequestBuilder<TraceEndpointEncoder>, telemetry: ComponentTelemetry,
    mut events_rx: Receiver<EventsBuffer>, payloads_tx: Sender<PayloadsBuffer>, flush_timeout: std::time::Duration,
) -> Result<(), GenericError> {
    let mut pending_flush = false;
    let pending_flush_timeout = sleep(flush_timeout);
    tokio::pin!(pending_flush_timeout);

    loop {
        select! {
            Some(event_buffer) = events_rx.recv() => {
                for event in event_buffer {
                    let trace = match event.try_into_trace() {
                        Some(trace) => trace,
                        None => continue,
                    };
                    // Encode the trace. If we get it back, that means the current request is full, and we need to
                    // flush it before we can try to encode the trace again.
                    let trace_to_retry = match trace_request_builder.encode(trace).await {
                        Ok(None) => continue,
                        Ok(Some(trace)) => trace,
                        Err(e) => {
                            error!(error = %e, "Failed to encode trace.");
                            telemetry.events_dropped_encoder().increment(1);
                            continue;
                        }
                    };

                    let maybe_requests = trace_request_builder.flush().await;
                    if maybe_requests.is_empty() {
                        panic!("builder told us to flush, but gave us nothing");
                    }

                    for maybe_request in maybe_requests {
                        match maybe_request {
                            Ok((events, request)) => {
                                let payload_meta = PayloadMetadata::from_event_count(events);
                                let http_payload = HttpPayload::new(payload_meta, request);
                                let payload = Payload::Http(http_payload);

                                payloads_tx.send(payload).await
                                    .map_err(|_| generic_error!("Failed to send payload to encoder."))?;
                            },
                            Err(e) => if e.is_recoverable() {
                                // If the error is recoverable, we'll hold on to the trace to retry it later.
                                continue;
                            } else {
                                return Err(GenericError::from(e).context("Failed to flush request."));
                            }
                        }
                    }

                    // Now try to encode the trace again.
                    if let Err(e) = trace_request_builder.encode(trace_to_retry).await {
                        error!(error = %e, "Failed to encode trace.");
                        telemetry.events_dropped_encoder().increment(1);
                    }
                }

                debug!("Processed event buffer.");

                // If we're not already pending a flush, we'll start the countdown.
                if !pending_flush {
                    pending_flush_timeout.as_mut().reset(tokio::time::Instant::now() + flush_timeout);
                    pending_flush = true;
                }
            },
            _ = &mut pending_flush_timeout, if pending_flush => {
                debug!("Flushing pending request(s).");

                pending_flush = false;

                // Once we've encoded and written all traces, we flush the request builders to generate a request with
                // anything left over. Again, we'll enqueue those requests to be sent immediately.
                let maybe_trace_requests = trace_request_builder.flush().await;
                for maybe_request in maybe_trace_requests {
                    match maybe_request {
                        Ok((events, request)) => {
                            let payload_meta = PayloadMetadata::from_event_count(events);
                            let http_payload = HttpPayload::new(payload_meta, request);
                            let payload = Payload::Http(http_payload);

                            payloads_tx.send(payload).await
                                .map_err(|_| generic_error!("Failed to send payload to encoder."))?;
                        },
                        Err(e) => if e.is_recoverable() {
                            continue;
                        } else {
                            return Err(GenericError::from(e).context("Failed to flush request."));
                        }
                    }
                }

                debug!("All flushed requests sent to I/O task. Waiting for next event buffer...");
            },

            // Event buffers channel has been closed, and we have no pending flushing, so we're all done.
            else => break,
        }
    }

    Ok(())
}

#[derive(Debug)]
struct TraceEndpointEncoder {
    tracer_payload_scratch: Vec<u8>,
    chunk_scratch: Vec<u8>,
    span_scratch: Vec<u8>,
    inner_scratch: Vec<u8>,
    tags_scratch: Vec<u8>,
    // TODO: do we need additional tags or tag deplicator?
    default_hostname: MetaString,
    agent_hostname: String,
    version: String,
    env: String,
    apm_config: ApmConfig,
    otlp_traces: TracesConfig,
}

impl TraceEndpointEncoder {
    fn new(
        default_hostname: MetaString, version: String, env: String, apm_config: ApmConfig, otlp_traces: TracesConfig,
    ) -> Self {
        Self {
            tracer_payload_scratch: Vec::new(),
            chunk_scratch: Vec::new(),
            span_scratch: Vec::new(),
            inner_scratch: Vec::new(),
            tags_scratch: Vec::new(),
            agent_hostname: default_hostname.as_ref().to_string(),
            default_hostname,
            version,
            env,
            apm_config,
            otlp_traces,
        }
    }

    fn encode_tracer_payload(&mut self, trace: &Trace, output_buffer: &mut Vec<u8>) -> Result<(), protobuf::Error> {
        let sampling_rate = self.sampling_rate();

        // Encode the trace chunk incrementally into chunk_scratch.
        encode_trace_chunk(
            trace,
            sampling_rate,
            &mut self.chunk_scratch,
            &mut self.span_scratch,
            &mut self.inner_scratch,
            &mut self.tags_scratch,
        )?;

        // Encode the tracer payload fields into tracer_payload_scratch,
        // referencing the already-encoded chunk bytes.
        encode_tracer_payload_fields(
            trace,
            self.default_hostname.as_ref(),
            &self.otlp_traces,
            &mut self.tracer_payload_scratch,
            &self.chunk_scratch,
            &mut self.tags_scratch,
        )?;

        // Encode the outer agent payload into the final output buffer,
        // referencing the already-encoded tracer payload bytes.
        encode_agent_payload_fields(
            &self.agent_hostname,
            &self.env,
            &self.version,
            &self.apm_config,
            &self.tracer_payload_scratch,
            output_buffer,
        )
    }

    fn sampling_rate(&self) -> f64 {
        let rate = self.otlp_traces.probabilistic_sampler.sampling_percentage / 100.0;
        if rate <= 0.0 || rate >= 1.0 {
            return 1.0;
        }
        rate
    }
}

impl EndpointEncoder for TraceEndpointEncoder {
    type Input = Trace;
    type EncodeError = protobuf::Error;
    fn encoder_name() -> &'static str {
        "traces"
    }

    fn compressed_size_limit(&self) -> usize {
        DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT
    }

    fn uncompressed_size_limit(&self) -> usize {
        DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT
    }

    fn encode(&mut self, trace: &Self::Input, buffer: &mut Vec<u8>) -> Result<(), Self::EncodeError> {
        self.encode_tracer_payload(trace, buffer)
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
}

/// Encodes the AgentPayload (outer message) fields into `output_buffer`.
///
/// Takes already-encoded tracer payload bytes and wraps them in the agent payload envelope
/// along with agent-level metadata fields.
fn encode_agent_payload_fields(
    agent_hostname: &str, env: &str, version: &str, apm_config: &ApmConfig, tracer_payload_bytes: &[u8],
    output_buffer: &mut Vec<u8>,
) -> Result<(), protobuf::Error> {
    let mut os = CodedOutputStream::vec(output_buffer);

    os.write_string(AGENT_PAYLOAD_HOSTNAME_FIELD_NUMBER, agent_hostname)?;
    os.write_string(AGENT_PAYLOAD_ENV_FIELD_NUMBER, env)?;
    os.write_string(AGENT_PAYLOAD_AGENT_VERSION_FIELD_NUMBER, version)?;
    os.write_double(
        AGENT_PAYLOAD_TARGET_TPS_FIELD_NUMBER,
        apm_config.target_traces_per_second(),
    )?;
    os.write_double(AGENT_PAYLOAD_ERROR_TPS_FIELD_NUMBER, apm_config.errors_per_second())?;

    // Write the pre-encoded TracerPayload as a nested message (repeated field).
    os.write_bytes(AGENT_PAYLOAD_TRACER_PAYLOADS_FIELD_NUMBER, tracer_payload_bytes)?;
    os.flush()?;

    Ok(())
}

/// Encodes all TracerPayload fields into `tracer_payload_scratch`.
///
/// Takes already-encoded trace chunk bytes and writes them as the `chunks` field,
/// along with tracer-level metadata resolved from resource tags.
fn encode_tracer_payload_fields(
    trace: &Trace, default_hostname: &str, otlp_traces: &TracesConfig, tracer_payload_scratch: &mut Vec<u8>,
    chunk_bytes: &[u8], tags_scratch: &mut Vec<u8>,
) -> Result<(), protobuf::Error> {
    let resource_tags = trace.resource_tags();
    let first_span = trace.spans().first();
    let source = tags_to_source(resource_tags);

    tracer_payload_scratch.clear();
    let mut os = CodedOutputStream::vec(tracer_payload_scratch);

    if let Some(container_id) = resolve_container_id(resource_tags, first_span) {
        os.write_string(TRACER_PAYLOAD_CONTAINER_ID_FIELD_NUMBER, container_id)?;
    }

    if let Some(lang) = get_resource_tag_value(resource_tags, "telemetry.sdk.language") {
        os.write_string(TRACER_PAYLOAD_LANGUAGE_NAME_FIELD_NUMBER, lang)?;
    }

    let sdk_version = get_resource_tag_value(resource_tags, "telemetry.sdk.version").unwrap_or("");
    let tracer_version = format!("otlp-{}", sdk_version);
    os.write_string(TRACER_PAYLOAD_TRACER_VERSION_FIELD_NUMBER, &tracer_version)?;

    // Write the pre-encoded TraceChunk as a nested message (repeated field).
    os.write_tag(TRACER_PAYLOAD_CHUNKS_FIELD_NUMBER, WireType::LengthDelimited)?;
    os.write_raw_varint32(chunk_bytes.len() as u32)?;
    os.write_raw_bytes(chunk_bytes)?;

    if let Some(tags) = resolve_container_tags(
        resource_tags,
        source.as_ref(),
        otlp_traces.ignore_missing_datadog_fields,
    ) {
        write_map_entry_string_string(
            &mut os,
            TRACER_PAYLOAD_TAGS_FIELD_NUMBER,
            CONTAINER_TAGS_META_KEY,
            tags.as_ref(),
            tags_scratch,
        )?;
    }

    if let Some(env) = resolve_env(resource_tags, otlp_traces.ignore_missing_datadog_fields) {
        os.write_string(TRACER_PAYLOAD_ENV_FIELD_NUMBER, env)?;
    }

    if let Some(hostname) = resolve_hostname(
        resource_tags,
        source.as_ref(),
        Some(default_hostname),
        otlp_traces.ignore_missing_datadog_fields,
    ) {
        os.write_string(TRACER_PAYLOAD_HOSTNAME_FIELD_NUMBER, hostname)?;
    }

    if let Some(app_version) = resolve_app_version(resource_tags) {
        os.write_string(TRACER_PAYLOAD_APP_VERSION_FIELD_NUMBER, app_version)?;
    }

    os.flush()?;
    Ok(())
}

/// Encodes a TraceChunk incrementally into `chunk_scratch`.
///
/// Writes priority, spans, tags, and droppedTrace fields directly from the trace's
/// sampling metadata and span data, without allocating intermediate protobuf objects.
fn encode_trace_chunk(
    trace: &Trace, sampling_rate: f64, chunk_scratch: &mut Vec<u8>, span_scratch: &mut Vec<u8>,
    inner_scratch: &mut Vec<u8>, tags_scratch: &mut Vec<u8>,
) -> Result<(), protobuf::Error> {
    chunk_scratch.clear();
    let mut os = CodedOutputStream::vec(chunk_scratch);

    let (priority, dropped_trace, decision_maker, otlp_sr) = match trace.sampling() {
        Some(sampling) => (
            sampling.priority.unwrap_or(DEFAULT_CHUNK_PRIORITY),
            sampling.dropped_trace,
            sampling.decision_maker.as_ref().map(|dm| dm.to_string()),
            sampling
                .otlp_sampling_rate
                .as_ref()
                .map(|sr| sr.to_string())
                .unwrap_or_else(|| format!("{:.2}", sampling_rate)),
        ),
        None => (DEFAULT_CHUNK_PRIORITY, false, None, format!("{:.2}", sampling_rate)),
    };

    os.write_int32(TRACE_CHUNK_PRIORITY_FIELD_NUMBER, priority)?;

    // Encode each span incrementally and write as a repeated length-delimited field.
    for span in trace.spans() {
        encode_span(span, span_scratch, inner_scratch, tags_scratch)?;
        os.write_tag(TRACE_CHUNK_SPANS_FIELD_NUMBER, WireType::LengthDelimited)?;
        os.write_raw_varint32(span_scratch.len() as u32)?;
        os.write_raw_bytes(span_scratch)?;
    }

    // Write chunk tags.
    if let Some(dm) = &decision_maker {
        write_map_entry_string_string(
            &mut os,
            TRACE_CHUNK_TAGS_FIELD_NUMBER,
            TAG_DECISION_MAKER,
            dm,
            tags_scratch,
        )?;
    }
    write_map_entry_string_string(
        &mut os,
        TRACE_CHUNK_TAGS_FIELD_NUMBER,
        TAG_OTLP_SAMPLING_RATE,
        &otlp_sr,
        tags_scratch,
    )?;

    if dropped_trace {
        os.write_bool(TRACE_CHUNK_DROPPED_TRACE_FIELD_NUMBER, true)?;
    }

    os.flush()?;
    Ok(())
}

/// Encodes a single span incrementally into `span_scratch`.
///
/// Writes all span fields directly from the `DdSpan` accessors without allocating
/// an intermediate `ProtoSpan`. SpanLinks and SpanEvents are still converted to their
/// protobuf message types since they are infrequent and deeply nested.
fn encode_span(
    span: &DdSpan, span_scratch: &mut Vec<u8>, inner_scratch: &mut Vec<u8>, tags_scratch: &mut Vec<u8>,
) -> Result<(), protobuf::Error> {
    span_scratch.clear();
    let mut os = CodedOutputStream::vec(span_scratch);

    os.write_string(SPAN_SERVICE_FIELD_NUMBER, span.service())?;
    os.write_string(SPAN_NAME_FIELD_NUMBER, span.name())?;
    os.write_string(SPAN_RESOURCE_FIELD_NUMBER, span.resource())?;
    os.write_uint64(SPAN_TRACE_ID_FIELD_NUMBER, span.trace_id())?;
    os.write_uint64(SPAN_SPAN_ID_FIELD_NUMBER, span.span_id())?;
    os.write_uint64(SPAN_PARENT_ID_FIELD_NUMBER, span.parent_id())?;
    os.write_int64(SPAN_START_FIELD_NUMBER, span.start() as i64)?;
    os.write_int64(SPAN_DURATION_FIELD_NUMBER, span.duration() as i64)?;
    os.write_int32(SPAN_ERROR_FIELD_NUMBER, span.error())?;

    // meta: map<string, string>
    for (k, v) in span.meta() {
        write_map_entry_string_string(&mut os, SPAN_META_FIELD_NUMBER, k.as_ref(), v.as_ref(), tags_scratch)?;
    }

    // metrics: map<string, double>
    for (k, v) in span.metrics() {
        write_map_entry_string_double(&mut os, SPAN_METRICS_FIELD_NUMBER, k.as_ref(), *v, tags_scratch)?;
    }

    os.write_string(SPAN_TYPE_FIELD_NUMBER, span.span_type())?;

    // meta_struct: map<string, bytes>
    for (k, v) in span.meta_struct() {
        write_map_entry_string_bytes(&mut os, SPAN_META_STRUCT_FIELD_NUMBER, k.as_ref(), v, tags_scratch)?;
    }

    // SpanLinks and SpanEvents: convert to proto messages and write via scratch buffer.
    for link in span.span_links() {
        let proto_link = convert_span_link(link);
        write_message_field(&mut os, SPAN_SPAN_LINKS_FIELD_NUMBER, &proto_link, inner_scratch)?;
    }

    for event in span.span_events() {
        let proto_event = convert_span_event(event);
        write_message_field(&mut os, SPAN_SPAN_EVENTS_FIELD_NUMBER, &proto_event, inner_scratch)?;
    }

    os.flush()?;
    Ok(())
}

fn convert_span_link(link: &DdSpanLink) -> ProtoSpanLink {
    let mut proto = ProtoSpanLink::new();
    proto.set_traceID(link.trace_id());
    proto.set_traceID_high(link.trace_id_high());
    proto.set_spanID(link.span_id());
    proto.set_attributes(
        link.attributes()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    );
    proto.set_tracestate(link.tracestate().to_string());
    proto.set_flags(link.flags());
    proto
}

fn convert_span_event(event: &DdSpanEvent) -> ProtoSpanEvent {
    let mut proto = ProtoSpanEvent::new();
    proto.set_time_unix_nano(event.time_unix_nano());
    proto.set_name(event.name().to_string());
    proto.set_attributes(
        event
            .attributes()
            .iter()
            .map(|(k, v)| (k.to_string(), convert_attribute_value(v)))
            .collect(),
    );
    proto
}

fn convert_attribute_value(value: &AttributeValue) -> AttributeAnyValue {
    let mut proto = AttributeAnyValue::new();
    match value {
        AttributeValue::String(v) => {
            proto.set_type(AttributeAnyValueType::STRING_VALUE);
            proto.set_string_value(v.to_string());
        }
        AttributeValue::Bool(v) => {
            proto.set_type(AttributeAnyValueType::BOOL_VALUE);
            proto.set_bool_value(*v);
        }
        AttributeValue::Int(v) => {
            proto.set_type(AttributeAnyValueType::INT_VALUE);
            proto.set_int_value(*v);
        }
        AttributeValue::Double(v) => {
            proto.set_type(AttributeAnyValueType::DOUBLE_VALUE);
            proto.set_double_value(*v);
        }
        AttributeValue::Array(values) => {
            proto.set_type(AttributeAnyValueType::ARRAY_VALUE);
            let mut array = AttributeArray::new();
            array.set_values(values.iter().map(convert_attribute_array_value).collect());
            proto.set_array_value(array);
        }
    }
    proto
}

fn convert_attribute_array_value(value: &AttributeScalarValue) -> AttributeArrayValue {
    let mut proto = AttributeArrayValue::new();
    match value {
        AttributeScalarValue::String(v) => {
            proto.set_type(AttributeArrayValueType::STRING_VALUE);
            proto.set_string_value(v.to_string());
        }
        AttributeScalarValue::Bool(v) => {
            proto.set_type(AttributeArrayValueType::BOOL_VALUE);
            proto.set_bool_value(*v);
        }
        AttributeScalarValue::Int(v) => {
            proto.set_type(AttributeArrayValueType::INT_VALUE);
            proto.set_int_value(*v);
        }
        AttributeScalarValue::Double(v) => {
            proto.set_type(AttributeArrayValueType::DOUBLE_VALUE);
            proto.set_double_value(*v);
        }
    }
    proto
}

fn write_message_field<M: Message>(
    output_stream: &mut CodedOutputStream<'_>, field_number: u32, message: &M, scratch_buf: &mut Vec<u8>,
) -> Result<(), protobuf::Error> {
    scratch_buf.clear();
    {
        // In protobuf, length-delimited is one of the wire types, it encodes data as [tag][length][value].
        // We use a nested output stream to write the message to because output_stream requires the size of the message to be known before writing
        // and the Message type size is not known as it depends on the actual data values and lengths and we
        // don't know this until runtime.
        let mut nested = CodedOutputStream::vec(scratch_buf);
        message.write_to(&mut nested)?;
        nested.flush()?;
    }
    output_stream.write_tag(field_number, WireType::LengthDelimited)?;
    output_stream.write_raw_varint32(scratch_buf.len() as u32)?;
    output_stream.write_raw_bytes(scratch_buf)?;
    Ok(())
}

fn write_map_entry_string_string(
    output_stream: &mut CodedOutputStream<'_>, field_number: u32, key: &str, value: &str, scratch_buf: &mut Vec<u8>,
) -> Result<(), protobuf::Error> {
    scratch_buf.clear();
    {
        let mut nested = CodedOutputStream::vec(scratch_buf);
        // the field number 1 and 2 correspond to key and value
        nested.write_string(1, key)?;
        nested.write_string(2, value)?;
        nested.flush()?;
    }
    output_stream.write_tag(field_number, WireType::LengthDelimited)?;
    output_stream.write_raw_varint32(scratch_buf.len() as u32)?;
    output_stream.write_raw_bytes(scratch_buf)?;
    Ok(())
}

fn write_map_entry_string_double(
    output_stream: &mut CodedOutputStream<'_>, field_number: u32, key: &str, value: f64, scratch_buf: &mut Vec<u8>,
) -> Result<(), protobuf::Error> {
    scratch_buf.clear();
    {
        let mut nested = CodedOutputStream::vec(scratch_buf);
        nested.write_string(1, key)?;
        nested.write_double(2, value)?;
        nested.flush()?;
    }
    output_stream.write_tag(field_number, WireType::LengthDelimited)?;
    output_stream.write_raw_varint32(scratch_buf.len() as u32)?;
    output_stream.write_raw_bytes(scratch_buf)?;
    Ok(())
}

fn write_map_entry_string_bytes(
    output_stream: &mut CodedOutputStream<'_>, field_number: u32, key: &str, value: &[u8], scratch_buf: &mut Vec<u8>,
) -> Result<(), protobuf::Error> {
    scratch_buf.clear();
    {
        let mut nested = CodedOutputStream::vec(scratch_buf);
        nested.write_string(1, key)?;
        nested.write_bytes(2, value)?;
        nested.flush()?;
    }
    output_stream.write_tag(field_number, WireType::LengthDelimited)?;
    output_stream.write_raw_varint32(scratch_buf.len() as u32)?;
    output_stream.write_raw_bytes(scratch_buf)?;
    Ok(())
}

fn get_resource_tag_value<'a>(resource_tags: &'a SharedTagSet, key: &str) -> Option<&'a str> {
    resource_tags.get_single_tag(key).and_then(|t| t.value())
}

fn resolve_hostname<'a>(
    resource_tags: &'a SharedTagSet, source: Option<&'a OtlpSource>, default_hostname: Option<&'a str>,
    ignore_missing_fields: bool,
) -> Option<&'a str> {
    let mut hostname = match source {
        Some(src) => match src.kind {
            OtlpSourceKind::HostnameKind => Some(src.identifier.as_str()),
            _ => Some(""),
        },
        None => default_hostname,
    };

    if ignore_missing_fields {
        hostname = Some("");
    }

    if let Some(value) = get_resource_tag_value(resource_tags, KEY_DATADOG_HOST) {
        hostname = Some(value);
    }

    hostname
}

fn resolve_env(resource_tags: &SharedTagSet, ignore_missing_fields: bool) -> Option<&str> {
    if let Some(value) = get_resource_tag_value(resource_tags, KEY_DATADOG_ENVIRONMENT) {
        return Some(value);
    }
    if ignore_missing_fields {
        return None;
    }
    if let Some(value) = get_resource_tag_value(resource_tags, DEPLOYMENT_ENVIRONMENT_NAME) {
        return Some(value);
    }
    get_resource_tag_value(resource_tags, DEPLOYMENT_ENVIRONMENT_KEY)
}

fn resolve_container_id<'a>(resource_tags: &'a SharedTagSet, first_span: Option<&'a DdSpan>) -> Option<&'a str> {
    for key in [KEY_DATADOG_CONTAINER_ID, CONTAINER_ID, K8S_POD_UID] {
        if let Some(value) = get_resource_tag_value(resource_tags, key) {
            return Some(value);
        }
    }
    // TODO: add container id fallback equivalent to cidProvider
    // https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/api/otlp.go#L414
    if let Some(span) = first_span {
        for (k, v) in span.meta() {
            if k == KEY_DATADOG_CONTAINER_ID || k == K8S_POD_UID {
                return Some(v.as_ref());
            }
        }
    }
    None
}

fn resolve_app_version(resource_tags: &SharedTagSet) -> Option<&str> {
    if let Some(value) = get_resource_tag_value(resource_tags, KEY_DATADOG_VERSION) {
        return Some(value);
    }
    get_resource_tag_value(resource_tags, SERVICE_VERSION)
}

fn resolve_container_tags(
    resource_tags: &SharedTagSet, source: Option<&OtlpSource>, ignore_missing_fields: bool,
) -> Option<MetaString> {
    // TODO: some refactoring is probably needed to normalize this function, the tags should already be normalized
    // since we do so when we transform OTLP spans to DD spans however to make this class extensible for non otlp traces, we would
    // need to normalize the tags here.
    if let Some(tags) = get_resource_tag_value(resource_tags, KEY_DATADOG_CONTAINER_TAGS) {
        if !tags.is_empty() {
            return Some(MetaString::from(tags));
        }
    }

    if ignore_missing_fields {
        return None;
    }
    let mut container_tags = TagSet::default();
    extract_container_tags_from_resource_tagset(resource_tags, &mut container_tags);
    let is_fargate_source = source.is_some_and(|src| src.kind == OtlpSourceKind::AwsEcsFargateKind);
    if container_tags.is_empty() && !is_fargate_source {
        return None;
    }

    let mut flattened = flatten_container_tag(container_tags);
    if is_fargate_source {
        if let Some(src) = source {
            append_tags(&mut flattened, &src.tag());
        }
    }

    if flattened.is_empty() {
        None
    } else {
        Some(MetaString::from(flattened))
    }
}

fn flatten_container_tag(tags: TagSet) -> String {
    let mut flattened = String::new();
    for tag in tags {
        if !flattened.is_empty() {
            flattened.push(',');
        }
        flattened.push_str(tag.as_str());
    }
    flattened
}

fn append_tags(target: &mut String, tags: &str) {
    if tags.is_empty() {
        return;
    }
    if !target.is_empty() {
        target.push(',');
    }
    target.push_str(tags);
}
