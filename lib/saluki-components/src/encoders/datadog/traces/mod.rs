use std::{fmt::Write, time::Duration};

use agent_data_plane_config::{defaults::DEFAULT_TRACE_ENV, domains, SharedConfiguration};
use async_trait::async_trait;
use datadog_protos::traces::builders::{idx::SpanKind, AgentPayloadBuilder};
use http::{uri::PathAndQuery, HeaderName, HeaderValue, Method, Uri};
use piecemeal::{ScratchBuffer, ScratchWriter};
use saluki_common::collections::{FastHashMap, FastIndexSet};
use saluki_common::strings::StringBuilder;
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
    compressor_kind: String,

    /// Effective zstd compression level, resolved by the configuration layer.
    zstd_compressor_level: i32,

    /// How long the encoder waits before flushing a partially filled payload.
    ///
    /// A zero duration is treated as "flush almost immediately" during `build`.
    flush_timeout: Duration,

    /// Global environment tag applied to emitted payloads.
    env: String,

    /// Target sampled traces per second, forwarded to the intake as `target_tps`.
    target_traces_per_second: f64,

    /// Target sampled error traces per second, forwarded to the intake as `error_tps`.
    errors_per_second: f64,

    /// Whether Error Tracking standalone mode is enabled.
    error_tracking_standalone: bool,

    /// Whether spans missing intake-required fields are ingested rather than rejected.
    ignore_missing_datadog_fields: bool,

    /// Percentage of OTLP traces the probabilistic sampler keeps.
    sampling_percentage: f64,

    /// Default hostname, resolved from the environment provider.
    default_hostname: Option<String>,

    /// ADP version string embedded in emitted payloads.
    version: String,
}

impl DatadogTraceConfiguration {
    /// Creates a new `DatadogTraceConfiguration` from the resolved traces and shared configuration.
    ///
    /// The OTLP trace settings live in their own domain, so they arrive as a separate slice rather
    /// than through the traces domain.
    pub fn from_configuration(
        traces: &domains::traces::Domain, otlp_traces: &domains::otlp::Traces, shared: &SharedConfiguration,
    ) -> Self {
        let app_details = saluki_metadata::get_app_details();
        let version = format!("agent-data-plane/{}", app_details.version().raw());

        let compression = &shared.endpoints.compression;

        // ADP defaults the global environment tag to `none` rather than the Core Agent's empty
        // string, so normalize an empty resolved value back to `none`.
        let env = if traces.env.is_empty() {
            DEFAULT_TRACE_ENV.to_owned()
        } else {
            traces.env.clone()
        };

        Self {
            compressor_kind: compression.compressor_kind.clone(),
            zstd_compressor_level: compression.effective_zstd_level(),
            flush_timeout: shared.metrics_encoding.flush_timeout,
            env,
            target_traces_per_second: traces.target_traces_per_second,
            errors_per_second: traces.errors_per_second,
            error_tracking_standalone: traces.error_tracking_standalone_enabled,
            ignore_missing_datadog_fields: otlp_traces.ignore_missing_datadog_fields,
            sampling_percentage: otlp_traces.probabilistic_sampler_sampling_percentage,
            default_hostname: None,
            version,
        }
    }

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
                self.target_traces_per_second,
                self.errors_per_second,
                self.error_tracking_standalone,
                self.ignore_missing_datadog_fields,
                self.sampling_percentage,
            ),
            compression_scheme,
            RB_BUFFER_CHUNK_SIZE,
        )
        .await?;
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
        context
            .spawner()
            .noninterruptible("request_builder", |_shutdown| request_builder_fut)
            .on_worker_pool()
            .spawn()
            .await
            .error_context("Failed to spawn request builder task.")?;

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
    sampling_percentage: f64,
    string_builder: StringBuilder,
    string_table: StringTable,
    error_tracking_standalone: bool,
    extra_headers: Vec<(HeaderName, HeaderValue)>,
}

