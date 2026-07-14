#![allow(dead_code)]

use std::{fmt::Write, time::Duration};

use async_trait::async_trait;
use datadog_protos::traces::builders::{idx::SpanKind, AgentPayloadBuilder};
use facet::Facet;
use http::{uri::PathAndQuery, HeaderName, HeaderValue, Method, Uri};
use piecemeal::{ScratchBuffer, ScratchWriter};
use saluki_common::collections::{FastHashMap, FastIndexSet};
use saluki_common::strings::StringBuilder;
use saluki_common::task::HandleExt as _;
use saluki_config::GenericConfiguration;
use saluki_context::tags::TagSet;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::data_model::event::trace::AttributeValue;
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
use tokio::pin;
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
    attributes_to_source, extract_container_tags_from_attributes_map, Source as OtlpSource,
    SourceKind as OtlpSourceKind, KEY_DATADOG_CONTAINER_TAGS,
};

const CONTAINER_TAGS_META_KEY: &str = "_dd.tags.container";
const MAX_TRACES_PER_PAYLOAD: usize = 10000;
static CONTENT_TYPE_PROTOBUF: HeaderValue = HeaderValue::from_static("application/x-protobuf");

// Sampling metadata keys / values.
const TAG_OTLP_SAMPLING_RATE: &str = "_dd.otlp_sr";
const DEFAULT_CHUNK_PRIORITY: i32 = 1; // PRIORITY_AUTO_KEEP

// ETS chunk-level attribute keys / values.
const TAG_ETS_STANDALONE_ERROR_KEY: &str = "_dd.error_tracking_standalone.error";
const TAG_ETS_STANDALONE_ERROR_VALUE: &str = "true";

/// String interning table for the `idx` tracer payload format.
///
/// Index 0 is always the empty string. All other strings are assigned indices in insertion order.
#[derive(Debug)]
struct StringTable {
    indices: FastIndexSet<MetaString>,
}

impl Default for StringTable {
    fn default() -> Self {
        Self::new()
    }
}

impl StringTable {
    fn new() -> Self {
        let mut t = Self {
            indices: FastIndexSet::default(),
        };
        t.intern("");
        t
    }

    fn clear(&mut self) {
        self.indices.clear();
        self.intern("");
    }

