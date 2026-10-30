use std::{borrow::Cow, fmt::Write, time::Duration};

use async_trait::async_trait;
use datadog_protos::traces::builders::{idx::SpanKind, AgentPayloadBuilder};
use http::{uri::PathAndQuery, HeaderName, HeaderValue, Method, Uri};
use piecemeal::{ScratchBuffer, ScratchWriter};
use saluki_common::collections::{FastHashMap, FastIndexSet};
use saluki_common::strings::StringBuilder;
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{encoders::*, BuildContext},
    data_model::{
        event::{
            trace::{AttributeValue, Trace, TraceIngestInfo},
            EventType,
        },
        payload::{HttpPayload, Payload, PayloadMetadata, PayloadType},
        tags::TagSet,
    },
    observability::ComponentMetricsExt as _,
    runtime,
    topology::{EventsBuffer, PayloadsBuffer},
};
use saluki_env::host::providers::BoxedHostProvider;
use saluki_env::{EnvironmentProvider, HostProvider};
use saluki_error::generic_error;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_io::compression::CompressionScheme;
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;
use tokio::pin;
use tokio::{
    select,
    sync::mpsc::{self, Receiver, Sender},
    time::sleep,
};
use tracing::{debug, error};

use crate::common::datadog::{
    io::RB_BUFFER_CHUNK_SIZE,
    request_builder::{EndpointEncoder, RequestBuilder},
    telemetry::ComponentTelemetry,
    DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT, DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT, TAG_DECISION_MAKER,
};
use crate::common::otlp::util::{
    attributes_to_source, extract_container_tags_from_attributes_map, Source as OtlpSource,
    SourceKind as OtlpSourceKind, KEY_DATADOG_CONTAINER_TAGS,
};
use crate::config::AgentEnv;

const CONTAINER_TAGS_META_KEY: &str = "_dd.tags.container";
const MAX_TRACES_PER_PAYLOAD: usize = 10000;
static CONTENT_TYPE_PROTOBUF: HeaderValue = HeaderValue::from_static("application/x-protobuf");

// Sampling metadata keys / values.
const TAG_OTLP_SAMPLING_RATE: &str = "_dd.otlp_sr";
const DEFAULT_CHUNK_PRIORITY: i32 = 1; // PRIORITY_AUTO_KEEP

// ETS chunk-level attribute keys / values.
const TAG_ETS_STANDALONE_ERROR_KEY: &str = "_dd.error_tracking_standalone.error";
const TAG_ETS_STANDALONE_ERROR_VALUE: &str = "true";

/// String interning table for the ETP (Efficient Trace Payload) tracer payload format.
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

/// Configuration for the Datadog Traces encoder.
///
/// This encoder converts trace events into Datadog's TracerPayload protobuf format and sends them
/// to the Datadog traces intake endpoint (`/api/v0.2/traces`). It handles batching, compression,
/// and enrichment with metadata such as hostname, environment, and container tags.
pub struct DatadogTraceConfiguration {
    /// Compression algorithm applied to outgoing payloads.
    pub compressor_kind: String,

    /// zstd compression level, used when the compression algorithm is zstd.
    pub zstd_compressor_level: i32,

    /// How long the encoder waits before flushing a partially filled payload.
    ///
    /// A zero duration is treated as "flush almost immediately" during `build`.
    pub flush_timeout: Duration,

    /// Agent environment of emitted payloads.
    pub agent_env: AgentEnv,

    /// Target sampled traces per second, forwarded to the intake as `target_tps`.
    pub target_traces_per_second: f64,

    /// Target sampled error traces per second, forwarded to the intake as `error_tps`.
    pub errors_per_second: f64,

    /// Whether Error Tracking standalone mode is enabled.
    pub error_tracking_standalone: bool,

    /// Whether spans missing intake-required fields are ingested rather than rejected.
    pub ignore_missing_datadog_fields: bool,

    /// Default hostname of emitted payloads.
    ///
    /// If unset, payloads carry an empty hostname. See
    /// [`with_environment_provider`](Self::with_environment_provider).
    pub default_hostname: Option<String>,
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

