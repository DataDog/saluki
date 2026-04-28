use std::collections::HashMap;
use std::pin::Pin;
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use datadog_protos::checks::{
    acr_ipc_server::{AcrIpc, AcrIpcServer},
    check_data::Data,
    log::LogLevel,
    metric::MetricType,
    service_check::Status as SvcStatus,
    CheckDataAck, CheckDataMsg, CheckResultMsg, ConfigData, Hello, HelloResp, SendCheckResultResponse,
    StreamConfigRequest,
};
use futures::stream::{self, Stream};
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::task::HandleExt as _;
use saluki_config::GenericConfiguration;
use saluki_context::tags::{Tag, TagSet};
use saluki_context::Context;
use saluki_core::data_model::event::eventd::EventD;
use saluki_core::data_model::event::log::Log;
use saluki_core::data_model::event::metric::Metric;
use saluki_core::data_model::event::service_check::{CheckStatus, ServiceCheck};
use saluki_core::data_model::event::{Event, EventType};
use saluki_core::topology::OutputDefinition;
use saluki_core::{
    components::{sources::*, ComponentContext},
    data_model::event::log::LogStatus,
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::ListenAddress;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use stringtheory::MetaString;
use tokio::select;
use tokio::sync::mpsc;
use tonic::transport::Server;
use tonic::{Response, Status};
use tracing::{debug, info, warn};

const fn default_grpc_endpoint() -> ListenAddress {
    ListenAddress::any_tcp(5105)
}

/// AcrIpc protocol version this server speaks. Bumped on
/// wire-incompatible changes; today the protocol is wire-additive so
/// we stay on 1.
const PROTOCOL_VERSION: u32 = 1;

/// Identifier surfaced in the Handshake response so a client can tell
/// which server flavor it connected to.
const SERVER_ID: &str = "saluki-checks-ipc";

// Named outputs the source dispatches into. Defined as constants so the
// `outputs()` declaration and the per-event dispatch match can't drift.
const OUTPUT_METRICS: &str = "metrics";
const OUTPUT_LOGS: &str = "logs";
const OUTPUT_EVENTS: &str = "events";
const OUTPUT_SERVICE_CHECKS: &str = "service_checks";

/// Checks IPC source.
#[derive(Debug, Deserialize)]
pub struct ChecksIPCConfiguration {
    #[serde(default = "default_grpc_endpoint")]
    grpc_endpoint: ListenAddress,
}

impl ChecksIPCConfiguration {
    /// Creates a new `ChecksIPCConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }
}

#[async_trait]
impl SourceBuilder for ChecksIPCConfiguration {
    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition<EventType>>> = LazyLock::new(|| {
            vec![
                OutputDefinition::named_output(OUTPUT_METRICS, EventType::Metric),
                OutputDefinition::named_output(OUTPUT_LOGS, EventType::Log),
                OutputDefinition::named_output(OUTPUT_EVENTS, EventType::EventD),
                OutputDefinition::named_output(OUTPUT_SERVICE_CHECKS, EventType::ServiceCheck),
            ]
        });

        &OUTPUTS
    }

    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(ChecksIPC {
            grpc_endpoint: self.grpc_endpoint.clone(),
        }))
    }
}

impl MemoryBounds for ChecksIPCConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // Capture the size of the heap allocation when the component is built.
        builder.minimum().with_single_value::<ChecksIPC>("checks_ipc");
    }
}

struct ChecksIPC {
    grpc_endpoint: ListenAddress,
}

#[async_trait]
impl Source for ChecksIPC {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let mut global_shutdown = context.take_shutdown_handle();
        let mut health = context.take_health_handle();

        let (events_tx, mut events_rx) = mpsc::channel(16);

        let grpc_server = Server::builder().add_service(AcrIpcServer::new(AcrIpcService { events_tx }));

        let grpc_socket_addr = match self.grpc_endpoint {
            ListenAddress::Tcp(addr) => addr,
            _ => return Err(generic_error!("AcrIpc gRPC endpoint must be a TCP address.")),
        };
        context
            .topology_context()
            .global_thread_pool()
            .spawn_traced_named("checks-ipc-grpc-server", grpc_server.serve(grpc_socket_addr));

        health.mark_ready();
        debug!("Checks IPC source started.");

        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break;
                },
                _ = health.live() => continue,
                Some(event) = events_rx.recv() => {
                    let output_name = match &event {
                        Event::Metric(_) => OUTPUT_METRICS,
                        Event::Log(_) => OUTPUT_LOGS,
                        Event::EventD(_) => OUTPUT_EVENTS,
                        Event::ServiceCheck(_) => OUTPUT_SERVICE_CHECKS,
                        _ => continue,
                    };
                    let buffered = context.dispatcher().buffered_named(output_name)
                        .error_context("Failed to get buffered dispatcher")?;
                    if let Err(e) = buffered.send_all([event]).await {
                        warn!("Failed to dispatch {output_name} event: {:?}", e);
                    }
                },
            }
        }

        debug!("Checks IPC source stopped.");
        Ok(())
    }
}