    /// Interns `s`, returning its index. If `s` was already interned, returns the existing index.
    fn intern(&mut self, s: &str) -> u32 {
        // We use get_index_of to check if the string is already interned to avoid reconstructing a new meta string if it's already interned.
        if let Some(idx) = self.indices.get_index_of(s) {
            idx as u32
        } else {
            let (idx, _) = self.indices.insert_full(MetaString::from(s));
            idx as u32
        }
    }
}

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
#[derive(Deserialize, Facet)]
#[cfg_attr(test, derive(Debug, PartialEq, serde::Serialize))]
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
    /// When the encoder has written traces to the in-flight request payload, but it hasn't yet reached the
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
    #[facet(opaque)]
    apm_config: ApmConfig,

    #[serde(skip)]
    #[facet(opaque)]
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
    /// Sets the `default_hostname` using the environment provider
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
    pin!(pending_flush_timeout);

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
                            Ok((events, _data_points, request)) => {
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
                        Ok((events, _data_points, request)) => {
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
    string_builder: StringBuilder,
    string_table: StringTable,
    error_tracking_standalone: bool,
    extra_headers: Vec<(HeaderName, HeaderValue)>,
}

impl TraceEndpointEncoder {
    fn new(
        default_hostname: MetaString, version: String, env: String, apm_config: ApmConfig, otlp_traces: TracesConfig,
    ) -> Self {
        let error_tracking_standalone = apm_config.error_tracking_standalone_enabled();
        let extra_headers = if error_tracking_standalone {
            vec![(
                HeaderName::from_static("x-datadog-error-tracking-standalone"),
                HeaderValue::from_static("true"),
            )]
        } else {
            Vec::new()
        };
        Self {
            scratch: ScratchWriter::new(Vec::with_capacity(8192)),
            agent_hostname: default_hostname.as_ref().to_string(),
            default_hostname,
            version,
            env,
            apm_config,
            otlp_traces,
            string_builder: StringBuilder::new(),
            string_table: StringTable::new(),
            error_tracking_standalone,
            extra_headers,
        }
    }

    fn encode_tracer_payload(&mut self, trace: &Trace, output_buffer: &mut Vec<u8>) -> std::io::Result<()> {
        let sampling_rate = self.sampling_rate();
        let source = attributes_to_source(&trace.attributes);

        // Resolve computed metadata strings (may produce strings not directly present on trace fields).
        let tracer_version = format!("otlp-{}", &trace.payload.tracer_version);
        let container_tags = resolve_container_tags_from_attrs(
            &trace.attributes,
            source.as_ref(),
            self.otlp_traces.ignore_missing_datadog_fields,
        );
        let env_str: Option<&str> = if !trace.payload.env.is_empty() {
            Some(&trace.payload.env)
        } else if self.otlp_traces.ignore_missing_datadog_fields {
            Some("")
        } else {
            None
        };
        // Clone to avoid a lifetime conflict: hostname_str borrows from this copy, and
        // self.string_table is mutably borrowed inside the encoding closure below.
        let default_hostname_copy = self.default_hostname.as_ref().to_string();
        let hostname_str: Option<&str> = resolve_hostname_from_payload(
            &trace.payload.hostname,
            source.as_ref(),
            Some(default_hostname_copy.as_str()),
            self.otlp_traces.ignore_missing_datadog_fields,
        );
        let decision_maker = trace.decision_maker.as_deref();
        let priority = trace.priority.unwrap_or(DEFAULT_CHUNK_PRIORITY);
        let dropped_trace = trace.dropped_trace;
        let otlp_sr = trace.otlp_sampling_rate.unwrap_or(sampling_rate);
        self.string_builder.clear();
        write!(&mut self.string_builder, "{:.2}", otlp_sr).expect("should never fail to format sampling rate");

        // Build 128-bit big-endian trace ID bytes for the chunk.
        let mut trace_id_bytes = [0u8; 16];
        trace_id_bytes[..8].copy_from_slice(&trace.trace_id_high.to_be_bytes());
        trace_id_bytes[8..].copy_from_slice(&trace.trace_id_low.to_be_bytes());

        // Reset the string table; strings are interned on the fly during encoding below.
        self.string_table.clear();

        let mut ap_builder = AgentPayloadBuilder::new(&mut self.scratch);

        ap_builder
            .host_name(&self.agent_hostname)?
            .env(&self.env)?
            .agent_version(&self.version)?
            .target_tps(self.apm_config.target_traces_per_second())?
            .error_tps(self.apm_config.errors_per_second())?;

        ap_builder.add_idx_tracer_payloads(|tp| {
            // Tracer payload metadata refs (skip default/empty values).
            // Strings are interned on the fly; the string table is written at the end of this
            // closure so it is complete by the time tp.strings() is called.
            if !trace.payload.container_id.is_empty() {
                tp.container_id_ref(self.string_table.intern(&trace.payload.container_id))?;
            }
            if !trace.payload.language_name.is_empty() {
                tp.language_name_ref(self.string_table.intern(&trace.payload.language_name))?;
            }
            if !trace.payload.language_version.is_empty() {
                tp.language_version_ref(self.string_table.intern(&trace.payload.language_version))?;
            }
            tp.tracer_version_ref(self.string_table.intern(tracer_version.as_str()))?;
            if !trace.payload.runtime_id.is_empty() {
                tp.runtime_id_ref(self.string_table.intern(&trace.payload.runtime_id))?;
            }
            if let Some(e) = env_str {
                tp.env_ref(self.string_table.intern(e))?;
            }
            if let Some(h) = hostname_str {
                tp.hostname_ref(self.string_table.intern(h))?;
            }
            if !trace.payload.app_version.is_empty() {
                tp.app_version_ref(self.string_table.intern(&trace.payload.app_version))?;
            }

            // Container tags go in the payload-level attributes map.
            if let Some(ct) = &container_tags {
                let k_ref = self.string_table.intern(CONTAINER_TAGS_META_KEY);
                let v_ref = self.string_table.intern(ct);
                tp.attributes().write_entry(k_ref, |av: &mut _| {
                    av.value(|vo| vo.string_value_ref(v_ref)).map(|_| ())
                })?;
            }

            // Single TraceChunk containing all spans.
            tp.add_chunks(|chunk| {
                chunk.priority(priority)?;

                if !trace.origin.is_empty() {
                    chunk.origin_ref(self.string_table.intern(&trace.origin))?;
                }

                // Write 128-bit trace ID.
                chunk.trace_id(&trace_id_bytes)?;

                // Sampling mechanism (only write when non-zero).
                if trace.sampling_mechanism != 0 {
                    chunk.sampling_mechanism(trace.sampling_mechanism)?;
                }

                // Spans.
                for span in trace.spans() {
                    let service_ref = self.string_table.intern(span.service());
                    let name_ref = self.string_table.intern(span.name());
                    let resource_ref = self.string_table.intern(span.resource());
                    let type_ref = self.string_table.intern(span.span_type());
                    let env_ref = (!span.env.is_empty()).then(|| self.string_table.intern(&span.env));
                    let version_ref = (!span.version.is_empty()).then(|| self.string_table.intern(&span.version));
                    let component_ref = (!span.component.is_empty()).then(|| self.string_table.intern(&span.component));

                    chunk.add_spans(|s| {
                        s.service_ref(service_ref)?
                            .name_ref(name_ref)?
                            .resource_ref(resource_ref)?
                            .span_id(span.span_id())?
                            .parent_id(span.parent_id())?
                            .start(span.start())?
                            .duration(span.duration())?
                            .error(span.error() != 0)?;

                        // Unified attribute map (replaces separate meta/metrics/meta_struct).
                        {
                            let mut attrs = s.attributes();
                            for (k, v) in &span.attributes {
                                let k_ref = self.string_table.intern(k);
                                attrs.write_entry(k_ref, |av: &mut _| {
                                    encode_idx_attribute_value(av, v, &mut self.string_table)
                                })?;
                            }
                        }

                        s.type_ref(type_ref)?;

                        if let Some(er) = env_ref {
                            s.env_ref(er)?;
                        }
                        if let Some(vr) = version_ref {
                            s.version_ref(vr)?;
                        }
                        if let Some(cr) = component_ref {
                            s.component_ref(cr)?;
                        }
                        if span.kind != 0 {
                            s.kind(SpanKind::from(span.kind as i32))?;
                        }

                        // Span links.
                        for link in span.span_links() {
                            let mut link_trace_id_bytes = [0u8; 16];
                            link_trace_id_bytes[..8].copy_from_slice(&link.trace_id_high().to_be_bytes());
                            link_trace_id_bytes[8..].copy_from_slice(&link.trace_id().to_be_bytes());
                            let tracestate_ref = self.string_table.intern(link.tracestate());

                            s.add_links(|sl| {
                                sl.trace_id(&link_trace_id_bytes)?.span_id(link.span_id())?;
                                {
                                    let mut lattrs = sl.attributes();
                                    for (k, v) in link.attributes() {
                                        let k_ref = self.string_table.intern(k);
                                        lattrs.write_entry(k_ref, |av: &mut _| {
                                            encode_idx_attribute_value(av, v, &mut self.string_table)
                                        })?;
                                    }
                                }
                                sl.tracestate_ref(tracestate_ref)?.flags(link.flags())?;
                                Ok(())
                            })?;
                        }

                        // Span events.
                        for event in span.span_events() {
                            let name_ref = self.string_table.intern(event.name());
                            s.add_events(|se| {
                                se.time(event.time_unix_nano())?.name_ref(name_ref)?;
                                {
                                    let mut eattrs = se.attributes();
                                    for (k, v) in event.attributes() {
                                        let k_ref = self.string_table.intern(k);
                                        eattrs.write_entry(k_ref, |av: &mut _| {
                                            encode_idx_attribute_value(av, v, &mut self.string_table)
                                        })?;
                                    }
                                }
                                Ok(())
                            })?;
                        }

                        Ok(())
                    })?;
                }

                // Chunk attributes: decision maker, ETS tag, OTLP sampling rate.
                {
                    let mut cattrs = chunk.attributes();
                    if let Some(dm) = decision_maker {
                        let k_ref = self.string_table.intern(TAG_DECISION_MAKER);
                        let v_ref = self.string_table.intern(dm);
                        cattrs.write_entry(k_ref, |av: &mut _| {
                            av.value(|vo| vo.string_value_ref(v_ref)).map(|_| ())
                        })?;
                    }
                    if self.error_tracking_standalone {
                        let trace_has_error = trace.spans().iter().any(|span| {
                            span.error() != 0
                                || span
                                    .attributes
                                    .get("_dd.span_events.has_exception")
                                    .and_then(AttributeValue::as_string)
                                    .is_some_and(|v| v == "true")
                        });
                        if trace_has_error {
                            let k_ref = self.string_table.intern(TAG_ETS_STANDALONE_ERROR_KEY);
                            let v_ref = self.string_table.intern(TAG_ETS_STANDALONE_ERROR_VALUE);
                            cattrs.write_entry(k_ref, |av: &mut _| {
                                av.value(|vo| vo.string_value_ref(v_ref)).map(|_| ())
                            })?;
                        }
                    }
                    {
                        let k_ref = self.string_table.intern(TAG_OTLP_SAMPLING_RATE);
                        let v_ref = self.string_table.intern(self.string_builder.as_str());
                        cattrs.write_entry(k_ref, |av: &mut _| {
                            av.value(|vo| vo.string_value_ref(v_ref)).map(|_| ())
                        })?;
                    }
                }

                if dropped_trace {
                    chunk.dropped_trace(true)?;
                }

                Ok(())
            })?;

            // Write the string table after all refs so the table is complete.
            // Protobuf allows fields in any order; decoders that do a full parse before
            // resolving refs handle strings-after-refs correctly.
            tp.strings(|sb| {
                for s in self.string_table.indices.iter() {
                    sb.add(s.as_ref())?;
                }
                Ok(())
            })?;

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

    fn additional_headers(&self) -> &[(HeaderName, HeaderValue)] {
        &self.extra_headers
    }
}

/// Encodes an [`AttributeValue`] into an idx-format `AnyValue` builder, interning any
/// string values into `st` on the fly.
fn encode_idx_attribute_value<S: ScratchBuffer>(
    builder: &mut datadog_protos::traces::builders::idx::AnyValueBuilder<'_, S>, value: &AttributeValue,
    st: &mut StringTable,
) -> std::io::Result<()> {
    builder
        .value(|vo| match value {
            AttributeValue::String(s) => vo.string_value_ref(st.intern(s)),
            AttributeValue::Bool(b) => vo.bool_value(*b),
            AttributeValue::Int(i) => vo.int_value(*i),
            AttributeValue::Float(f) => vo.double_value(*f),
            AttributeValue::Bytes(b) => vo.bytes_value(b),
            AttributeValue::Array(values) => vo.array_value(|arr| {
                for v in values {
                    arr.add_values(|av| encode_idx_attribute_value(av, v, st))?;
                }
                Ok(())
            }),
            AttributeValue::KeyValueList(kvs) => vo.key_value_list(|kvl| {
                for (k, v) in kvs {
                    kvl.add_key_values(|kv| {
                        kv.key(st.intern(k))?
                            .value(|av| encode_idx_attribute_value(av, v, st))?;
                        Ok(())
                    })?;
                }
                Ok(())
            }),
        })
        .map(|_| ())
}

fn resolve_hostname_from_payload<'a>(
    payload_hostname: &'a str, source: Option<&'a OtlpSource>, default_hostname: Option<&'a str>,
    ignore_missing_fields: bool,
) -> Option<&'a str> {
    if !payload_hostname.is_empty() {
        return Some(payload_hostname);
    }
    if ignore_missing_fields {
        return Some("");
    }
    match source {
        Some(src) => match src.kind {
            OtlpSourceKind::HostnameKind => Some(src.identifier.as_str()),
            _ => Some(""),
        },
        None => default_hostname,
    }
}

fn resolve_container_tags_from_attrs(
    attributes: &FastHashMap<MetaString, AttributeValue>, source: Option<&OtlpSource>, ignore_missing_fields: bool,
) -> Option<MetaString> {
    if let Some(AttributeValue::String(tags)) = attributes.get(KEY_DATADOG_CONTAINER_TAGS) {
        if !tags.is_empty() {
            return Some(tags.clone());
        }
    }

    if ignore_missing_fields {
        return None;
    }
    let mut container_tags = TagSet::default();
    extract_container_tags_from_attributes_map(attributes, &mut container_tags);
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use protobuf::CodedInputStream;
    use saluki_config::ConfigurationLoader;
    use saluki_core::data_model::event::trace::{Span as DdSpan, Trace};
    use stringtheory::MetaString;

    use super::*;
    use crate::common::datadog::apm::ApmConfig;
    use crate::common::otlp::config::TracesConfig;
    use crate::config::{DatadogRemapper, KEY_ALIASES};

    // ---------------------------------------------------------------------------
    // Decode helper: reads the idx AgentPayload wire format and returns the
    // resolved string-valued chunk attributes from all idx tracer payloads.
    //
    // Wire format notes (all tags = field_number << 3 | wire_type):
    //   AgentPayload.idxTracerPayloads  field 11, LEN  → tag 90
    //   idx TracerPayload.strings       field  1, LEN  → tag 10 (repeated)
    //   idx TracerPayload.chunks        field 11, LEN  → tag 90 (repeated)
    //   idx TraceChunk.attributes       field  3, LEN  → tag 26 (map)
    //   map-entry key   (Varint<u32>)   field  1, VARINT → tag 8
    //   map-entry value (AnyValue LEN)  field  2, LEN    → tag 18
    //   AnyValue.string_value_ref       field  1, VARINT → tag 8
    //
    // Note: the encoder writes string refs before the string table (strings appear
    // after chunks in the wire format). The helpers below collect all raw chunk bytes
    // first, then resolve string refs once the string table is fully read.
    // ---------------------------------------------------------------------------

    /// Decodes the chunk-level string attributes from an encoded AgentPayload.
    ///
    /// Returns one `HashMap<String, String>` per chunk, containing only string-valued
    /// attributes (resolved through the string table). Handles the case where the
    /// string table (field 1) appears after chunks (field 11) in the wire format.
    fn decode_idx_chunk_attributes(buf: &[u8]) -> Vec<HashMap<String, String>> {
        let mut result = Vec::new();
        let mut is = CodedInputStream::from_bytes(buf);
        while let Some(tag) = is.read_raw_tag_or_eof().unwrap() {
            if tag == 90 {
                // AgentPayload.idxTracerPayloads (field 11, LEN)
                let len = is.read_raw_varint32().unwrap();
                let old = is.push_limit(len as u64).unwrap();
                result.extend(decode_idx_tracer_payload_chunks(&mut is));
                is.pop_limit(old);
            } else {
                protobuf::rt::skip_field_for_tag(tag, &mut is).unwrap();
            }
        }
        result
    }

    fn decode_idx_tracer_payload_chunks(is: &mut CodedInputStream<'_>) -> Vec<HashMap<String, String>> {
        let mut strings: Vec<String> = Vec::new();
        // Buffer raw chunk bytes; string table may appear after chunks in the wire format.
        let mut raw_chunks: Vec<Vec<u8>> = Vec::new();
        while let Some(tag) = is.read_raw_tag_or_eof().unwrap() {
            match tag {
                10 => {
                    // idx TracerPayload.strings (field 1, LEN) - repeated string
                    strings.push(is.read_string().unwrap());
                }
                90 => {
                    // idx TracerPayload.chunks (field 11, LEN) - buffer raw bytes
                    let len = is.read_raw_varint32().unwrap();
                    raw_chunks.push(is.read_raw_bytes(len).unwrap());
                }
                _ => {
                    protobuf::rt::skip_field_for_tag(tag, is).unwrap();
                }
            }
        }
        // Resolve chunk string refs now that the string table is complete.
        raw_chunks
            .iter()
            .map(|bytes| {
                let mut chunk_is = CodedInputStream::from_bytes(bytes);
                decode_idx_chunk_string_attrs(&mut chunk_is, &strings)
            })
            .collect()
    }

    fn decode_idx_chunk_string_attrs(is: &mut CodedInputStream<'_>, strings: &[String]) -> HashMap<String, String> {
        let mut attrs = HashMap::new();
        while let Some(tag) = is.read_raw_tag_or_eof().unwrap() {
            if tag == 26 {
                // idx TraceChunk.attributes map entry (field 3, LEN)
                let len = is.read_raw_varint32().unwrap();
                let old = is.push_limit(len as u64).unwrap();
                let (k_ref, v_str_ref) = decode_string_map_entry(is);
                is.pop_limit(old);
                if let Some(v_ref) = v_str_ref {
                    let k = strings.get(k_ref as usize).cloned().unwrap_or_default();
                    let v = strings.get(v_ref as usize).cloned().unwrap_or_default();
                    if !k.is_empty() {
                        attrs.insert(k, v);
                    }
                }
            } else {
                protobuf::rt::skip_field_for_tag(tag, is).unwrap();
            }
        }
        attrs
    }

    /// Reads a single map entry (`key: u32 varint`, `value: AnyValue`).
    /// Returns `(key_ref, Some(string_value_ref))` when the `AnyValue` is a string ref.
    fn decode_string_map_entry(is: &mut CodedInputStream<'_>) -> (u32, Option<u32>) {
        let mut k_ref: u32 = 0;
        let mut v_str_ref: Option<u32> = None;
        while let Some(tag) = is.read_raw_tag_or_eof().unwrap() {
            match tag {
                8 => {
                    // map-entry key, field 1, VARINT
                    k_ref = is.read_raw_varint32().unwrap();
                }
                18 => {
                    // map-entry value AnyValue, field 2, LEN
                    let len = is.read_raw_varint32().unwrap();
                    let old = is.push_limit(len as u64).unwrap();
                    while let Some(av_tag) = is.read_raw_tag_or_eof().unwrap() {
                        if av_tag == 8 {
                            // AnyValue.string_value_ref, field 1, VARINT
                            v_str_ref = Some(is.read_raw_varint32().unwrap());
                        } else {
                            protobuf::rt::skip_field_for_tag(av_tag, is).unwrap();
                        }
                    }
                    is.pop_limit(old);
                }
                _ => {
                    protobuf::rt::skip_field_for_tag(tag, is).unwrap();
                }
            }
        }
        (k_ref, v_str_ref)
    }

    // ---------------------------------------------------------------------------
    // Test fixtures
    // ---------------------------------------------------------------------------

    async fn make_encoder(ets_enabled: bool) -> TraceEndpointEncoder {
        let env_vars: Vec<(String, String)> = if ets_enabled {
            vec![("APM_ERROR_TRACKING_STANDALONE_ENABLED".to_string(), "true".to_string())]
        } else {
            vec![]
        };
        let (cfg, _) = ConfigurationLoader::for_tests_with_provider_factory(
            None,
            Some(&env_vars),
            false,
            KEY_ALIASES,
            DatadogRemapper::new,
        )
        .await;
        let apm_config = ApmConfig::from_configuration(&cfg).expect("ApmConfig should deserialize");
        TraceEndpointEncoder::new(
            MetaString::from("test-host"),
            "0.0.0".to_string(),
            "none".to_string(),
            apm_config,
            TracesConfig::default(),
        )
    }

    fn make_trace() -> Trace {
        let span = DdSpan::new(
            MetaString::from("svc"),
            MetaString::from("op"),
            MetaString::from("res"),
            MetaString::from("web"),
            1,    // span_id
            0,    // parent_id
            0,    // start
            1000, // duration
            0,    // error
        );
        let mut trace = Trace::new(vec![span]);
        trace.priority = Some(1);
        trace
    }

    fn make_error_trace() -> Trace {
        let span = DdSpan::new(
            MetaString::from("svc"),
            MetaString::from("op"),
            MetaString::from("res"),
            MetaString::from("web"),
            1,    // span_id
            0,    // parent_id
            0,    // start
            1000, // duration
            1,    // error
        );
        let mut trace = Trace::new(vec![span]);
        trace.priority = Some(1);
        trace
    }

    // ---------------------------------------------------------------------------
    // Tests
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn ets_header_present_when_enabled() {
        let encoder = make_encoder(true).await;
        let headers = encoder.additional_headers();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0.as_str(), "x-datadog-error-tracking-standalone");
        assert_eq!(headers[0].1, "true");
    }

    #[tokio::test]
    async fn ets_header_absent_when_disabled() {
        let encoder = make_encoder(false).await;
        assert!(encoder.additional_headers().is_empty());
    }

    #[tokio::test]
    async fn ets_chunk_tag_present_for_error_trace() {
        let mut encoder = make_encoder(true).await;
        let trace = make_error_trace();
        let mut buf = Vec::new();
        encoder.encode(&trace, &mut buf).expect("encode should succeed");
        let chunk_attrs = decode_idx_chunk_attributes(&buf);
        let tag_value = chunk_attrs
            .iter()
            .find_map(|attrs| attrs.get("_dd.error_tracking_standalone.error").map(|v| v.as_str()));
        assert_eq!(
            tag_value,
            Some("true"),
            "ETS chunk tag should be present for error traces when ETS is enabled"
        );
    }

    #[tokio::test]
    async fn ets_chunk_tag_absent_for_non_error_trace() {
        let mut encoder = make_encoder(true).await;
        let trace = make_trace();
        let mut buf = Vec::new();
        encoder.encode(&trace, &mut buf).expect("encode should succeed");
        let chunk_attrs = decode_idx_chunk_attributes(&buf);
        let has_tag = chunk_attrs
            .iter()
            .any(|attrs| attrs.contains_key("_dd.error_tracking_standalone.error"));
        assert!(!has_tag, "ETS chunk tag should be absent for non-error traces");
    }

    #[tokio::test]
    async fn ets_chunk_tag_absent_when_disabled() {
        let mut encoder = make_encoder(false).await;
        let trace = make_trace();
        let mut buf = Vec::new();
        encoder.encode(&trace, &mut buf).expect("encode should succeed");
        let chunk_attrs = decode_idx_chunk_attributes(&buf);
        let has_tag = chunk_attrs
            .iter()
            .any(|attrs| attrs.contains_key("_dd.error_tracking_standalone.error"));
        assert!(!has_tag, "ETS chunk tag should be absent when ETS is disabled");
    }
}

#[cfg(test)]
mod config_smoke {
    use datadog_agent_config_testing::config_registry::structs;
    use datadog_agent_config_testing::run_config_smoke_tests;
    use serde_json::json;

    use super::DatadogTraceConfiguration;
    use crate::config::{DatadogRemapper, KEY_ALIASES};

    #[tokio::test]
    async fn smoke_test() {
        run_config_smoke_tests(
            structs::DATADOG_TRACE_CONFIGURATION,
            &[],
            json!({}),
            |cfg| {
                cfg.as_typed::<DatadogTraceConfiguration>()
                    .expect("DatadogTraceConfiguration should deserialize")
            },
            KEY_ALIASES,
            DatadogRemapper::new,
        )
        .await
    }
}
