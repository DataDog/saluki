#![allow(dead_code)]

use async_trait::async_trait;
use bytes::Bytes;
use datadog_protos::traces::{
    attribute_any_value::AttributeAnyValueType, attribute_array_value::AttributeArrayValueType, AttributeAnyValue,
    AttributeArray, AttributeArrayValue, Span as ProtoSpan, SpanEvent as ProtoSpanEvent, SpanLink as ProtoSpanLink,
    TraceChunk,
};
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use opentelemetry_semantic_conventions::resource::{
    CONTAINER_ID, DEPLOYMENT_ENVIRONMENT_NAME, K8S_POD_UID, SERVICE_VERSION,
};
use protobuf::{rt::WireType, CodedOutputStream, Message};
use saluki_config::GenericConfiguration;
use saluki_context::tags::TagSet;
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
use saluki_error::generic_error;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_io::compression::CompressionScheme;
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use stringtheory::MetaString;
use tokio::{select, time::sleep};
use tracing::{debug, error};

use crate::common::datadog::{
    io::RB_BUFFER_CHUNK_SIZE,
    request_builder::{EndpointEncoder, RequestBuilder},
    telemetry::ComponentTelemetry,
    DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT, DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT,
};
use crate::common::otlp::util::{
    extract_container_tags_from_resource_tagset, tags_to_source, Source as OtlpSource, SourceKind as OtlpSourceKind,
    DEPLOYMENT_ENVIRONMENT_KEY, KEY_DATADOG_CONTAINER_ID, KEY_DATADOG_CONTAINER_TAGS, KEY_DATADOG_ENVIRONMENT,
    KEY_DATADOG_HOST, KEY_DATADOG_VERSION,
};

const CONTAINER_TAGS_META_KEY: &str = "_dd.tags.container";
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

// Field numbers from lib/protos/datadog/proto/datadog-agent/datadog/trace/tracer_payload.proto. This is is used to construct the format of the tracer payload and trace chunk.
const TRACER_PAYLOAD_CONTAINER_ID_FIELD_NUMBER: u32 = 1;
const TRACER_PAYLOAD_CHUNKS_FIELD_NUMBER: u32 = 6;
const TRACER_PAYLOAD_TAGS_FIELD_NUMBER: u32 = 7;
const TRACER_PAYLOAD_ENV_FIELD_NUMBER: u32 = 8;
const TRACER_PAYLOAD_HOSTNAME_FIELD_NUMBER: u32 = 9;
const TRACER_PAYLOAD_APP_VERSION_FIELD_NUMBER: u32 = 10;

fn default_ignore_missing_datadog_fields() -> bool {
    false
}

#[derive(Deserialize, Debug)]
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

    #[serde(default = "default_ignore_missing_datadog_fields")]
    ignore_missing_datadog_fields: bool,
}

impl DatadogTraceConfiguration {
    /// Creates a new `DatadogTraceConfiguration` from the given configuration.
    pub fn from_configuration(user_configuration: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(user_configuration.as_typed()?)  // The user configuration is deserialized into a DatadogTraceConfiguration and fields override the defaults of DatadogTraceConfiguration.
    }
}