impl TraceEndpointEncoder {
    fn new(
        default_hostname: MetaString, version: String, env: String, target_traces_per_second: f64,
        errors_per_second: f64, error_tracking_standalone: bool, ignore_missing_datadog_fields: bool,
        sampling_percentage: f64,
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
            sampling_percentage,
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
            tp.strings(|sb| sb.add_many_mapped(&self.string_table.indices, |s| &**s))?;

            Ok(())
        })?;

        ap_builder.finish(output_buffer)?;

        Ok(())
    }

    fn sampling_rate(&self) -> f64 {
        let rate = self.sampling_percentage / 100.0;
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
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use datadog_protos::traces::{idx, AgentPayload};
    use protobuf::Message as _;
    use saluki_context::tags::Tag;
    use saluki_core::data_model::event::trace::{Span as DdSpan, Trace};
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
    // Keep-everything sampling percentage (the OTLP probabilistic sampler default).
    const DEFAULT_SAMPLING_PERCENTAGE: f64 = 100.0;

    fn make_encoder(ets_enabled: bool) -> TraceEndpointEncoder {
        TraceEndpointEncoder::new(
            MetaString::from("test-host"),
            "0.0.0".to_string(),
            "none".to_string(),
            DEFAULT_TARGET_TPS,
            DEFAULT_ERRORS_PER_SECOND,
            ets_enabled,
            false,
            DEFAULT_SAMPLING_PERCENTAGE,
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
        let trace = make_trace(); // no error
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
        let trace = make_trace();
        let mut buf = Vec::new();
        encoder.encode(&trace, &mut buf).expect("encode should succeed");
        let chunk_attrs = decode_etp_chunk_attributes(&buf);
        let has_tag = chunk_attrs
            .iter()
            .any(|attrs| attrs.contains_key("_dd.error_tracking_standalone.error"));
        assert!(!has_tag, "ETS chunk tag should be absent when ETS is disabled");
    }

    #[test]
    fn sampling_rate_clamps_percentage_to_unit_interval() {
        // `sampling_percentage` is a 0..100 percentage; only strictly in-range values map to a fractional rate, and
        // anything <= 0 or >= 100 collapses to 1.0 (sample everything).
        let cases = [
            (25.0, 0.25),
            (50.0, 0.5),
            (0.0, 1.0),
            (-10.0, 1.0),
            (100.0, 1.0),
            (150.0, 1.0),
        ];
        for (percentage, expected) in cases {
            let encoder = TraceEndpointEncoder::new(
                MetaString::from("test-host"),
                "0.0.0".to_string(),
                "none".to_string(),
                DEFAULT_TARGET_TPS,
                DEFAULT_ERRORS_PER_SECOND,
                false,
                false,
                percentage,
            );
            assert_eq!(expected, encoder.sampling_rate(), "sampling_rate for {percentage}%");
        }
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
    fn encode_prefixes_tracer_version_and_writes_otlp_sampling_rate() {
        let mut encoder = make_encoder(false);
        let mut trace = make_trace();
        trace.payload.tracer_version = MetaString::from("1.2.3");
        trace.otlp_sampling_rate = Some(0.5);

        let mut buf = Vec::new();
        encoder.encode(&trace, &mut buf).expect("encode should succeed");

        // The encoder emits the ETP `idxTracerPayloads` format, so decode through the string table.
        let tracer_versions = decode_etp_tracer_versions(&buf);
        let tracer_version = tracer_versions.first().expect("a tracer payload should be encoded");
        // The tracer version is prefixed with `otlp-` to mark the OTLP ingestion path.
        assert_eq!("otlp-1.2.3", tracer_version);

        // The OTLP sampling rate is written to each chunk formatted to two decimal places.
        let chunk_attrs = decode_etp_chunk_attributes(&buf);
        let otlp_sr = chunk_attrs
            .iter()
            .find_map(|attrs| attrs.get("_dd.otlp_sr"))
            .expect("chunk should carry the _dd.otlp_sr tag");
        assert_eq!("0.50", otlp_sr.as_str());
    }
}
