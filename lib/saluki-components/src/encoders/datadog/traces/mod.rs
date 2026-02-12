#![allow(dead_code)]

use std::time::Duration;

use async_trait::async_trait;
use datadog_protos::traces::builders::{
    attribute_any_value::AttributeAnyValueType, attribute_array_value::AttributeArrayValueType, AgentPayloadBuilder,
    AttributeAnyValueBuilder, AttributeArrayValueBuilder,
};
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use opentelemetry_semantic_conventions::resource::{
    CONTAINER_ID, DEPLOYMENT_ENVIRONMENT_NAME, K8S_POD_UID, SERVICE_VERSION,
};
use piecemeal::{ScratchBuffer, ScratchWriter};
use saluki_common::task::HandleExt as _;
use saluki_config::GenericConfiguration;
use saluki_context::tags::{SharedTagSet, TagSet};
use saluki_core::data_model::event::trace::{AttributeScalarValue, AttributeValue, Span as DdSpan};
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
    scratch: ScratchWriter<Vec<u8>>,
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
            scratch: ScratchWriter::new(Vec::with_capacity(8192)),
            agent_hostname: default_hostname.as_ref().to_string(),
            default_hostname,
            version,
            env,
            apm_config,
            otlp_traces,
        }
    }

    fn encode_tracer_payload(&mut self, trace: &Trace, output_buffer: &mut Vec<u8>) -> std::io::Result<()> {
        let sampling_rate = self.sampling_rate();
        let resource_tags = trace.resource_tags();
        let first_span = trace.spans().first();
        let source = tags_to_source(resource_tags);

        // Resolve metadata from resource tags.
        let container_id = resolve_container_id(resource_tags, first_span);
        let lang = get_resource_tag_value(resource_tags, "telemetry.sdk.language");
        let sdk_version = get_resource_tag_value(resource_tags, "telemetry.sdk.version").unwrap_or("");
        let tracer_version = format!("otlp-{}", sdk_version);
        let container_tags = resolve_container_tags(
            resource_tags,
            source.as_ref(),
            self.otlp_traces.ignore_missing_datadog_fields,
        );
        let env = resolve_env(resource_tags, self.otlp_traces.ignore_missing_datadog_fields);
        let hostname = resolve_hostname(
            resource_tags,
            source.as_ref(),
            Some(self.default_hostname.as_ref()),
            self.otlp_traces.ignore_missing_datadog_fields,
        );
        let app_version = resolve_app_version(resource_tags);

        // Resolve sampling metadata.
        let (priority, dropped_trace, decision_maker, otlp_sr) = match trace.sampling() {
            Some(sampling) => (
                sampling.priority.unwrap_or(DEFAULT_CHUNK_PRIORITY),
                sampling.dropped_trace,
                sampling.decision_maker.as_deref(),
                sampling
                    .otlp_sampling_rate
                    .as_ref()
                    .map(|sr| sr.to_string())
                    .unwrap_or_else(|| format!("{:.2}", sampling_rate)),
            ),
            None => (DEFAULT_CHUNK_PRIORITY, false, None, format!("{:.2}", sampling_rate)),
        };

        // Now incrementally build the payload.
        let mut ap_builder = AgentPayloadBuilder::new(&mut self.scratch);

        ap_builder
            .host_name(&self.agent_hostname)?
            .env(&self.env)?
            .agent_version(&self.version)?
            .target_tps(self.apm_config.target_traces_per_second())?
            .error_tps(self.apm_config.errors_per_second())?;

        ap_builder.add_tracer_payloads(|tp| {
            if let Some(cid) = container_id {
                tp.container_id(cid)?;
            }
            if let Some(l) = lang {
                tp.language_name(l)?;
            }
            tp.tracer_version(&tracer_version)?;

            // Encode the single TraceChunk containing all spans.
            tp.add_chunks(|chunk| {
                chunk.priority(priority)?;

                for span in trace.spans() {
                    chunk.add_spans(|s| {
                        s.service(span.service())?
                            .name(span.name())?
                            .resource(span.resource())?
                            .trace_id(span.trace_id())?
                            .span_id(span.span_id())?
                            .parent_id(span.parent_id())?
                            .start(span.start() as i64)?
                            .duration(span.duration() as i64)?
                            .error(span.error())?;

                        {
                            let mut meta = s.meta();
                            for (k, v) in span.meta() {
                                meta.write_entry(k.as_ref(), v.as_ref())?;
                            }
                        }

                        {
                            let mut metrics = s.metrics();
                            for (k, v) in span.metrics() {
                                metrics.write_entry(k.as_ref(), *v)?;
                            }
                        }

                        s.type_(span.span_type())?;

                        {
                            let mut ms = s.meta_struct();
                            for (k, v) in span.meta_struct() {
                                ms.write_entry(k.as_ref(), v.as_slice())?;
                            }
                        }

                        for link in span.span_links() {
                            s.add_span_links(|sl| {
                                sl.trace_id(link.trace_id())?
                                    .trace_id_high(link.trace_id_high())?
                                    .span_id(link.span_id())?;
                                {
                                    let mut attrs = sl.attributes();
                                    for (k, v) in link.attributes() {
                                        attrs.write_entry(&**k, &**v)?;
                                    }
                                }
                                let tracestate = link.tracestate().to_string();
                                sl.tracestate(tracestate.as_str())?.flags(link.flags())?;
                                Ok(())
                            })?;
                        }

                        for event in span.span_events() {
                            s.add_span_events(|se| {
                                se.time_unix_nano(event.time_unix_nano())?.name(event.name())?;
                                {
                                    let mut attrs = se.attributes();
                                    for (k, v) in event.attributes() {
                                        attrs.write_entry(&**k, |av| encode_attribute_value(av, v))?;
                                    }
                                }
                                Ok(())
                            })?;
                        }

                        Ok(())
                    })?;
                }

                // Chunk tags.
                {
                    let mut tags = chunk.tags();
                    if let Some(dm) = decision_maker {
                        tags.write_entry(TAG_DECISION_MAKER, dm)?;
                    }
                    tags.write_entry(TAG_OTLP_SAMPLING_RATE, otlp_sr.as_str())?;
                }

                if dropped_trace {
                    chunk.dropped_trace(true)?;
                }

                Ok(())
            })?;

            // Tracer payload tags.
            if let Some(ct) = container_tags {
                let mut tags = tp.tags();
                tags.write_entry(CONTAINER_TAGS_META_KEY, &*ct)?;
            }

            if let Some(e) = env {
                tp.env(e)?;
            }
            if let Some(h) = hostname {
                tp.hostname(h)?;
            }
            if let Some(av) = app_version {
                tp.app_version(av)?;
            }

            Ok(())
        })?;

        ap_builder.finish(output_buffer)?;

        Ok(())
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
    type EncodeError = std::io::Error;
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

fn encode_attribute_value<S: ScratchBuffer>(
    builder: &mut AttributeAnyValueBuilder<'_, S>, value: &AttributeValue,
) -> std::io::Result<()> {
    match value {
        AttributeValue::String(v) => {
            builder.type_(AttributeAnyValueType::STRING_VALUE)?.string_value(v)?;
        }
        AttributeValue::Bool(v) => {
            builder.type_(AttributeAnyValueType::BOOL_VALUE)?.bool_value(*v)?;
        }
        AttributeValue::Int(v) => {
            builder.type_(AttributeAnyValueType::INT_VALUE)?.int_value(*v)?;
        }
        AttributeValue::Double(v) => {
            builder.type_(AttributeAnyValueType::DOUBLE_VALUE)?.double_value(*v)?;
        }
        AttributeValue::Array(values) => {
            builder.type_(AttributeAnyValueType::ARRAY_VALUE)?.array_value(|arr| {
                for val in values {
                    arr.add_values(|av| encode_attribute_array_value(av, val))?;
                }
                Ok(())
            })?;
        }
    }
    Ok(())
}

fn encode_attribute_array_value<S: ScratchBuffer>(
    builder: &mut AttributeArrayValueBuilder<'_, S>, value: &AttributeScalarValue,
) -> std::io::Result<()> {
    match value {
        AttributeScalarValue::String(v) => {
            builder.type_(AttributeArrayValueType::STRING_VALUE)?.string_value(v)?;
        }
        AttributeScalarValue::Bool(v) => {
            builder.type_(AttributeArrayValueType::BOOL_VALUE)?.bool_value(*v)?;
        }
        AttributeScalarValue::Int(v) => {
            builder.type_(AttributeArrayValueType::INT_VALUE)?.int_value(*v)?;
        }
        AttributeScalarValue::Double(v) => {
            builder.type_(AttributeArrayValueType::DOUBLE_VALUE)?.double_value(*v)?;
        }
    }
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