impl DatadogTraceConfiguration {
    /// Sets the default hostname to use when traces don't have one.
    // TODO: call this in the function add_otlp_to_blueprint
    pub fn with_default_hostname(mut self, hostname: String) -> Self {
        self.default_hostname = Some(hostname);
        self
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

        let default_hostname = MetaString::from(self.default_hostname.clone().unwrap_or_default());

        // Create request builder for traces which is used to generate HTTP requests.
        let mut trace_rb = RequestBuilder::new(
            TraceEndpointEncoder::new(default_hostname, self.ignore_missing_datadog_fields),
            compression_scheme,
            RB_BUFFER_CHUNK_SIZE,
        )
        .await?;
        trace_rb.with_max_inputs_per_payload(MAX_TRACES_PER_PAYLOAD);

        let flush_timeout = match self.flush_timeout_secs {
            // Give ourselves a minimum flush timeout of 10ms to allow for minimal batching
            0 => std::time::Duration::from_millis(10),
            secs => std::time::Duration::from_secs(secs),
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
    flush_timeout: std::time::Duration,
}

// Encodes Trace events to TracerPayloads.
#[async_trait]
impl Encoder for DatadogTrace {
    async fn run(mut self: Box<Self>, mut context: EncoderContext) -> Result<(), GenericError> {
        use saluki_common::task::HandleExt as _;
        use tokio::{select, sync::mpsc};
        use tracing::debug;

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
    mut events_rx: tokio::sync::mpsc::Receiver<saluki_core::topology::EventsBuffer>,
    payloads_tx: tokio::sync::mpsc::Sender<saluki_core::topology::PayloadsBuffer>, flush_timeout: std::time::Duration,
) -> Result<(), GenericError> {
    let mut pending_flush = false;
    let pending_flush_timeout = sleep(flush_timeout);
    tokio::pin!(pending_flush_timeout);

    loop {
        select! {
            Some(event_buffer) = events_rx.recv() => {
                for event in event_buffer {
                    let trace = match event {
                        saluki_core::data_model::event::Event::Trace(trace) => trace,
                        _ => continue,
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
    scratch_buf: Vec<u8>,
    // TODO: do we need additional tags or tag deplicator?
    default_hostname: MetaString,
    ignore_missing_datadog_fields: bool,
}

impl TraceEndpointEncoder {
    fn new(default_hostname: MetaString, ignore_missing_datadog_fields: bool) -> Self {
        Self {
            scratch_buf: Vec::new(),
            default_hostname,
            ignore_missing_datadog_fields,
        }
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
        // TODO: add stats https://github.com/DataDog/datadog-agent/blob/test-journal-payloads-to-disk/pkg/trace/api/otlp.go#L277-L294
        encode_tracer_payload(
            trace,
            &self.default_hostname,
            self.ignore_missing_datadog_fields,
            buffer,
            &mut self.scratch_buf,
        )?;

        Ok(())
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

fn build_trace_chunk(trace: &Trace) -> TraceChunk {
    let spans: Vec<ProtoSpan> = trace.spans().into_iter().map(convert_span).collect();
    let mut chunk = TraceChunk::new();
    chunk.set_spans(spans);
    // TODO: add sampling https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/api/otlp.go#L550
    chunk
}

fn convert_span(span: &DdSpan) -> ProtoSpan {
    let mut proto = ProtoSpan::new();
    proto.set_service(span.service().to_string().into());
    proto.set_name(span.name().to_string().into());
    proto.set_resource(span.resource().to_string().into());
    proto.set_traceID(span.trace_id());
    proto.set_spanID(span.span_id());
    proto.set_parentID(span.parent_id());
    proto.set_start(span.start());
    proto.set_duration(span.duration());
    proto.set_error(span.error());
    proto.set_type(span.span_type().to_string().into());

    proto.set_meta(
        span.meta()
            .iter()
            .map(|(k, v)| (k.to_string().into(), v.to_string().into()))
            .collect(),
    );
    proto.set_metrics(span.metrics().iter().map(|(k, v)| (k.to_string().into(), *v)).collect());
    proto.set_meta_struct(
        span.meta_struct()
            .iter()
            .map(|(k, v)| (k.to_string().into(), Bytes::from(v.clone())))
            .collect(),
    );
    proto.set_spanLinks(span.span_links().iter().map(convert_span_link).collect());
    proto.set_spanEvents(span.span_events().iter().map(convert_span_event).collect());
    proto
}

fn convert_span_link(link: &DdSpanLink) -> ProtoSpanLink {
    let mut proto = ProtoSpanLink::new();
    proto.set_traceID(link.trace_id());
    proto.set_traceID_high(link.trace_id_high());
    proto.set_spanID(link.span_id());
    proto.set_attributes(
        link.attributes()
            .iter()
            .map(|(k, v)| (k.to_string().into(), v.to_string().into()))
            .collect(),
    );
    proto.set_tracestate(link.tracestate().to_string().into());
    proto.set_flags(link.flags());
    proto
}

fn convert_span_event(event: &DdSpanEvent) -> ProtoSpanEvent {
    let mut proto = ProtoSpanEvent::new();
    proto.set_time_unix_nano(event.time_unix_nano());
    proto.set_name(event.name().to_string().into());
    proto.set_attributes(
        event
            .attributes()
            .iter()
            .map(|(k, v)| (k.to_string().into(), convert_attribute_value(v)))
            .collect(),
    );
    proto
}

fn convert_attribute_value(value: &AttributeValue) -> AttributeAnyValue {
    let mut proto = AttributeAnyValue::new();
    match value {
        AttributeValue::String(v) => {
            proto.set_type(AttributeAnyValueType::STRING_VALUE);
            proto.set_string_value(v.to_string().into());
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
            proto.set_string_value(v.to_string().into());
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

fn encode_tracer_payload(
    trace: &Trace, default_hostname: &MetaString, ignore_missing_fields: bool, output_buffer: &mut Vec<u8>,
    scratch_buf: &mut Vec<u8>,
) -> Result<(), protobuf::Error> {
    let resource_tags = trace.resource_tags();
    let first_span = trace.spans().first().cloned();
    let source = tags_to_source(resource_tags);
    let mut output_stream = CodedOutputStream::vec(output_buffer);
    if let Some(container_id) = resolve_container_id(resource_tags, first_span) {
        output_stream.write_string(
            TRACER_PAYLOAD_CONTAINER_ID_FIELD_NUMBER,
            container_id.to_string().as_str(),
        )?;
    }
    // TODO: add language name, language version and tracer version from stats
    // https://github.com/DataDog/datadog-agent/blob/redo_instrument_otlp_traffic/pkg/trace/api/otlp.go#L496-L498

    // write a chunk of the trace, we write each trace as a chunk and multiple chunks are allowed in the tracer payload, they get appended
    // to the payload before it flushes. The code `repeated TraceChunk chunks = 6;` in the tracer_payload.proto allows us to repeatedly write chunks that append
    // instead of overriding the previous chunk. The TraceChunk implements ::protobuf::Message so we can pass it in as a Message type to the write_message_field function.
    write_message_field(
        &mut output_stream,
        TRACER_PAYLOAD_CHUNKS_FIELD_NUMBER,
        &build_trace_chunk(trace),
        scratch_buf,
    )?;

    if let Some(tags) = resolve_container_tags(resource_tags, source.as_ref(), ignore_missing_fields) {
        write_map_entry_string_string(
            &mut output_stream,
            TRACER_PAYLOAD_TAGS_FIELD_NUMBER,
            CONTAINER_TAGS_META_KEY,
            tags.to_string().as_str(),
            scratch_buf,
        )?;
    }

    if let Some(env) = resolve_env(resource_tags, ignore_missing_fields) {
        output_stream.write_string(TRACER_PAYLOAD_ENV_FIELD_NUMBER, env.to_string().as_str())?;
    }

    if let Some(hostname) =
        resolve_hostname(resource_tags, source.as_ref(), Some(default_hostname), ignore_missing_fields)
    {
        output_stream.write_string(TRACER_PAYLOAD_HOSTNAME_FIELD_NUMBER, hostname.to_string().as_str())?;
    }

    if let Some(app_version) = resolve_app_version(resource_tags) {
        output_stream.write_string(
            TRACER_PAYLOAD_APP_VERSION_FIELD_NUMBER,
            app_version.to_string().as_str(),
        )?;
    }

    Ok(())
}

fn get_resource_tag_value<'a>(resource_tags: &'a TagSet, key: &str) -> Option<&'a str> {
    resource_tags.get_single_tag(key).and_then(|t| t.value())
}

fn resolve_hostname(
    resource_tags: &TagSet, source: Option<&OtlpSource>, default_hostname: Option<&MetaString>,
    ignore_missing_fields: bool,
) -> Option<MetaString> {
    let mut hostname = match source {
        Some(src) => match src.kind {
            OtlpSourceKind::HostnameKind => Some(MetaString::from(src.identifier.as_str())),
            _ => Some(MetaString::empty()),
        },
        None => default_hostname.cloned(),
    };

    if ignore_missing_fields {
        hostname = Some(MetaString::empty());
    }

    if let Some(value) = get_resource_tag_value(resource_tags, KEY_DATADOG_HOST) {
        hostname = Some(MetaString::from(value));
    }

    hostname
}

fn resolve_env(resource_tags: &TagSet, ignore_missing_fields: bool) -> Option<MetaString> {
    if let Some(value) = get_resource_tag_value(resource_tags, KEY_DATADOG_ENVIRONMENT) {
        return Some(MetaString::from(value));
    }
    if ignore_missing_fields {
        return None;
    }
    if let Some(value) = get_resource_tag_value(resource_tags, DEPLOYMENT_ENVIRONMENT_NAME) {
        return Some(MetaString::from(value));
    }
    get_resource_tag_value(resource_tags, DEPLOYMENT_ENVIRONMENT_KEY).map(MetaString::from)
}

fn resolve_container_id(resource_tags: &TagSet, first_span: Option<DdSpan>) -> Option<MetaString> {
    for key in [KEY_DATADOG_CONTAINER_ID, CONTAINER_ID, K8S_POD_UID] {
        if let Some(value) = get_resource_tag_value(resource_tags, key) {
            return Some(MetaString::from(value));
        }
    }
    // TODO: add container id fallback equivalent to cidProvider
    // https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/api/otlp.go#L414
    if let Some(span) = first_span {
        for (k, v) in span.meta() {
            if k == KEY_DATADOG_CONTAINER_ID || k == K8S_POD_UID {
                return Some(v.clone());
            }
        }
    }
    None
}

fn resolve_app_version(resource_tags: &TagSet) -> Option<MetaString> {
    if let Some(value) = get_resource_tag_value(resource_tags, KEY_DATADOG_VERSION) {
        return Some(MetaString::from(value));
    }
    get_resource_tag_value(resource_tags, SERVICE_VERSION).map(MetaString::from)
}

fn resolve_container_tags(
    resource_tags: &TagSet, source: Option<&OtlpSource>, ignore_missing_fields: bool,
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
