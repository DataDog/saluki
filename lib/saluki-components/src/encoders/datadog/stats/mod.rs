//! Datadog APM Stats encoder.

use std::time::Duration;

use async_trait::async_trait;
use datadog_protos::traces::{
    ClientGroupedStats as ProtoClientGroupedStats, ClientStatsBucket as ProtoClientStatsBucket,
    ClientStatsPayload as ProtoClientStatsPayload, StatsPayload as ProtoStatsPayload, Trilean,
};
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::task::HandleExt as _;
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{encoders::*, ComponentContext},
    data_model::{
        event::{
            trace_stats::{ClientGroupedStats, ClientStatsBucket, ClientStatsPayload, TraceStats},
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
    io::RB_BUFFER_CHUNK_SIZE,
    request_builder::{EndpointEncoder, RequestBuilder},
    telemetry::ComponentTelemetry,
    DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT, DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT,
};

const MAX_STATS_PER_PAYLOAD: usize = 4000;
static CONTENT_TYPE_MSGPACK: HeaderValue = HeaderValue::from_static("application/msgpack");

const fn default_flush_timeout_secs() -> u64 {
    2
}

fn default_env() -> String {
    "none".to_string()
}

/// Configuration for the Datadog APM Stats encoder.
#[derive(Deserialize)]
pub struct DatadogApmStatsEncoderConfiguration {
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
    agent_hostname: Option<String>,

    #[serde(skip)]
    agent_version: String,

    #[serde(default = "default_env")]
    env: String,
}

impl DatadogApmStatsEncoderConfiguration {
    /// Creates a new `DatadogApmStatsEncoderConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let mut stats_config: Self = config.as_typed()?;
        let app_details = saluki_metadata::get_app_details();
        stats_config.agent_version = format!("agent-data-plane/{}", app_details.version().raw());

        Ok(stats_config)
    }

    /// Sets the agent hostname using the environment provider.
    pub async fn with_environment_provider<E>(mut self, environment_provider: E) -> Result<Self, GenericError>
    where
        E: EnvironmentProvider<Host = BoxedHostProvider>,
    {
        let host_provider = environment_provider.host();
        let hostname = host_provider.get_hostname().await?;
        self.agent_hostname = Some(hostname);
        Ok(self)
    }
}

#[async_trait]
impl EncoderBuilder for DatadogApmStatsEncoderConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::TraceStats
    }

    fn output_payload_type(&self) -> PayloadType {
        PayloadType::Http
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Encoder + Send>, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let compression_scheme = CompressionScheme::gzip_default();

        let agent_hostname = MetaString::from(self.agent_hostname.clone().unwrap_or_default());
        let agent_version = MetaString::from(self.agent_version.clone());
        let agent_env = MetaString::from(self.env.clone());

        let mut stats_rb = RequestBuilder::new(
            StatsEndpointEncoder::new(agent_hostname, agent_version, agent_env),
            compression_scheme,
            RB_BUFFER_CHUNK_SIZE,
        )
        .await?;
        stats_rb.with_max_inputs_per_payload(MAX_STATS_PER_PAYLOAD);

        let flush_timeout = match self.flush_timeout_secs {
            // We always give ourselves a minimum flush timeout of 10ms to allow for some very minimal amount of
            // batching, while still practically flushing things almost immediately.
            0 => Duration::from_millis(10),
            secs => Duration::from_secs(secs),
        };

        Ok(Box::new(DatadogStats {
            stats_rb,
            telemetry,
            flush_timeout,
        }))
    }
}

impl MemoryBounds for DatadogApmStatsEncoderConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<DatadogStats>("component struct")
            .with_array::<EventsBuffer>("request builder events channel", 8)
            .with_array::<PayloadsBuffer>("request builder payloads channel", 8);

        builder
            .firm()
            .with_array::<TraceStats>("stats split re-encode buffer", MAX_STATS_PER_PAYLOAD);
    }
}

pub struct DatadogStats {
    stats_rb: RequestBuilder<StatsEndpointEncoder>,
    telemetry: ComponentTelemetry,
    flush_timeout: Duration,
}