/// Server-side implementation of the AcrIpc gRPC service. ACR-shaped
/// clients (the agent-check-runner crate's IPC destination, the in-tree
/// fake test server, etc.) connect, hand off check data, and receive
/// per-batch acks. This service pushes received events through
/// `events_tx` to the source's run loop, which fans them out to the
/// topology's named outputs.
struct AcrIpcService {
    events_tx: mpsc::Sender<Event>,
}

#[async_trait]
impl AcrIpc for AcrIpcService {
    async fn handshake(&self, request: tonic::Request<Hello>) -> Result<Response<HelloResp>, Status> {
        let hello = request.into_inner();
        info!(
            client_id = %hello.client_id,
            client_version = %hello.client_version,
            client_protocol_version = hello.protocol_version,
            "ACR client connected.",
        );
        Ok(Response::new(HelloResp {
            protocol_version: PROTOCOL_VERSION,
            server_id: SERVER_ID.to_string(),
            accepted: true,
            reject_reason: String::new(),
        }))
    }

    async fn send_check_data(
        &self, request: tonic::Request<CheckDataMsg>,
    ) -> Result<Response<CheckDataAck>, Status> {
        let msg = request.into_inner();
        let sequence_id = msg.sequence_id;

        for check_data in msg.data.into_iter().filter_map(|d| d.data) {
            let event = match data_to_event(check_data) {
                Some(e) => e,
                None => continue,
            };
            if let Err(e) = self.events_tx.send(event).await {
                warn!("Failed to forward event to source pipeline: {:?}", e);
                return Ok(Response::new(CheckDataAck {
                    sequence_id,
                    success: false,
                    error: format!("forward failed: {}", e),
                }));
            }
        }

        Ok(Response::new(CheckDataAck {
            sequence_id,
            success: true,
            error: String::new(),
        }))
    }

    async fn send_check_result(
        &self, request: tonic::Request<CheckResultMsg>,
    ) -> Result<Response<SendCheckResultResponse>, Status> {
        let msg = request.into_inner();
        if msg.error.is_empty() {
            debug!(
                check_name = %msg.check_name,
                check_id = %msg.check_id,
                "Check completed successfully."
            );
        } else {
            warn!(
                check_name = %msg.check_name,
                check_id = %msg.check_id,
                error = %msg.error,
                "Check completed with error."
            );
        }
        Ok(Response::new(SendCheckResultResponse {}))
    }

    type StreamConfigStream = Pin<Box<dyn Stream<Item = Result<ConfigData, Status>> + Send + 'static>>;

    async fn stream_config(
        &self, _request: tonic::Request<StreamConfigRequest>,
    ) -> Result<Response<Self::StreamConfigStream>, Status> {
        // ADP is purely a check-data sink; it has no autodiscovery
        // configs to push back to the client. Hold the stream open
        // (forever pending) so the client doesn't tight-loop reconnect
        // — its `Ok(None)` arm treats stream completion as a signal to
        // reconnect. The stream stays alive until the underlying
        // connection drops, at which point the gRPC layer terminates
        // the call.
        Ok(Response::new(Box::pin(stream::pending())))
    }
}

/// Translates a single received `CheckData` payload into a saluki `Event`,
/// returning `None` for variants that the source intentionally drops
/// (unsupported payload shapes, malformed enums, etc).
fn data_to_event(data: Data) -> Option<Event> {
    match data {
        Data::Metric(metric) => metric_to_event(metric),
        Data::Log(log) => Some(log_to_event(log)),
        Data::Event(event) => Some(eventd_to_event(event)),
        Data::ServiceCheck(sc) => service_check_to_event(sc),
        Data::Sketch(_) => {
            // DDSketch reception requires a public `DDSketch::from_bins`
            // (or equivalent) constructor in saluki's ddsketch crate;
            // today `insert_raw_bin` is `pub(crate)` only. Drop the
            // payload for now — the wire format and ACR-side encoder
            // are in place so the unblock is purely a follow-up on the
            // ddsketch crate's surface area.
            debug!("AcrIpc Sketch payload received; sketch ingestion not yet implemented.");
            None
        }
        Data::EventPlatformEvent(_) => {
            // Event-platform events are an Agent-internal event-pipeline
            // concept (DBM, NPM, etc.) with no native saluki Event
            // equivalent. ADP intentionally doesn't surface them.
            debug!("AcrIpc EventPlatformEvent payload received; not handled by checks_ipc source.");
            None
        }
        Data::HistogramBucket(_) => {
            // Pre-aggregated buckets target the agent's
            // `Sender.HistogramBucket` / `Sender.OpenmetricsBucket`
            // upcalls; saluki's metric pipeline has no equivalent
            // ingestion path today.
            debug!("AcrIpc HistogramBucket payload received; not handled by checks_ipc source.");
            None
        }
    }
}

