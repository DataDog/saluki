use std::{borrow::Cow, fmt, fmt::Write, sync::Arc, time::Duration};

use async_trait::async_trait;
use datadog_protos::traces::builders::{idx::SpanKind, AgentPayloadBuilder};
use http::{uri::PathAndQuery, HeaderName, HeaderValue, Method, Uri};
use piecemeal::{ScratchBuffer, ScratchWriter};
use saluki_common::collections::{FastHashMap, FastIndexSet};
use saluki_common::strings::StringBuilder;
use saluki_core::data_model::event::trace::AttributeValueExt as _;
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{encoders::*, BuildContext},
    data_model::{
        event::{
            trace::{AttributeValue, Trace, TraceIngestInfo},
            EventType,
        },
        origin::OriginTagCardinality,
        payload::{HttpPayload, Payload, PayloadMetadata, PayloadType},
        tags::{SharedTagSet, TagSet},
    },
    observability::ComponentMetricsExt as _,
    runtime,
    topology::{EventsBuffer, PayloadsBuffer},
};
use saluki_env::host::providers::BoxedHostProvider;
use saluki_env::{workload::EntityId, EnvironmentProvider, HostProvider, WorkloadProvider};
use saluki_error::generic_error;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_io::compression::CompressionScheme;
use saluki_metrics::{static_metrics, Counter, Histogram, MetricsBuilder};
use stringtheory::MetaString;
use tokio::pin;
use tokio::{
    select,
    sync::mpsc::{self, Receiver, Sender},
    time::sleep,
};
use tracing::{debug, error};

use crate::common::datadog::{
    get_tracer_hostname,
    io::RB_BUFFER_CHUNK_SIZE,
    request_builder::{EndpointEncoder, RequestBuilder},
    telemetry::ComponentTelemetry,
    version_data::ContainerVersionData,
    DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT, DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT, KEY_ANALYZED_SPANS,
    TAG_DECISION_MAKER,
};
use crate::common::otlp::util::{
    attributes_to_source, extract_trace_container_tags, Source as OtlpSource, SourceKind as OtlpSourceKind,
    KEY_DATADOG_CONTAINER_TAGS,
};
use crate::common::traces::normalize::{is_normalized_tag_value, normalize_tag_value_into_unchecked};
use crate::common::traces::{TAG_PROCESS_TAGS, TAG_SDK_OTLP_EXPORT};
use crate::config::AgentEnv;

const CONTAINER_TAGS_META_KEY: &str = "_dd.tags.container";

/// Cardinality of the container tags looked up to fill in a missing application version.
///
/// The version data tags are low cardinality, so a low cardinality lookup finds them.
const VERSION_DATA_TAG_CARDINALITY: OriginTagCardinality = OriginTagCardinality::Low;

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

    /// Whether OTLP traces skip recomputing missing Datadog fields (`otlp_config.traces.ignore_missing_datadog_fields`).
    ///
    /// Has no effect on Datadog traces.
    pub ignore_missing_datadog_fields: bool,

    /// Default hostname of emitted payloads.
    ///
    /// If unset, payloads carry an empty hostname. See
    /// [`with_environment_provider`](Self::with_environment_provider).
    pub default_hostname: Option<String>,

    /// Workload provider used to look up container tags, if any.
    ///
    /// When a trace carries no application version, the `version` tag of its container fills it in. The lookup never
    /// waits: if the container's tags aren't known yet, the version stays empty.
    pub workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,
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
        .with_container_tag_lookup(ContainerTagLookup(self.workload_provider.clone()))
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
        let payload_telemetry = TracePayloadTelemetry::new(context.component_id(), context.component_type().as_str());
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
            payload_telemetry,
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
    payload_telemetry: TracePayloadTelemetry,
    flush_timeout: Duration,
}

/// Telemetry for what the traces encoder writes into payloads.
#[static_metrics(prefix = component, labels(component_id, component_type))]
struct TracePayloadTelemetry {
    traces_encoded_total: Counter,
    spans_encoded_total: Counter,
    trace_events_encoded_total: Counter,
    bytes_uncompressed_total: Counter,
    /// The size of each request body.
    ///
    /// Exported distributions keep their buckets, sum and count, but not the exact maximum, so a derived gauge keeps
    /// the largest body seen so far.
    #[metric(derived(max))]
    payload_size_bytes: Histogram,
}

impl TracePayloadTelemetry {
    /// Tracks a trace written into a payload.
    fn track_trace(&self, counts: TraceCounts) {
        self.traces_encoded_total().increment(1);
        self.spans_encoded_total().increment(counts.spans);
        self.trace_events_encoded_total().increment(counts.events);
    }

    /// Tracks a flushed payload, given its size before compression and the size of the request body.
    fn track_payload(&self, uncompressed_len: usize, body_len: usize) {
        self.bytes_uncompressed_total().increment(uncompressed_len as u64);
        self.payload_size_bytes().record(body_len as f64);
    }
}

/// Spans and events that a trace adds to a payload.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TraceCounts {
    spans: u64,
    events: u64,
}

impl TraceCounts {
    /// Counts the spans and events of a trace.
    ///
    /// A trace that sampling dropped is still sent when it carries spans kept as standalone events. Only those
    /// analyzed spans count, as events, and none of its spans count as spans. A kept trace counts all of its spans as
    /// spans and no events, even when some of its spans are analyzed: those are not sent as standalone events.
    fn from_trace(trace: &Trace) -> Self {
        if trace.dropped_trace {
            let events = trace
                .spans()
                .iter()
                .filter(|span| span.attributes.contains_key(KEY_ANALYZED_SPANS))
                .count();
            Self {
                spans: 0,
                events: events as u64,
            }
        } else {
            Self {
                spans: trace.spans().len() as u64,
                events: 0,
            }
        }
    }
}