#[async_trait]
impl Encoder for DatadogStats {
    async fn run(mut self: Box<Self>, mut context: EncoderContext) -> Result<(), GenericError> {
        let Self {
            stats_rb,
            telemetry,
            flush_timeout,
        } = *self;

        let mut health = context.take_health_handle();

        let (events_tx, events_rx) = mpsc::channel(8);
        let (payloads_tx, mut payloads_rx) = mpsc::channel(8);

        let request_builder_fut = run_request_builder(stats_rb, telemetry, events_rx, payloads_tx, flush_timeout);
        let request_builder_handle = context
            .topology_context()
            .global_thread_pool()
            .spawn_traced_named("dd-stats-request-builder", request_builder_fut);

        health.mark_ready();
        debug!("Datadog APM Stats encoder started.");

        loop {
            select! {
                biased;

                _ = health.live() => continue,
                maybe_payload = payloads_rx.recv() => match maybe_payload {
                    Some(payload) => {
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

        debug!("Datadog APM Stats encoder stopped.");

        Ok(())
    }
}

async fn run_request_builder(
    mut stats_request_builder: RequestBuilder<StatsEndpointEncoder>, telemetry: ComponentTelemetry,
    mut events_rx: Receiver<EventsBuffer>, payloads_tx: Sender<PayloadsBuffer>, flush_timeout: std::time::Duration,
) -> Result<(), GenericError> {
    let mut pending_flush = false;
    let pending_flush_timeout = sleep(flush_timeout);
    tokio::pin!(pending_flush_timeout);

    loop {
        select! {
            Some(event_buffer) = events_rx.recv() => {
                for event in event_buffer {
                    let trace_stats = match event.try_into_trace_stats() {
                        Some(stats) => stats,
                        None => continue,
                    };

                    // Encode the stats. If we get it back, that means the current request is full, and we need to
                    // flush it before we can try to encode the stats again.
                    let stats_to_retry = match stats_request_builder.encode(trace_stats).await {
                        Ok(None) => continue,
                        Ok(Some(stats)) => stats,
                        Err(e) => {
                            error!(error = %e, "Failed to encode stats.");
                            telemetry.events_dropped_encoder().increment(1);
                            continue;
                        }
                    };

                    let maybe_requests = stats_request_builder.flush().await;
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
                                continue;
                            } else {
                                return Err(GenericError::from(e).context("Failed to flush request."));
                            }
                        }
                    }

                    if let Err(e) = stats_request_builder.encode(stats_to_retry).await {
                        error!(error = %e, "Failed to encode stats.");
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

                let maybe_stats_requests = stats_request_builder.flush().await;
                for maybe_request in maybe_stats_requests {
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

            else => break,
        }
    }

    Ok(())
}

#[derive(Debug)]
struct StatsEndpointEncoder {
    agent_hostname: MetaString,
    agent_version: MetaString,
    agent_env: MetaString,
}

impl StatsEndpointEncoder {
    fn new(agent_hostname: MetaString, agent_version: MetaString, agent_env: MetaString) -> Self {
        Self {
            agent_hostname,
            agent_version,
            agent_env,
        }
    }

    fn to_proto_stats_payload(&self, stats: &TraceStats) -> ProtoStatsPayload {
        let mut payload = ProtoStatsPayload::new();
        payload.set_agentHostname(self.agent_hostname.to_string());
        payload.set_agentEnv(self.agent_env.to_string());
        payload.set_agentVersion(self.agent_version.to_string());
        payload.set_clientComputed(false);
        payload.set_splitPayload(false);
        payload.set_stats(stats.stats().iter().map(convert_client_stats_payload).collect());

        payload
    }
}

fn convert_client_stats_payload(client_payload: &ClientStatsPayload) -> ProtoClientStatsPayload {
    let mut proto_client = ProtoClientStatsPayload::new();
    proto_client.set_hostname(client_payload.hostname().to_string());
    proto_client.set_env(client_payload.env().to_string());
    proto_client.set_version(client_payload.version().to_string());
    proto_client.set_lang(client_payload.lang().to_string());
    proto_client.set_tracerVersion(client_payload.tracer_version().to_string());
    proto_client.set_runtimeID(client_payload.runtime_id().to_string());
    proto_client.set_sequence(client_payload.sequence());
    proto_client.set_agentAggregation(client_payload.agent_aggregation().to_string());
    proto_client.set_service(client_payload.service().to_string());
    proto_client.set_containerID(client_payload.container_id().to_string());
    proto_client.set_tags(client_payload.tags().iter().map(|s| s.to_string()).collect());
    proto_client.set_git_commit_sha(client_payload.git_commit_sha().to_string());
    proto_client.set_image_tag(client_payload.image_tag().to_string());
    proto_client.set_process_tags_hash(client_payload.process_tags_hash());
    proto_client.set_process_tags(client_payload.process_tags().to_string());
    proto_client.set_stats(client_payload.stats().iter().map(convert_client_stats_bucket).collect());
    proto_client
}

fn convert_client_stats_bucket(bucket: &ClientStatsBucket) -> ProtoClientStatsBucket {
    let mut proto_bucket = ProtoClientStatsBucket::new();
    proto_bucket.set_start(bucket.start());
    proto_bucket.set_duration(bucket.duration());
    proto_bucket.set_agentTimeShift(bucket.agent_time_shift());
    proto_bucket.set_stats(bucket.stats().iter().map(convert_client_grouped_stats).collect());
    proto_bucket
}

fn convert_client_grouped_stats(grouped: &ClientGroupedStats) -> ProtoClientGroupedStats {
    let mut proto_grouped = ProtoClientGroupedStats::new();
    proto_grouped.set_service(grouped.service().to_string());
    proto_grouped.set_name(grouped.name().to_string());
    proto_grouped.set_resource(grouped.resource().to_string());
    proto_grouped.set_HTTP_status_code(grouped.http_status_code());
    proto_grouped.set_type(grouped.span_type().to_string());
    proto_grouped.set_DB_type(grouped.db_type().to_string());
    proto_grouped.set_hits(grouped.hits());
    proto_grouped.set_errors(grouped.errors());
    proto_grouped.set_duration(grouped.duration());
    proto_grouped.set_okSummary(grouped.ok_summary().to_vec());
    proto_grouped.set_errorSummary(grouped.error_summary().to_vec());
    proto_grouped.set_synthetics(grouped.synthetics());
    proto_grouped.set_topLevelHits(grouped.top_level_hits());
    proto_grouped.set_span_kind(grouped.span_kind().to_string());
    proto_grouped.set_peer_tags(grouped.peer_tags().iter().map(|s| s.to_string()).collect());
    proto_grouped.set_is_trace_root(match grouped.is_trace_root() {
        None => Trilean::NOT_SET,
        Some(true) => Trilean::TRUE,
        Some(false) => Trilean::FALSE,
    });
    proto_grouped.set_GRPC_status_code(grouped.grpc_status_code().to_string());
    proto_grouped.set_HTTP_method(grouped.http_method().to_string());
    proto_grouped.set_HTTP_endpoint(grouped.http_endpoint().to_string());
    proto_grouped
}

/// Error type for stats encoding.
#[derive(Debug)]
pub struct StatsEncodeError(rmp_serde::encode::Error);

impl std::fmt::Display for StatsEncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "failed to encode stats as MessagePack: {}", self.0)
    }
}

impl std::error::Error for StatsEncodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl EndpointEncoder for StatsEndpointEncoder {
    type Input = TraceStats;
    type EncodeError = StatsEncodeError;

    fn encoder_name() -> &'static str {
        "stats"
    }

    fn compressed_size_limit(&self) -> usize {
        DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT
    }

    fn uncompressed_size_limit(&self) -> usize {
        DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT
    }

    fn encode(&mut self, stats: &Self::Input, buffer: &mut Vec<u8>) -> Result<(), Self::EncodeError> {
        let payload = self.to_proto_stats_payload(stats);
        rmp_serde::encode::write_named(buffer, &payload).map_err(StatsEncodeError)?;
        Ok(())
    }

    fn endpoint_uri(&self) -> Uri {
        PathAndQuery::from_static("/api/v0.2/stats").into()
    }

    fn endpoint_method(&self) -> Method {
        Method::POST
    }

    fn content_type(&self) -> HeaderValue {
        CONTENT_TYPE_MSGPACK.clone()
    }
}