fn metric_to_event(metric: datadog_protos::checks::metric::Metric) -> Option<Event> {
    let metric_type = MetricType::try_from(metric.r#type).ok()?;
    let context = Context::from_parts(metric.name, proto_tags_to_tagset(metric.tags));

    let event = match metric_type {
        MetricType::Counter => Metric::counter(context, (metric.timestamp, metric.value)),
        MetricType::Gauge => Metric::gauge(context, (metric.timestamp, metric.value)),
        MetricType::Rate => {
            if metric.interval_secs == 0 {
                warn!("Received rate metric from check with interval of zero. Skipping.");
                return None;
            }
            Metric::rate(
                context,
                (metric.timestamp, metric.value),
                Duration::from_secs(metric.interval_secs),
            )
        }
        MetricType::Histogram => Metric::histogram(context, (metric.timestamp, metric.value)),
        MetricType::MonotonicCount => {
            // Monotonic counts ship absolute readings (e.g. /proc/stat
            // ticks); the receiver is responsible for diffing
            // successive samples to produce the delta. Saluki's
            // aggregator has no monotonic-aware sink — forwarding as a
            // Counter would sum absolute values into nonsense. Drop
            // until ADP grows a monotonic ingest path.
            debug!("AcrIpc MonotonicCount metric received; not yet supported by checks_ipc source.");
            return None;
        }
        MetricType::Historate => {
            // Historate is a rate over histogram buckets; degrading to
            // Histogram loses the rate semantics. No native saluki
            // equivalent today.
            debug!("AcrIpc Historate metric received; not yet supported by checks_ipc source.");
            return None;
        }
        MetricType::Unspecified => {
            warn!("Received metric with unspecified type. Skipping.");
            return None;
        }
    };

    Some(Event::Metric(event))
}

fn log_to_event(log: datadog_protos::checks::log::Log) -> Event {
    let status = match LogLevel::try_from(log.level) {
        Ok(level) => Some(log_level_to_log_status(level)),
        Err(_) => None,
    };

    // ACR encodes additional_properties values as JSON-encoded strings
    // (since proto's map<string,string> can't carry typed payloads
    // directly); decode each back to a JsonValue for saluki's typed
    // map. Entries that fail to parse are dropped — losing one
    // attribute is preferable to losing the whole log.
    let additional_properties: HashMap<MetaString, JsonValue> = log
        .additional_properties
        .into_iter()
        .filter_map(|(k, v)| {
            serde_json::from_str::<JsonValue>(&v)
                .ok()
                .map(|val| (MetaString::from(k), val))
        })
        .collect();

    let mut out = Log::new(log.message)
        .with_status(status)
        .with_source(string_to_meta_opt(log.source))
        .with_hostname(string_to_meta_opt(log.hostname))
        .with_service(string_to_meta_opt(log.service))
        .with_tags(Some(proto_tags_to_tagset(log.tags)));
    if !additional_properties.is_empty() {
        out = out.with_additional_properties(Some(additional_properties));
    }
    Event::Log(out)
}

fn eventd_to_event(event: datadog_protos::checks::event::Event) -> Event {
    Event::EventD(
        EventD::new(event.title, event.text)
            .with_timestamp(event.timestamp)
            .with_tags(proto_tags_to_tagset(event.tags)),
    )
}

fn service_check_to_event(sc: datadog_protos::checks::service_check::ServiceCheck) -> Option<Event> {
    let status = match SvcStatus::try_from(sc.status) {
        Ok(SvcStatus::Ok) => CheckStatus::Ok,
        Ok(SvcStatus::Warning) => CheckStatus::Warning,
        Ok(SvcStatus::Critical) => CheckStatus::Critical,
        Ok(SvcStatus::Unknown) => CheckStatus::Unknown,
        Ok(SvcStatus::Unspecified) | Err(_) => {
            warn!(
                "Received service check with unspecified/invalid status: {}. Skipping.",
                sc.status
            );
            return None;
        }
    };
    Some(Event::ServiceCheck(
        ServiceCheck::new(sc.name, status)
            .with_message(MetaString::from(sc.message))
            .with_hostname(string_to_meta_opt(sc.hostname))
            .with_tags(proto_tags_to_tagset(sc.tags)),
    ))
}

/// Empty proto strings represent "field not set" in proto3; map them to
/// `None` so saluki's optional-MetaString builders stay accurate rather
/// than treating empty as a real value.
fn string_to_meta_opt(s: String) -> Option<MetaString> {
    if s.is_empty() {
        None
    } else {
        Some(MetaString::from(s))
    }
}

/// Converts the proto's `repeated string` tag list into a saluki
/// `TagSet`, consuming each owned string into a `Tag` without an
/// intermediate clone.
fn proto_tags_to_tagset(tags: Vec<String>) -> TagSet {
    tags.into_iter().map(Tag::from).collect()
}

fn log_level_to_log_status(log_level: LogLevel) -> LogStatus {
    match log_level {
        LogLevel::Trace => LogStatus::Trace,
        LogLevel::Debug => LogStatus::Debug,
        LogLevel::Info => LogStatus::Info,
        LogLevel::Warning => LogStatus::Warning,
        LogLevel::Error => LogStatus::Error,
        LogLevel::Critical => LogStatus::Emergency,
        _ => LogStatus::Info,
    }
}