// Encodes Trace events to TracerPayloads.
#[async_trait]
impl Encoder for DatadogTrace {
    async fn run(mut self: Box<Self>, mut context: EncoderContext) -> Result<(), GenericError> {
        let Self {
            trace_rb,
            telemetry,
            payload_telemetry,
            flush_timeout,
        } = *self;

        let mut health = context.take_health_handle();

        let (events_tx, events_rx) = mpsc::channel(8);
        let (payloads_tx, mut payloads_rx) = mpsc::channel(8);

        // Run our request builder task on the worker pool.
        //
        // The request builder task ignores the shutdown signal on purpose: it drains its incoming event buffer channel
        // until the channel closes, which is what guarantees every buffered metric is encoded and dispatched.
        let request_builder_fut = run_request_builder(
            trace_rb,
            telemetry,
            payload_telemetry,
            events_rx,
            payloads_tx,
            flush_timeout,
        );
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
    payload_telemetry: TracePayloadTelemetry, mut events_rx: Receiver<EventsBuffer>,
    payloads_tx: Sender<PayloadsBuffer>, flush_timeout: std::time::Duration,
) -> Result<(), GenericError> {
    let mut pending_flush = false;
    let pending_flush_timeout = sleep(flush_timeout);
    pin!(pending_flush_timeout);

    loop {
        select! {
            Some(event_buffer) = events_rx.recv() => {
                for trace in event_buffer.into_events::<Trace>() {
                    // Encode the trace. If we get it back, that means the current request is full, and we need to
                    // flush it before we can try to encode the trace again.
                    let trace_counts = TraceCounts::from_trace(&trace);
                    let trace_to_retry = match trace_request_builder.encode(trace).await {
                        Ok(None) => {
                            payload_telemetry.track_trace(trace_counts);
                            continue;
                        }
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
                            Ok((events, _data_points, uncompressed_len, request)) => {
                                payload_telemetry.track_payload(uncompressed_len, request.body().len());
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
                    match trace_request_builder.encode(trace_to_retry).await {
                        Ok(None) => payload_telemetry.track_trace(trace_counts),
                        Ok(Some(_)) => {
                            error!("Trace did not fit in a request after flushing; dropping it.");
                            telemetry.events_dropped_encoder().increment(1);
                        }
                        Err(e) => {
                            error!(error = %e, "Failed to encode trace.");
                            telemetry.events_dropped_encoder().increment(1);
                        }
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
                        Ok((events, _data_points, uncompressed_len, request)) => {
                            payload_telemetry.track_payload(uncompressed_len, request.body().len());
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

/// One-shot lookup of a container's tags.
///
/// The lookup doesn't wait for the container to become known, and doesn't retry.
#[derive(Clone, Default)]
struct ContainerTagLookup(Option<Arc<dyn WorkloadProvider + Send + Sync>>);

impl ContainerTagLookup {
    /// Returns the tags of the container with the given ID, if the container is known.
    fn get(&self, container_id: &MetaString) -> Option<SharedTagSet> {
        if container_id.is_empty() {
            return None;
        }
        let provider = self.0.as_ref()?;
        provider.get_tags_for_entity(&EntityId::Container(container_id.clone()), VERSION_DATA_TAG_CARDINALITY)
    }
}

impl fmt::Debug for ContainerTagLookup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContainerTagLookup")
            .field("enabled", &self.0.is_some())
            .finish()
    }
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
    env_builder: StringBuilder,
    string_table: StringTable,
    error_tracking_standalone: bool,
    extra_headers: Vec<(HeaderName, HeaderValue)>,
    container_tags: ContainerTagLookup,
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
            env_builder: StringBuilder::new(),
            string_table: StringTable::new(),
            error_tracking_standalone,
            extra_headers,
            container_tags: ContainerTagLookup::default(),
        }
    }

    /// Sets the lookup used to fill in a missing application version from container tags.
    fn with_container_tag_lookup(mut self, container_tags: ContainerTagLookup) -> Self {
        self.container_tags = container_tags;
        self
    }

    fn encode_tracer_payload(&mut self, trace: &Trace, output_buffer: &mut Vec<u8>) -> std::io::Result<()> {
        let is_otlp = matches!(trace.ingest_info(), TraceIngestInfo::Otlp { .. });
        // Resource attributes only describe the source of OTLP traces.
        let source = if is_otlp {
            attributes_to_source(&trace.attributes)
        } else {
            None
        };

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

        // `ignore_missing_datadog_fields` is an OTLP setting, so it never changes how Datadog traces are encoded.
        let ignore_missing_datadog_fields = self.ignore_missing_datadog_fields && is_otlp;

        // The payload environment is normalized like any other tag value before it's interned.
        let payload_env: &str = if is_normalized_tag_value(&trace.payload.env) {
            &trace.payload.env
        } else {
            normalize_tag_value_into_unchecked(&trace.payload.env, &mut self.env_builder);
            self.env_builder.as_str()
        };
        let env_str: Option<&str> = if !payload_env.is_empty() {
            Some(payload_env)
        } else if ignore_missing_datadog_fields {
            Some("")
        } else {
            None
        };

        // OTLP traces derive container tags from their resource, and their hostname is the one the translator resolved.
        // Datadog traces carry what the receiver resolved, and an empty tracer hostname stays empty, as the trace-agent
        // writer leaves it: the agent's own hostname is reported once, on the agent payload.
        let otlp_container_tags = is_otlp
            .then(|| {
                resolve_container_tags_from_attrs(
                    &trace.attributes,
                    trace.payload.resolved_container_tags(),
                    source.as_ref(),
                )
            })
            .flatten();
        let (container_tags, hostname_str): (Option<&str>, Option<&str>) = if is_otlp {
            (
                otlp_container_tags.as_deref(),
                resolve_hostname(
                    get_tracer_hostname(trace).map(|hostname| hostname.as_ref()),
                    true,
                    Some(self.default_hostname.as_ref()),
                    ignore_missing_datadog_fields,
                ),
            )
        } else {
            (
                non_empty(&trace.payload.receiver().container_tags),
                non_empty(&trace.payload.hostname),
            )
        };
        // The container's `version` tag fills in an application version that the payload leaves empty. The tags come
        // from the trace's own container-tag attributes and from the container's known tags, as they do for stats. They
        // are gathered only when the version is missing.
        let version_tags = if trace.payload.app_version.is_empty() && !trace.payload.container_id.is_empty() {
            let mut tags = TagSet::default();
            extract_trace_container_tags(trace.payload.resolved_container_tags(), &trace.attributes, &mut tags);
            if let Some(provider_tags) = self.container_tags.get(&trace.payload.container_id) {
                tags.merge_shared(&provider_tags);
            }
            Some(tags)
        } else {
            None
        };
        let app_version: &str = if !trace.payload.app_version.is_empty() {
            &trace.payload.app_version
        } else {
            version_tags
                .as_ref()
                .map(|tags| ContainerVersionData::from_tags(tags).app_version)
                .unwrap_or_default()
        };
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
            if !app_version.is_empty() {
                tp.app_version_ref(self.string_table.intern(app_version))?;
            }

            // Receiver-resolved metadata goes in the payload-level attributes map, as the trace-agent sets it.
            for (key, value) in [
                (CONTAINER_TAGS_META_KEY, container_tags),
                (TAG_PROCESS_TAGS, non_empty(&trace.payload.receiver().process_tags)),
                (
                    TAG_SDK_OTLP_EXPORT,
                    non_empty(&trace.payload.receiver().sdk_otlp_export),
                ),
            ] {
                if let Some(value) = value {
                    let k_ref = self.string_table.intern(key);
                    let v_ref = self.string_table.intern(value);
                    tp.attributes().write_entry(k_ref, |av: &mut _| {
                        av.value(|vo| vo.string_value_ref(v_ref)).map(|_| ())
                    })?;
                }
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

fn non_empty(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}

/// Resolves the hostname to report for a trace.
///
/// `tracer_hostname` is the hostname the trace carries. Without one, OTLP traces report none: the translator already
/// applied its source cascade, so an empty hostname means a serverless resource. Other traces fall back to
/// `default_hostname`.
fn resolve_hostname<'a>(
    tracer_hostname: Option<&'a str>, is_otlp: bool, default_hostname: Option<&'a str>, ignore_missing_fields: bool,
) -> Option<&'a str> {
    if let Some(hostname) = tracer_hostname {
        return Some(hostname);
    }
    if ignore_missing_fields || is_otlp {
        return Some("");
    }
    default_hostname
}

fn resolve_container_tags_from_attrs(
    attributes: &FastHashMap<MetaString, AttributeValue>, resolved_tags: &[MetaString], source: Option<&OtlpSource>,
) -> Option<MetaString> {
    // An explicit, non-empty `datadog.container_tags` attribute is always emitted, with the tags the source resolved
    // from the resource appended. Everything reaches the payload regardless of how missing Datadog fields are
    // treated, matching the trace agent's receiver, which extracts container tags unconditionally.
    let mut flattened = String::new();
    if let Some(AttributeValue::String(tags)) = attributes.get(KEY_DATADOG_CONTAINER_TAGS) {
        if !tags.is_empty() {
            flattened.push_str(tags.as_ref());
        }
    }

    let is_fargate_source = source.is_some_and(|src| src.kind == OtlpSourceKind::AwsEcsFargate);
    if resolved_tags.is_empty() {
        let mut container_tags = TagSet::default();
        extract_trace_container_tags(resolved_tags, attributes, &mut container_tags);
        for tag in container_tags {
            append_tags(&mut flattened, tag.as_str());
        }
    } else {
        // The resolved tags are already deduplicated, so append them instead of copying them into a tag set.
        for tag in resolved_tags {
            append_tags(&mut flattened, tag.as_ref());
        }
    }
    if is_fargate_source {
        if let Some(src) = source {
            append_tags(&mut flattened, &src.tag());
        }
    }

    (!flattened.is_empty()).then(|| MetaString::from(flattened))
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
        workload_provider: None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use datadog_protos::traces::{idx, AgentPayload};
    use protobuf::Message as _;
    use saluki_core::data_model::event::{
        trace::{ReceiverMetadata, Span as DdSpan, Trace, TraceIngestInfo},
        Event,
    };
    use saluki_env::workload::providers::TestWorkloadProvider;
    use saluki_metrics::test::TestRecorder;
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

    /// Resolves the `hostnameRef` of every ETP tracer payload in an encoded `AgentPayload`.
    fn decode_etp_hostnames(buf: &[u8]) -> Vec<String> {
        let payload = AgentPayload::parse_from_bytes(buf).expect("AgentPayload should decode");
        payload
            .idxTracerPayloads
            .iter()
            .map(|tp| resolve_ref(tp.strings(), tp.hostnameRef()).to_string())
            .collect()
    }

    fn encoded_hostname(trace: &Trace) -> String {
        let mut encoder = make_encoder(false);
        let mut buf = Vec::new();
        encoder.encode(trace, &mut buf).expect("encode should succeed");
        decode_etp_hostnames(&buf).remove(0)
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

    /// Reads the spans of the first tracer payload and chunk of an encoded `AgentPayload`, with its string table.
    fn decode_etp_first_span(buf: &[u8]) -> (idx::Span, Vec<String>) {
        let payload = AgentPayload::parse_from_bytes(buf).expect("AgentPayload should decode");
        let tracer_payload = &payload.idxTracerPayloads[0];
        let span = tracer_payload.chunks()[0].spans()[0].clone();
        (span, tracer_payload.strings().to_vec())
    }

    #[test]
    fn otlp_links_and_events_are_encoded_as_span_links_and_span_events() {
        use std::num::NonZeroUsize;

        use otlp_protos::opentelemetry::proto::common::v1::{any_value::Value, AnyValue, ArrayValue, KeyValue};
        use otlp_protos::opentelemetry::proto::trace::v1::span::{Event, Link};
        use stringtheory::interning::GenericMapInterner;

        use crate::common::otlp::traces::transform::{otlp_events_to_span_events, otlp_links_to_span_links};

        let kv = |key: &str, value: Value| KeyValue {
            key: key.to_string(),
            value: Some(AnyValue { value: Some(value) }),
        };
        let link = Link {
            trace_id: (1..=16).collect(),
            span_id: vec![0, 0, 0, 0, 0, 0, 0, 9],
            trace_state: "vendor=value".to_string(),
            // The low 8 bits are the W3C trace flags; bit 8 is OTLP's `is_remote`, which Datadog doesn't carry.
            flags: 0x101,
            attributes: vec![
                kv("k", Value::StringValue("v".to_string())),
                kv("n", Value::IntValue(42)),
                kv(
                    "list",
                    Value::ArrayValue(ArrayValue {
                        values: vec![AnyValue {
                            value: Some(Value::BoolValue(true)),
                        }],
                    }),
                ),
            ],
            ..Default::default()
        };
        let event = Event {
            time_unix_nano: 1_700_000_000_123,
            name: "exception".to_string(),
            attributes: vec![
                kv("message", Value::StringValue("boom".to_string())),
                kv("count", Value::IntValue(3)),
                kv("ratio", Value::DoubleValue(0.5)),
                kv("fatal", Value::BoolValue(false)),
                kv(
                    "codes",
                    Value::ArrayValue(ArrayValue {
                        values: vec![
                            AnyValue {
                                value: Some(Value::IntValue(1)),
                            },
                            AnyValue {
                                value: Some(Value::IntValue(2)),
                            },
                        ],
                    }),
                ),
            ],
            ..Default::default()
        };

        let interner = GenericMapInterner::new(NonZeroUsize::new(64 * 1024).unwrap());
        let span = make_trace(TraceIngestInfo::Datadog).spans()[0]
            .clone()
            .with_span_links(otlp_links_to_span_links(&[link], &interner))
            .with_span_events(otlp_events_to_span_events(&[event], &interner));
        let mut trace = Trace::new(vec![span], TraceIngestInfo::Datadog);
        trace.priority = Some(1);

        let mut buf = Vec::new();
        make_encoder(false)
            .encode(&trace, &mut buf)
            .expect("encode should succeed");
        let (span, strings) = decode_etp_first_span(&buf);

        // Resolves a string-table ref, or a string-valued attribute value.
        let text = |string_ref: u32| resolve_ref(&strings, string_ref).to_string();
        let attribute = |attributes: &HashMap<u32, idx::AnyValue>, key: &str| {
            attributes
                .iter()
                .find(|(k_ref, _)| resolve_ref(&strings, **k_ref) == key)
                .map(|(_, value)| value.value.clone().expect("attribute has a value"))
        };

        assert_eq!(span.links().len(), 1);
        let link = &span.links()[0];
        let trace_id: Vec<u8> = (1..=16).collect();
        assert_eq!(
            link.traceID(),
            trace_id.as_slice(),
            "128-bit trace ID, high bytes first"
        );
        assert_eq!(link.spanID(), 9);
        assert_eq!(text(link.tracestateRef()), "vendor=value");
        assert_eq!(
            link.flags(),
            0x8000_0001,
            "trace flags kept, is_remote dropped, bit 31 marks them present"
        );
        assert!(matches!(
            attribute(link.attributes(), "k"),
            Some(idx::any_value::Value::StringValueRef(v_ref)) if text(v_ref) == "v"
        ));
        // Link attributes are strings in the Datadog model, so the int is stringified and the array is flattened.
        assert!(matches!(
            attribute(link.attributes(), "n"),
            Some(idx::any_value::Value::StringValueRef(v_ref)) if text(v_ref) == "42"
        ));
        assert!(matches!(
            attribute(link.attributes(), "list.0"),
            Some(idx::any_value::Value::StringValueRef(v_ref)) if text(v_ref) == "true"
        ));

        assert_eq!(span.events().len(), 1);
        let event = &span.events()[0];
        assert_eq!(event.time(), 1_700_000_000_123);
        assert_eq!(text(event.nameRef()), "exception");
        assert!(matches!(
            attribute(event.attributes(), "message"),
            Some(idx::any_value::Value::StringValueRef(v_ref)) if text(v_ref) == "boom"
        ));
        assert!(matches!(
            attribute(event.attributes(), "count"),
            Some(idx::any_value::Value::IntValue(3))
        ));
        assert!(
            matches!(attribute(event.attributes(), "ratio"), Some(idx::any_value::Value::DoubleValue(r)) if r == 0.5)
        );
        assert!(matches!(
            attribute(event.attributes(), "fatal"),
            Some(idx::any_value::Value::BoolValue(false))
        ));
        match attribute(event.attributes(), "codes") {
            Some(idx::any_value::Value::ArrayValue(array)) => {
                let ints: Vec<_> = array
                    .values()
                    .iter()
                    .map(|v| match v.value {
                        Some(idx::any_value::Value::IntValue(i)) => i,
                        ref other => panic!("array values keep their type, got {other:?}"),
                    })
                    .collect();
                assert_eq!(ints, [1, 2]);
            }
            other => panic!("typed arrays stay arrays on the wire, got {other:?}"),
        }
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

    /// Resolves the hostname and string-valued payload attributes of the single ETP tracer payload in `buf`.
    fn decode_etp_payload_metadata(buf: &[u8]) -> (String, HashMap<String, String>) {
        let payload = AgentPayload::parse_from_bytes(buf).expect("AgentPayload should decode");
        let [tp] = payload.idxTracerPayloads.as_slice() else {
            panic!("expected one tracer payload");
        };
        let strings = tp.strings();
        (
            resolve_ref(strings, tp.hostnameRef()).to_string(),
            string_attrs(tp.attributes(), strings),
        )
    }

    #[test]
    fn datadog_trace_with_empty_hostname_does_not_fall_back_to_agent_hostname() {
        let mut encoder = make_encoder(false);
        let mut buf = Vec::new();
        encoder
            .encode(&make_trace(TraceIngestInfo::Datadog), &mut buf)
            .expect("encode should succeed");
        let (hostname, _) = decode_etp_payload_metadata(&buf);
        assert_eq!(hostname, "");

        let mut trace = make_trace(TraceIngestInfo::Datadog);
        trace.payload.hostname = MetaString::from("tracer-host");
        buf.clear();
        encoder.encode(&trace, &mut buf).expect("encode should succeed");
        let (hostname, _) = decode_etp_payload_metadata(&buf);
        assert_eq!(hostname, "tracer-host");
    }

    #[test]
    fn datadog_trace_payload_metadata_becomes_payload_attributes() {
        let mut encoder = make_encoder(false);
        let mut buf = Vec::new();
        encoder
            .encode(&make_trace(TraceIngestInfo::Datadog), &mut buf)
            .expect("encode should succeed");
        let (_, attrs) = decode_etp_payload_metadata(&buf);
        assert!(attrs.is_empty(), "empty metadata is not emitted: {attrs:?}");

        let mut trace = make_trace(TraceIngestInfo::Datadog);
        trace.payload.receiver = Some(Arc::new(ReceiverMetadata {
            container_tags: MetaString::from("image_tag:1.0,kube_namespace:prod"),
            process_tags: MetaString::from("entrypoint.name:app"),
            sdk_otlp_export: MetaString::from("false"),
            ..ReceiverMetadata::new()
        }));
        // OTLP-style resource attributes don't describe Datadog traces, so they never become container tags.
        Arc::make_mut(&mut trace.attributes).insert(
            MetaString::from("container.id"),
            AttributeValue::String(MetaString::from("abc123")),
        );
        buf.clear();
        encoder.encode(&trace, &mut buf).expect("encode should succeed");
        let (_, attrs) = decode_etp_payload_metadata(&buf);
        assert_eq!(
            attrs,
            HashMap::from([
                (
                    "_dd.tags.container".to_string(),
                    "image_tag:1.0,kube_namespace:prod".to_string()
                ),
                ("_dd.tags.process".to_string(), "entrypoint.name:app".to_string()),
                ("_dd.sdk.otlp_export".to_string(), "false".to_string()),
            ])
        );
    }

    #[test]
    fn resolve_hostname_prefers_the_trace_hostname_then_the_ingest_default() {
        // A hostname carried by the trace always wins.
        assert_eq!(
            Some("tracer-host"),
            resolve_hostname(Some("tracer-host"), true, Some("default"), true)
        );
        // Without one, OTLP traces and ignored missing fields report an empty hostname.
        assert_eq!(Some(""), resolve_hostname(None, true, Some("default"), false));
        assert_eq!(Some(""), resolve_hostname(None, false, Some("default"), true));
        // Other traces fall back to the default hostname, which may itself be absent.
        assert_eq!(Some("default"), resolve_hostname(None, false, Some("default"), false));
        assert_eq!(None, resolve_hostname(None, false, None, false));
    }

    #[test]
    fn encoded_otlp_hostname_comes_from_the_payload_then_the_root_span() {
        let with_root_hostname = |payload_hostname: &str| {
            let mut attributes = FastHashMap::default();
            attributes.insert(
                MetaString::from("_dd.hostname"),
                AttributeValue::String(MetaString::from("root-host")),
            );
            let span = DdSpan::new("svc", "op", "res", "web", 1, 0, 0, 1000, 0).with_attributes(attributes);
            let mut trace = Trace::new(
                vec![span],
                TraceIngestInfo::Otlp {
                    receiver_sampling_rate: 1.0,
                },
            );
            trace.priority = Some(1);
            trace.payload.hostname = MetaString::from(payload_hostname);
            trace
        };

        // The payload hostname beats the root span.
        let trace = with_root_hostname("payload-host");
        assert_eq!(encoded_hostname(&trace), "payload-host");

        // The root span applies only when the payload hostname is empty.
        let trace = with_root_hostname("");
        assert_eq!(encoded_hostname(&trace), "root-host");
    }

    #[test]
    fn encoded_hostname_is_empty_for_otlp_traces_without_one() {
        // The translator leaves the hostname empty for serverless resources.
        let trace = make_trace(TraceIngestInfo::Otlp {
            receiver_sampling_rate: 1.0,
        });
        assert_eq!(encoded_hostname(&trace), "");

        // Datadog traces without one stay empty too, as the trace-agent writer leaves them.
        let trace = make_trace(TraceIngestInfo::Datadog);
        assert_eq!(encoded_hostname(&trace), "");
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
    fn resolve_container_tags_merges_explicit_and_extracted_tags() {
        let mut attributes = FastHashMap::default();
        attributes.insert(
            MetaString::from(KEY_DATADOG_CONTAINER_TAGS),
            AttributeValue::String(MetaString::from("team:core")),
        );
        attributes.insert(
            MetaString::from("kube_namespace"),
            AttributeValue::String(MetaString::from("payments")),
        );

        // An explicit, non-empty `datadog.container_tags` attribute is emitted, and the tags resolved from the
        // resource are appended rather than replacing it.
        let merged = resolve_container_tags_from_attrs(&attributes, &[], None).expect("container tags should resolve");
        assert_eq!(merged.as_ref(), "team:core,kube_namespace:payments");

        // The explicit attribute alone still resolves.
        attributes.remove(&MetaString::from("kube_namespace"));
        assert_eq!(
            resolve_container_tags_from_attrs(&attributes, &[], None).map(|tags| tags.to_string()),
            Some("team:core".to_string())
        );
    }

    #[test]
    fn resolve_container_tags_returns_none_without_container_attributes() {
        // With no container attributes, resolved tags, or Fargate source there is nothing to emit.
        let attributes = FastHashMap::default();
        assert_eq!(None, resolve_container_tags_from_attrs(&attributes, &[], None));
    }

    #[test]
    fn resolve_container_tags_uses_the_tags_the_source_resolved() {
        let resolved = [
            MetaString::from("image_tag:v1"),
            MetaString::from("kube_namespace:payments"),
        ];
        let attributes = FastHashMap::from_iter([(
            MetaString::from("datadog.container.tag.team"),
            AttributeValue::String(MetaString::from("core")),
        )]);

        // The resolved tags stand in for the attributes, which would otherwise yield `team:core`.
        let tags = resolve_container_tags_from_attrs(&attributes, &resolved, None).expect("tags");
        assert_eq!(
            BTreeSet::from(["image_tag:v1", "kube_namespace:payments"]),
            tags.split(',').collect::<BTreeSet<_>>()
        );

        // An explicit `datadog.container_tags` attribute is emitted along with the resolved tags.
        let explicit = FastHashMap::from_iter([(
            MetaString::from(KEY_DATADOG_CONTAINER_TAGS),
            AttributeValue::String(MetaString::from("region:us")),
        )]);
        let merged = resolve_container_tags_from_attrs(&explicit, &resolved, None).expect("tags");
        assert_eq!(
            BTreeSet::from(["region:us", "image_tag:v1", "kube_namespace:payments"]),
            merged.split(',').collect::<BTreeSet<_>>()
        );
    }

    /// Translates a single-span `ResourceSpans` with the given resource attributes, encodes the trace, and returns
    /// the decoded string-valued tracer payload attributes and span attributes.
    fn encode_translated_trace(
        ignore_missing_fields: bool, encoder: TraceEndpointEncoder, resource_attrs: &[(&str, &str)],
    ) -> (HashMap<String, String>, Vec<HashMap<String, String>>) {
        use datadog_agent_apm::semantics::SemanticRegistryProvider;
        use otlp_protos::opentelemetry::proto::common::v1::{any_value::Value, AnyValue, KeyValue};
        use otlp_protos::opentelemetry::proto::resource::v1::Resource;
        use otlp_protos::opentelemetry::proto::trace::v1::{ResourceSpans, ScopeSpans, Span as OtlpSpan};

        use crate::common::otlp::origin::OtlpOriginTagResolver;
        use crate::common::otlp::traces::translator::{traces_settings, OtlpTracesSettings, OtlpTracesTranslator};
        use crate::common::otlp::Metrics;

        let kv = |(key, value): (&str, &str)| KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(Value::StringValue(value.to_string())),
            }),
        };
        let resource_spans = ResourceSpans {
            resource: Some(Resource {
                attributes: resource_attrs.iter().map(|pair| kv(*pair)).collect(),
                ..Default::default()
            }),
            scope_spans: vec![ScopeSpans {
                spans: vec![OtlpSpan {
                    trace_id: vec![1; 16],
                    span_id: vec![1; 8],
                    name: "span".to_owned(),
                    end_time_unix_nano: 2,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };

        let settings = OtlpTracesSettings {
            ignore_missing_datadog_fields: ignore_missing_fields,
            ..traces_settings()
        };
        let mut translator = OtlpTracesTranslator::new(
            settings,
            5000,
            SemanticRegistryProvider::default(),
            OtlpOriginTagResolver::new(Arc::new(TestWorkloadProvider::new())),
        );
        let traces: Vec<Trace> = translator
            .translate_spans(resource_spans, &Metrics::for_tests())
            .filter_map(Event::try_into_trace)
            .collect();
        assert_eq!(traces.len(), 1);

        let mut buf = Vec::new();
        let mut encoder = encoder;
        encoder.encode(&traces[0], &mut buf).expect("encode should succeed");
        let payload = AgentPayload::parse_from_bytes(&buf).expect("AgentPayload should decode");
        let tracer_payload = &payload.idxTracerPayloads[0];

        let payload_attrs = string_attrs(tracer_payload.attributes(), tracer_payload.strings());
        let span_attrs = tracer_payload
            .chunks()
            .iter()
            .flat_map(|chunk| chunk.spans().iter())
            .map(|span| string_attrs(span.attributes(), tracer_payload.strings()))
            .collect();
        (payload_attrs, span_attrs)
    }

    #[test]
    fn consumed_resource_keys_reach_the_payload_container_tags_with_explicit_tags() {
        // An explicit `datadog.container_tags` on the resource emits that string, and the resolved container tags
        // are appended instead of replacing it. Neither reaches span meta.
        let (payload_attrs, span_attrs) = encode_translated_trace(
            false,
            make_encoder(false),
            &[("datadog.container_tags", "team:core"), ("kube_namespace", "payments")],
        );

        let container_tags = &payload_attrs[CONTAINER_TAGS_META_KEY];
        assert_eq!(container_tags, "team:core,kube_namespace:payments");
        assert!(span_attrs
            .iter()
            .all(|attrs| !attrs.contains_key("kube_namespace") && !attrs.contains_key("datadog.container_tags")));
    }

    #[test]
    fn consumed_resource_keys_reach_the_payload_container_tags_when_ignoring_missing_fields() {
        // Container tags are resolved even when missing Datadog fields are ignored, as the trace agent's receiver
        // does: the setting does not gate extraction there.
        let encoder = TraceEndpointEncoder::new(
            MetaString::from("test-host"),
            "0.0.0".to_string(),
            "none".to_string(),
            DEFAULT_TARGET_TPS,
            DEFAULT_ERRORS_PER_SECOND,
            false,
            true,
        );
        let (payload_attrs, span_attrs) = encode_translated_trace(true, encoder, &[("kube_namespace", "payments")]);

        assert_eq!(payload_attrs[CONTAINER_TAGS_META_KEY], "kube_namespace:payments");
        assert!(span_attrs.iter().all(|attrs| !attrs.contains_key("kube_namespace")));
    }

    #[test]
    fn otlp_image_tags_array_reaches_the_encoded_payload_container_tags() {
        use datadog_agent_apm::semantics::SemanticRegistryProvider;
        use otlp_protos::opentelemetry::proto::common::v1::{any_value::Value, AnyValue, ArrayValue, KeyValue};
        use otlp_protos::opentelemetry::proto::resource::v1::Resource;
        use otlp_protos::opentelemetry::proto::trace::v1::{ResourceSpans, ScopeSpans, Span as OtlpSpan};
        use saluki_core::data_model::event::Event;

        use crate::common::otlp::origin::OtlpOriginTagResolver;
        use crate::common::otlp::traces::translator::{traces_settings, OtlpTracesTranslator};
        use crate::common::otlp::Metrics;

        let string_value = |value: &str| AnyValue {
            value: Some(Value::StringValue(value.to_string())),
        };
        let resource_spans = ResourceSpans {
            resource: Some(Resource {
                attributes: vec![
                    KeyValue {
                        key: "container.image.tags".to_string(),
                        value: Some(AnyValue {
                            value: Some(Value::ArrayValue(ArrayValue {
                                values: vec![string_value("v1"), string_value("latest")],
                            })),
                        }),
                    },
                    KeyValue {
                        key: "container.name".to_string(),
                        value: Some(string_value("web")),
                    },
                ],
                ..Default::default()
            }),
            scope_spans: vec![ScopeSpans {
                spans: vec![OtlpSpan {
                    trace_id: vec![1; 16],
                    span_id: vec![2; 8],
                    name: "span".to_string(),
                    end_time_unix_nano: 2,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut translator = OtlpTracesTranslator::new(
            traces_settings(),
            5000,
            SemanticRegistryProvider::default(),
            OtlpOriginTagResolver::new(Arc::new(TestWorkloadProvider::new())),
        );
        let trace = translator
            .translate_spans(resource_spans, &Metrics::for_tests())
            .find_map(Event::try_into_trace)
            .expect("the span should translate into a trace");

        let mut buf = Vec::new();
        make_encoder(false)
            .encode(&trace, &mut buf)
            .expect("encode should succeed");
        let payload = AgentPayload::parse_from_bytes(&buf).expect("AgentPayload should decode");
        let tracer_payload = &payload.idxTracerPayloads[0];
        let attrs = string_attrs(tracer_payload.attributes(), tracer_payload.strings());
        let container_tags = attrs
            .get(CONTAINER_TAGS_META_KEY)
            .expect("container tags should be emitted");
        assert_eq!(
            BTreeSet::from(["container_name:web", "image_tag:v1"]),
            container_tags.split(',').collect::<BTreeSet<_>>()
        );
    }

    /// `ignore_missing_datadog_fields` is an OTLP setting, so a Datadog trace encodes the same either way.
    #[test]
    fn ignore_missing_datadog_fields_does_not_change_datadog_traces() {
        let encode = |ingest_info, ignore_missing_datadog_fields| {
            let mut encoder = TraceEndpointEncoder::new(
                MetaString::from("test-host"),
                "0.0.0".to_string(),
                "none".to_string(),
                DEFAULT_TARGET_TPS,
                DEFAULT_ERRORS_PER_SECOND,
                false,
                ignore_missing_datadog_fields,
            );
            let mut buf = Vec::new();
            encoder
                .encode(&make_trace(ingest_info), &mut buf)
                .expect("encode should succeed");
            buf
        };

        assert_eq!(
            encode(TraceIngestInfo::Datadog, true),
            encode(TraceIngestInfo::Datadog, false)
        );
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

    fn make_span(span_id: u64, analyzed: bool) -> DdSpan {
        let span = DdSpan::new(
            MetaString::from("svc"),
            MetaString::from("op"),
            MetaString::from("res"),
            MetaString::from("web"),
            span_id,
            0,    // parent_id
            0,    // start
            1000, // duration
            0,    // error
        );
        if analyzed {
            span.with_attributes(FastHashMap::from_iter([(
                MetaString::from(KEY_ANALYZED_SPANS),
                AttributeValue::Float(1.0),
            )]))
        } else {
            span
        }
    }

    fn make_sampled_trace(spans: Vec<DdSpan>, dropped_trace: bool) -> Trace {
        let mut trace = Trace::new(spans, TraceIngestInfo::Datadog);
        trace.priority = Some(1);
        trace.dropped_trace = dropped_trace;
        trace
    }

    /// Builds the key of a payload telemetry metric, with the labels that `make_payload_telemetry` uses.
    fn payload_metric_key(name: &'static str) -> metrics::Key {
        metrics::Key::from_parts(
            name,
            vec![
                metrics::Label::new("component_id", "dd_traces_encode"),
                metrics::Label::new("component_type", "encoder"),
            ],
        )
    }

    fn make_payload_telemetry() -> TracePayloadTelemetry {
        TracePayloadTelemetry::new("dd_traces_encode", "encoder")
    }

    #[test]
    fn largest_payload_gauge_only_moves_up() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let telemetry = make_payload_telemetry();
        assert_eq!(
            TracePayloadTelemetry::payload_size_bytes_max_name(),
            "component_payload_size_bytes_max"
        );

        for (body_len, expected_largest) in [(100_000, 100_000.0), (3_000_000, 3_000_000.0), (50_000, 3_000_000.0)] {
            telemetry.track_payload(body_len, body_len);
            assert_eq!(
                recorder.gauge(payload_metric_key("component_payload_size_bytes_max")),
                Some(expected_largest)
            );
        }
    }

    #[test]
    fn trace_counts_follow_the_sampling_decision() {
        // A kept trace counts every span as a span, analyzed or not, and no events.
        let kept = make_sampled_trace(vec![make_span(1, true), make_span(2, false)], false);
        assert_eq!(TraceCounts::from_trace(&kept), TraceCounts { spans: 2, events: 0 });

        // A dropped trace counts only its analyzed spans, as events.
        let dropped = make_sampled_trace(vec![make_span(1, true), make_span(2, true), make_span(3, false)], true);
        assert_eq!(TraceCounts::from_trace(&dropped), TraceCounts { spans: 0, events: 2 });
    }

    #[tokio::test]
    async fn request_builder_tracks_written_traces_and_payload_sizes() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let metrics_builder = MetricsBuilder::default();

        let traces = vec![
            make_sampled_trace(vec![make_span(1, false), make_span(2, true)], false),
            make_sampled_trace(vec![make_span(3, true), make_span(4, false)], true),
            make_sampled_trace(vec![make_span(5, false)], false),
        ];

        // Without compression or framing, the payload before compression is every trace encoded back to back.
        let expected_uncompressed_len = traces
            .iter()
            .map(|trace| {
                let mut buf = Vec::new();
                make_encoder(false)
                    .encode(trace, &mut buf)
                    .expect("encode should succeed");
                buf.len() as u64
            })
            .sum::<u64>();

        let request_builder = RequestBuilder::new(make_encoder(false), CompressionScheme::noop(), RB_BUFFER_CHUNK_SIZE)
            .await
            .expect("request builder should be created");
        let (events_tx, events_rx) = mpsc::channel(1);
        let (payloads_tx, mut payloads_rx) = mpsc::channel(8);

        let mut events = EventsBuffer::default();
        for trace in traces {
            assert!(events.try_push(Event::Trace(trace)).is_none());
        }
        events_tx.send(events).await.expect("events should be sent");
        drop(events_tx);

        run_request_builder(
            request_builder,
            ComponentTelemetry::from_builder(&metrics_builder),
            make_payload_telemetry(),
            events_rx,
            payloads_tx,
            Duration::from_millis(1),
        )
        .await
        .expect("request builder should run to completion");

        let mut body_lens = Vec::new();
        while let Some(payload) = payloads_rx.recv().await {
            let http_payload = payload.try_into_http_payload().expect("payload should be HTTP");
            let (_, request) = http_payload.into_parts();
            body_lens.push(request.body().len() as f64);
        }
        assert_eq!(body_lens.len(), 1);
        assert_eq!(
            recorder.histogram(payload_metric_key("component_payload_size_bytes")),
            Some(body_lens.clone())
        );
        assert_eq!(
            recorder.gauge(payload_metric_key("component_payload_size_bytes_max")),
            Some(body_lens[0])
        );

        assert_eq!(
            recorder.counter(payload_metric_key("component_traces_encoded_total")),
            Some(3)
        );
        assert_eq!(
            recorder.counter(payload_metric_key("component_spans_encoded_total")),
            Some(3)
        );
        assert_eq!(
            recorder.counter(payload_metric_key("component_trace_events_encoded_total")),
            Some(1)
        );
        assert_eq!(
            recorder.counter(payload_metric_key("component_bytes_uncompressed_total")),
            Some(expected_uncompressed_len)
        );
    }

    /// Resolves the env and app version of every ETP tracer payload in an encoded `AgentPayload`.
    fn decode_etp_env_and_app_versions(buf: &[u8]) -> Vec<(String, String)> {
        let payload = AgentPayload::parse_from_bytes(buf).expect("AgentPayload should decode");
        payload
            .idxTracerPayloads
            .iter()
            .map(|tp| {
                (
                    resolve_ref(tp.strings(), tp.envRef()).to_string(),
                    resolve_ref(tp.strings(), tp.appVersionRef()).to_string(),
                )
            })
            .collect()
    }

    fn encoder_with_container(container_id: &str, tags: &[&str]) -> TraceEndpointEncoder {
        let provider = TestWorkloadProvider::with_entity(EntityId::Container(MetaString::from(container_id)), tags);
        make_encoder(false).with_container_tag_lookup(ContainerTagLookup(Some(Arc::new(provider))))
    }

    #[test]
    fn app_version_falls_back_to_container_version_tag() {
        let mut encoder = encoder_with_container("cid-1", &["image_tag:1.2.3", "version:container-v"]);
        let encode = |encoder: &mut TraceEndpointEncoder, trace: &Trace| {
            let mut buf = Vec::new();
            encoder.encode(trace, &mut buf).expect("encode should succeed");
            decode_etp_env_and_app_versions(&buf)[0].1.clone()
        };

        // A payload with no version takes the container's.
        let mut trace = make_trace(TraceIngestInfo::Datadog);
        trace.payload.container_id = MetaString::from("cid-1");
        assert_eq!(encode(&mut encoder, &trace), "container-v");

        // The payload's own version wins.
        trace.payload.app_version = MetaString::from("payload-v");
        assert_eq!(encode(&mut encoder, &trace), "payload-v");

        // A container the provider doesn't know yet supplies nothing.
        let mut trace = make_trace(TraceIngestInfo::Datadog);
        trace.payload.container_id = MetaString::from("cid-unknown");
        assert_eq!(encode(&mut encoder, &trace), "");

        // Neither does a payload without a container ID.
        let trace = make_trace(TraceIngestInfo::Datadog);
        assert_eq!(encode(&mut encoder, &trace), "");
    }

    #[test]
    fn app_version_falls_back_to_the_traces_own_container_tag_attributes() {
        // An OTLP trace carries its container's tags as attributes, and the tagger may not know the container yet.
        let mut encoder = make_encoder(false);
        let mut trace = make_trace(TraceIngestInfo::Datadog);
        trace.payload.container_id = MetaString::from("cid-1");
        trace.attributes = Arc::new(FastHashMap::from_iter([(
            MetaString::from("datadog.container.tag.version"),
            AttributeValue::String(MetaString::from("attribute-v")),
        )]));

        let mut buf = Vec::new();
        encoder.encode(&trace, &mut buf).expect("encode should succeed");
        assert_eq!(decode_etp_env_and_app_versions(&buf)[0].1, "attribute-v");

        // Without a container ID, the attributes don't apply.
        trace.payload.container_id = MetaString::default();
        let mut buf = Vec::new();
        encoder.encode(&trace, &mut buf).expect("encode should succeed");
        assert_eq!(decode_etp_env_and_app_versions(&buf)[0].1, "");
    }

    #[test]
    fn container_version_lookup_leaves_container_tags_untouched() {
        let mut encoder = encoder_with_container("cid-1", &["version:container-v", "kube_namespace:default"]);
        let mut trace = make_trace(TraceIngestInfo::Datadog);
        trace.payload.container_id = MetaString::from("cid-1");

        let mut buf = Vec::new();
        encoder.encode(&trace, &mut buf).expect("encode should succeed");
        let payload = AgentPayload::parse_from_bytes(&buf).expect("AgentPayload should decode");
        let tracer_payload = &payload.idxTracerPayloads[0];
        let attrs = string_attrs(tracer_payload.attributes(), tracer_payload.strings());
        assert!(!attrs.contains_key(CONTAINER_TAGS_META_KEY));
    }

    #[test]
    fn payload_env_is_normalized() {
        let cases = [
            ("prod", "prod"),
            ("Prod Env", "prod_env"),
            ("STAGING", "staging"),
            ("!!!", ""),
            ("", ""),
        ];
        for (env, expected) in cases {
            let mut trace = make_trace(TraceIngestInfo::Datadog);
            trace.payload.env = MetaString::from(env);

            let mut buf = Vec::new();
            make_encoder(false)
                .encode(&trace, &mut buf)
                .expect("encode should succeed");
            assert_eq!(decode_etp_env_and_app_versions(&buf)[0].0, expected, "env {env:?}");
        }
    }
}