    fn endpoint_encoder(&self) -> TraceEndpointEncoder {
        let app_details = saluki_metadata::get_app_details();
        TraceEndpointEncoder::new(
            MetaString::from(self.default_hostname.clone().unwrap_or_default()),
            format!("agent-data-plane/{}", app_details.version().raw()),
            self.agent_env.as_str().to_owned(),
            self.target_traces_per_second,
            self.errors_per_second,
            self.error_tracking_standalone,
            self.ignore_missing_datadog_fields,
        )
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

    async fn build(&self, context: BuildContext) -> Result<Box<dyn Encoder + Send>, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(context.component_context());
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let compression_scheme = CompressionScheme::new(&self.compressor_kind, self.zstd_compressor_level);

        // Create request builder for traces which is used to generate HTTP requests.
        let mut trace_rb =
            RequestBuilder::new(self.endpoint_encoder(), compression_scheme, RB_BUFFER_CHUNK_SIZE).await?;
        trace_rb.with_max_inputs_per_payload(MAX_TRACES_PER_PAYLOAD);

        let flush_timeout = if self.flush_timeout.is_zero() {
            // We always give ourselves a minimum flush timeout of 10ms to allow for some very minimal amount of
            // batching, while still practically flushing things almost immediately.
            Duration::from_millis(10)
        } else {
            self.flush_timeout
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

        let (events_tx, events_rx) = mpsc::channel(8);
        let (payloads_tx, mut payloads_rx) = mpsc::channel(8);

        // Run our request builder task on the worker pool.
        //
        // The request builder task ignores the shutdown signal on purpose: it drains its incoming event buffer channel
        // until the channel closes, which is what guarantees every buffered metric is encoded and dispatched.
        let request_builder_fut = run_request_builder(trace_rb, telemetry, events_rx, payloads_tx, flush_timeout);
        runtime::worker("request_builder", request_builder_fut)
            .on_runtime(context.topology_context().global_thread_pool().clone())
            .needed()
            .spawn();

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

        // Draining `payloads_rx` to completion already implies the request builder finished: it owns the only sender,
        // so the channel only closes once that child's future has run to completion (or been dropped).
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
    target_traces_per_second: f64,
    errors_per_second: f64,
    ignore_missing_datadog_fields: bool,
    string_builder: StringBuilder,
    string_table: StringTable,
    error_tracking_standalone: bool,
    extra_headers: Vec<(HeaderName, HeaderValue)>,
}

impl TraceEndpointEncoder {
    fn new(
        default_hostname: MetaString, version: String, env: String, target_traces_per_second: f64,
        errors_per_second: f64, error_tracking_standalone: bool, ignore_missing_datadog_fields: bool,
    ) -> Self {
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
            target_traces_per_second,
            errors_per_second,
            ignore_missing_datadog_fields,
            string_builder: StringBuilder::new(),
            string_table: StringTable::new(),
            error_tracking_standalone,
            extra_headers,
        }
    }

    fn encode_tracer_payload(&mut self, trace: &Trace, output_buffer: &mut Vec<u8>) -> std::io::Result<()> {
        let source = attributes_to_source(&trace.attributes);

        // OTLP traces report the receiver sampling rate and mark the tracer version as OTLP. Native traces keep
        // their tracer version as sent.
        let (tracer_version, otlp_sampling_rate) = match trace.ingest_info() {
            TraceIngestInfo::Datadog => (Cow::Borrowed(trace.payload.tracer_version.as_ref()), None),
            TraceIngestInfo::Otlp { receiver_sampling_rate } => {
                self.string_builder.clear();
                write!(&mut self.string_builder, "{:.2}", receiver_sampling_rate)
                    .expect("should never fail to format sampling rate");
                (
                    Cow::Owned(format!("otlp-{}", trace.payload.tracer_version)),
                    Some(self.string_builder.as_str()),
                )
            }
        };

        // Resolve computed metadata strings (may produce strings not directly present on trace fields).
        let container_tags =
            resolve_container_tags_from_attrs(&trace.attributes, source.as_ref(), self.ignore_missing_datadog_fields);
        let env_str: Option<&str> = if !trace.payload.env.is_empty() {
            Some(&trace.payload.env)
        } else if self.ignore_missing_datadog_fields {
            Some("")
        } else {
            None
        };
        let hostname_str: Option<&str> = resolve_hostname_from_payload(
            &trace.payload.hostname,
            source.as_ref(),
            Some(self.default_hostname.as_ref()),
            self.ignore_missing_datadog_fields,
        );
        let decision_maker = trace.decision_maker.as_deref();
        let priority = trace.priority.unwrap_or(DEFAULT_CHUNK_PRIORITY);
        let dropped_trace = trace.dropped_trace;

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
            .target_tps(self.target_traces_per_second)?
            .error_tps(self.errors_per_second)?;

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
            if !tracer_version.is_empty() {
                tp.tracer_version_ref(self.string_table.intern(&tracer_version))?;
            }
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
                                    encode_etp_attribute_value(av, v, &mut self.string_table)
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
                                            encode_etp_attribute_value(av, v, &mut self.string_table)
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
                                            encode_etp_attribute_value(av, v, &mut self.string_table)
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
                    if let Some(rate) = otlp_sampling_rate {
                        let k_ref = self.string_table.intern(TAG_OTLP_SAMPLING_RATE);
                        let v_ref = self.string_table.intern(rate);
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
            tp.strings(|sb| sb.add_many_mapped(&self.string_table.indices, |s| &**s))?;

            Ok(())
        })?;

        ap_builder.finish(output_buffer)?;

        Ok(())
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

/// Encodes an [`AttributeValue`] into an ETP-format `AnyValue` builder, interning any
/// string values into `st` on the fly.
fn encode_etp_attribute_value<S: ScratchBuffer>(
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
                    arr.add_values(|av| encode_etp_attribute_value(av, v, st))?;
                }
                Ok(())
            }),
            AttributeValue::KeyValueList(kvs) => vo.key_value_list(|kvl| {
                for (k, v) in kvs {
                    kvl.add_key_values(|kv| {
                        kv.key(st.intern(k))?
                            .value(|av| encode_etp_attribute_value(av, v, st))?;
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
mod sampling_round_trip;

/// Returns trace encoder settings carrying the configuration defaults.
#[cfg(test)]
fn trace_configuration(agent_env: AgentEnv) -> DatadogTraceConfiguration {
    DatadogTraceConfiguration {
        compressor_kind: "zstd".to_string(),
        zstd_compressor_level: 3,
        flush_timeout: Duration::from_secs(2),
        agent_env,
        target_traces_per_second: 10.0,
        errors_per_second: 10.0,
        error_tracking_standalone: false,
        ignore_missing_datadog_fields: false,
        default_hostname: None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};
    use std::sync::Arc;

    use datadog_protos::traces::{idx, AgentPayload};
    use protobuf::Message as _;
    use saluki_core::data_model::{
        event::trace::{Span as DdSpan, Trace, TraceIngestInfo},
        tags::Tag,
    };
    use stringtheory::MetaString;

    use super::*;

    // ---------------------------------------------------------------------------
    // Decode helpers for assertions on the encoded ETP `AgentPayload`.
    //
    // The encoder emits the wire format via `piecemeal` builders; here we decode it back
    // with the independently code-generated `rust-protobuf` types (`idx::TracerPayload`
    // and friends). A full-message parse resolves protobuf field ordering for us, so the
    // fact that the encoder writes string refs before the string table is a non-issue.
    //
    // The only ETP-specific work left is resolving `u32` string-table refs and unwrapping
    // the `AnyValue` oneof, which the two helpers below handle.
    // ---------------------------------------------------------------------------

    /// Resolves a string-table ref against a tracer payload's string table.
    fn resolve_ref(strings: &[String], string_ref: u32) -> &str {
        strings.get(string_ref as usize).map(String::as_str).unwrap_or_default()
    }

    /// Resolves the string-valued entries of an ETP attribute map into a readable
    /// `key -> value` map, keeping only entries whose `AnyValue` is a string ref and
    /// whose key resolves to a non-empty string. Keys and values are resolved through
    /// the payload string table.
    fn string_attrs(attrs: &HashMap<u32, idx::AnyValue>, strings: &[String]) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for (k_ref, value) in attrs {
            if let Some(idx::any_value::Value::StringValueRef(v_ref)) = &value.value {
                let key = resolve_ref(strings, *k_ref);
                if !key.is_empty() {
                    out.insert(key.to_string(), resolve_ref(strings, *v_ref).to_string());
                }
            }
        }
        out
    }

    /// Collects the resolved string-valued attributes of every chunk across all ETP
    /// tracer payloads in an encoded `AgentPayload`, one map per chunk.
    fn decode_etp_chunk_attributes(buf: &[u8]) -> Vec<HashMap<String, String>> {
        let payload = AgentPayload::parse_from_bytes(buf).expect("AgentPayload should decode");
        payload
            .idxTracerPayloads
            .iter()
            .flat_map(|tp| {
                let strings = tp.strings();
                tp.chunks()
                    .iter()
                    .map(move |chunk| string_attrs(chunk.attributes(), strings))
            })
            .collect()
    }

    /// Collects the resolved string-valued attributes of every span across all ETP tracer payloads in an encoded
    /// `AgentPayload`, one map per span.
    fn decode_etp_span_attributes(buf: &[u8]) -> Vec<HashMap<String, String>> {
        let payload = AgentPayload::parse_from_bytes(buf).expect("AgentPayload should decode");
        payload
            .idxTracerPayloads
            .iter()
            .flat_map(|tp| {
                let strings = tp.strings();
                tp.chunks()
                    .iter()
                    .flat_map(|chunk| chunk.spans())
                    .map(move |span| string_attrs(span.attributes(), strings))
            })
            .collect()
    }

    /// Resolves the `tracerVersionRef` of every ETP tracer payload in an encoded `AgentPayload`.
    fn decode_etp_tracer_versions(buf: &[u8]) -> Vec<String> {
        let payload = AgentPayload::parse_from_bytes(buf).expect("AgentPayload should decode");
        payload
            .idxTracerPayloads
            .iter()
            .map(|tp| resolve_ref(tp.strings(), tp.tracerVersionRef()).to_string())
            .collect()
    }

    // The APM sampler defaults: 10 target and error traces per second.
    const DEFAULT_TARGET_TPS: f64 = 10.0;
    const DEFAULT_ERRORS_PER_SECOND: f64 = 10.0;

    fn make_encoder(ets_enabled: bool) -> TraceEndpointEncoder {
        TraceEndpointEncoder::new(
            MetaString::from("test-host"),
            "0.0.0".to_string(),
            "none".to_string(),
            DEFAULT_TARGET_TPS,
            DEFAULT_ERRORS_PER_SECOND,
            ets_enabled,
            false,
        )
    }

    fn make_trace(ingest_info: TraceIngestInfo) -> Trace {
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
        let mut trace = Trace::new(vec![span], ingest_info);
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
        let mut trace = Trace::new(vec![span], TraceIngestInfo::Datadog);
        trace.priority = Some(1);
        trace
    }

    #[test]
    fn ets_header_present_when_enabled() {
        let encoder = make_encoder(true);
        let headers = encoder.additional_headers();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0.as_str(), "x-datadog-error-tracking-standalone");
        assert_eq!(headers[0].1, "true");
    }

    #[test]
    fn ets_header_absent_when_disabled() {
        let encoder = make_encoder(false);
        assert!(encoder.additional_headers().is_empty());
    }

    #[test]
    fn ets_chunk_tag_present_for_error_trace() {
        let mut encoder = make_encoder(true);
        let trace = make_error_trace();
        let mut buf = Vec::new();
        encoder.encode(&trace, &mut buf).expect("encode should succeed");
        let chunk_attrs = decode_etp_chunk_attributes(&buf);
        let tag_value = chunk_attrs
            .iter()
            .find_map(|attrs| attrs.get("_dd.error_tracking_standalone.error").map(|v| v.as_str()));
        assert_eq!(
            tag_value,
            Some("true"),
            "ETS chunk tag should be present for error traces when ETS is enabled"
        );
    }

    #[test]
    fn ets_chunk_tag_absent_for_non_error_trace() {
        let mut encoder = make_encoder(true);
        let trace = make_trace(TraceIngestInfo::Datadog); // no error
        let mut buf = Vec::new();
        encoder.encode(&trace, &mut buf).expect("encode should succeed");
        let chunk_attrs = decode_etp_chunk_attributes(&buf);
        let has_tag = chunk_attrs
            .iter()
            .any(|attrs| attrs.contains_key("_dd.error_tracking_standalone.error"));
        assert!(!has_tag, "ETS chunk tag should be absent for non-error traces");
    }

    #[test]
    fn ets_chunk_tag_absent_when_disabled() {
        let mut encoder = make_encoder(false);
        let trace = make_trace(TraceIngestInfo::Datadog);
        let mut buf = Vec::new();
        encoder.encode(&trace, &mut buf).expect("encode should succeed");
        let chunk_attrs = decode_etp_chunk_attributes(&buf);
        let has_tag = chunk_attrs
            .iter()
            .any(|attrs| attrs.contains_key("_dd.error_tracking_standalone.error"));
        assert!(!has_tag, "ETS chunk tag should be absent when ETS is disabled");
    }

    #[test]
    fn resolve_hostname_from_payload_prefers_payload_then_source_then_default() {
        let host_source = OtlpSource {
            kind: OtlpSourceKind::HostnameKind,
            identifier: "resolved-host".to_string(),
        };
        let fargate_source = OtlpSource {
            kind: OtlpSourceKind::AwsEcsFargateKind,
            identifier: "task-arn".to_string(),
        };

        // A non-empty payload hostname always wins.
        assert_eq!(
            Some("payload-host"),
            resolve_hostname_from_payload("payload-host", Some(&host_source), Some("default"), false)
        );
        // An empty payload plus `ignore_missing_fields` short-circuits to an empty hostname.
        assert_eq!(
            Some(""),
            resolve_hostname_from_payload("", Some(&host_source), Some("default"), true)
        );
        // Honoring fields, a hostname-kind source supplies its identifier.
        assert_eq!(
            Some("resolved-host"),
            resolve_hostname_from_payload("", Some(&host_source), Some("default"), false)
        );
        // A non-hostname (Fargate) source resolves to an empty hostname.
        assert_eq!(
            Some(""),
            resolve_hostname_from_payload("", Some(&fargate_source), Some("default"), false)
        );
        // With no source, it falls back to the default hostname (which may itself be absent).
        assert_eq!(
            Some("default"),
            resolve_hostname_from_payload("", None, Some("default"), false)
        );
        assert_eq!(None, resolve_hostname_from_payload("", None, None, false));
    }

    #[test]
    fn append_tags_joins_non_empty_segments_with_commas() {
        let mut target = String::new();

        // Appending an empty segment is a no-op.
        append_tags(&mut target, "");
        assert_eq!("", target);

        // The first non-empty append does not prepend a separator.
        append_tags(&mut target, "a:1");
        assert_eq!("a:1", target);

        // Subsequent non-empty appends are comma-separated.
        append_tags(&mut target, "b:2");
        assert_eq!("a:1,b:2", target);

        // An empty segment remains a no-op even once the target is non-empty.
        append_tags(&mut target, "");
        assert_eq!("a:1,b:2", target);
    }

    #[test]
    fn flatten_container_tag_comma_joins_the_tag_set() {
        assert_eq!("", flatten_container_tag(TagSet::default()));

        let single: TagSet = std::iter::once(Tag::from_static("image_name:web")).collect();
        assert_eq!("image_name:web", flatten_container_tag(single));

        let multiple: TagSet = ["image_name:web", "runtime:docker"]
            .into_iter()
            .map(Tag::from_static)
            .collect();
        let flattened = flatten_container_tag(multiple);
        assert_eq!(
            BTreeSet::from(["image_name:web", "runtime:docker"]),
            flattened.split(',').collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn resolve_container_tags_prefers_explicit_container_tags_attribute() {
        // An explicit, non-empty `datadog.container_tags` attribute is used verbatim.
        let mut attributes = FastHashMap::default();
        attributes.insert(
            MetaString::from(KEY_DATADOG_CONTAINER_TAGS),
            AttributeValue::String(MetaString::from("region:us,team:core")),
        );
        assert_eq!(
            Some(MetaString::from("region:us,team:core")),
            resolve_container_tags_from_attrs(&attributes, None, false)
        );
    }

    #[test]
    fn resolve_container_tags_returns_none_without_container_attributes() {
        let attributes = FastHashMap::default();
        // `ignore_missing_fields` skips the extraction path entirely.
        assert_eq!(None, resolve_container_tags_from_attrs(&attributes, None, true));
        // Honoring fields but with no container attributes and no Fargate source still yields nothing.
        assert_eq!(None, resolve_container_tags_from_attrs(&attributes, None, false));
    }

    #[test]
    fn agent_payload_env_is_the_agent_env() {
        // The encoder sends the resolved agent environment unchanged: one that normalizes to empty does not become
        // `none`.
        let cases = [
            (AgentEnv::resolve(None, Some("Prod"), &[], &[]), "prod"),
            (AgentEnv::resolve(Some("!!!"), None, &[], &[]), ""),
        ];
        for (agent_env, expected) in cases {
            let config = trace_configuration(agent_env);
            let mut buf = Vec::new();
            config
                .endpoint_encoder()
                .encode(&make_trace(TraceIngestInfo::Datadog), &mut buf)
                .expect("encode should succeed");

            let payload = AgentPayload::parse_from_bytes(&buf).expect("AgentPayload should decode");
            assert_eq!(payload.env, expected);
        }
    }

    /// Only OTLP traces get the `otlp-` tracer version prefix and `_dd.otlp_sr`, as in the Go trace agent's OTLP
    /// receiver; native tracer versions pass through unchanged. The rate is formatted like Go's
    /// `strconv.FormatFloat(rate, 'f', 2, 64)`, which rounds exact ties to even:
    /// https://github.com/DataDog/datadog-agent/blob/c6f7e0bc2d5744601a10f231751948ac7a4ee781/pkg/trace/api/otlp.go#L581-L590
    #[test]
    fn encode_marks_only_otlp_traces_with_tracer_version_prefix_and_sampling_rate() {
        let otlp = |receiver_sampling_rate| TraceIngestInfo::Otlp { receiver_sampling_rate };
        let cases = [
            (otlp(0.5), "1.2.3", "otlp-1.2.3", Some("0.50")),
            (otlp(1.0), "", "otlp-", Some("1.00")),
            (otlp(0.125), "1.2.3", "otlp-1.2.3", Some("0.12")),
            (otlp(0.375), "1.2.3", "otlp-1.2.3", Some("0.38")),
            (otlp(1.0 / 3.0), "1.2.3", "otlp-1.2.3", Some("0.33")),
            (otlp(0.001), "1.2.3", "otlp-1.2.3", Some("0.00")),
            (TraceIngestInfo::Datadog, "v2.1.0", "v2.1.0", None),
            (TraceIngestInfo::Datadog, "", "", None),
        ];
        for (ingest_info, tracer_version, want_tracer_version, want_rate) in cases {
            let mut trace = make_trace(ingest_info);
            trace.payload.tracer_version = MetaString::from(tracer_version);

            let mut buf = Vec::new();
            make_encoder(false)
                .encode(&trace, &mut buf)
                .expect("encode should succeed");

            let ctx = format!("{ingest_info:?}, tracer version {tracer_version:?}");
            assert_eq!(decode_etp_tracer_versions(&buf), [want_tracer_version], "{ctx}");
            let chunk_attrs = decode_etp_chunk_attributes(&buf);
            assert_eq!(chunk_attrs.len(), 1, "{ctx}");
            assert_eq!(
                chunk_attrs[0].get(TAG_OTLP_SAMPLING_RATE).map(String::as_str),
                want_rate,
                "{ctx}"
            );
        }
    }

    /// The chunk's `_dd.otlp_sr` comes only from the ingest information: a trace attribute of the same name, such as
    /// an OTLP resource attribute, doesn't replace it. A span attribute of the same name is span data and passes
    /// through.
    #[test]
    fn encode_otlp_sampling_rate_is_independent_of_same_named_attributes() {
        let conflicting = |ingest_info| {
            let mut trace = make_trace(ingest_info);
            trace.attributes = Arc::new(FastHashMap::from_iter([(
                MetaString::from(TAG_OTLP_SAMPLING_RATE),
                AttributeValue::String(MetaString::from("0.33")),
            )]));
            trace.spans_mut()[0].attributes.insert(
                MetaString::from(TAG_OTLP_SAMPLING_RATE),
                AttributeValue::String(MetaString::from("0.77")),
            );
            trace
        };
        let encode = |trace: &Trace| {
            let mut buf = Vec::new();
            make_encoder(false)
                .encode(trace, &mut buf)
                .expect("encode should succeed");
            buf
        };

        let buf = encode(&conflicting(TraceIngestInfo::Otlp {
            receiver_sampling_rate: 0.5,
        }));
        assert_eq!(decode_etp_chunk_attributes(&buf)[0][TAG_OTLP_SAMPLING_RATE], "0.50");
        assert_eq!(decode_etp_span_attributes(&buf)[0][TAG_OTLP_SAMPLING_RATE], "0.77");

        // The encoder never synthesizes a native chunk rate. A native trace's own attribute may be dropped or kept.
        let buf = encode(&conflicting(TraceIngestInfo::Datadog));
        let native_chunk_rate = decode_etp_chunk_attributes(&buf)[0]
            .get(TAG_OTLP_SAMPLING_RATE)
            .cloned();
        assert!(
            matches!(native_chunk_rate.as_deref(), None | Some("0.33")),
            "unexpected native chunk rate {native_chunk_rate:?}"
        );
        assert_eq!(decode_etp_span_attributes(&buf)[0][TAG_OTLP_SAMPLING_RATE], "0.77");
    }
}
