use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use datadog_protos::checks::{
    check_data::Data,
    checks_server::{Checks, ChecksServer},
    log::LogLevel,
    metric::MetricType,
    SendCheckPayloadRequest, SendCheckPayloadResponse,
};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use protobuf::Enum as _;
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
use stringtheory::MetaString;
use tokio::select;
use tokio::sync::mpsc;
use tonic::transport::Server;
use tonic::{Response, Status};
use tracing::{debug, info, warn};

const fn default_grpc_endpoint() -> ListenAddress {
    ListenAddress::any_tcp(5105)
}

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
                OutputDefinition::named_output("metrics", EventType::Metric),
                OutputDefinition::named_output("logs", EventType::Log),
                OutputDefinition::named_output("events", EventType::EventD),
                OutputDefinition::named_output("service_checks", EventType::ServiceCheck),
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

        let grpc_server = Server::builder().add_service(ChecksServer::new(ChecksService { events_tx }));

        let grpc_socket_addr = match self.grpc_endpoint {
            ListenAddress::Tcp(addr) => addr,
            _ => return Err(generic_error!("OTLP gRPC endpoint must be a TCP address.")),
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
                        Event::Metric(_) => "metrics",
                        Event::Log(_) => "logs",
                        Event::EventD(_) => "events",
                        Event::ServiceCheck(_) => "service_checks",
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

struct ChecksService {
    events_tx: mpsc::Sender<Event>,
}

#[async_trait]
impl Checks for ChecksService {
    async fn send_check_payload(
        &self, request: tonic::Request<SendCheckPayloadRequest>,
    ) -> Result<Response<SendCheckPayloadResponse>, Status> {
        // command for testing locally:
        //
        // DD_DATA_PLANE_CHECKS_ENABLED=true make run-adp-standalone
        // grpcurl -d '{"payload": {"data": [{"metric": {"type": 1, "name": "my_counter", "tags": ["tag1:value1"], "points": [{"timestamp": 1234, "value": 1.0}]}}]}}' -plaintext -proto lib/protos/datadog/proto/checks/checks.proto localhost:5105 datadog.checks.Checks/SendCheckPayload

        info!("Received check payload.");

        let payload = request.into_inner();
        for check_data in payload.data.into_iter().filter_map(|data| data.data) {
            let event = match check_data {
                Data::Metric(metric) => {
                    let metric_type = match MetricType::from_i32(metric.r#type) {
                        Some(typ) => typ,
                        None => continue,
                    };

                    let tags = metric.tags.into_iter().map(Tag::from).collect::<TagSet>();
                    let context = Context::from_parts(metric.name, tags.into_shared());
                    let points = metric
                        .points
                        .into_iter()
                        .map(|point| (point.timestamp as u64, point.value))
                        .collect::<Vec<_>>();
                    let metric = match metric_type {
                        MetricType::COUNT => Metric::counter(context, &points[..]),
                        MetricType::GAUGE => Metric::gauge(context, &points[..]),
                        MetricType::RATE => {
                            let interval_secs = metric.interval_secs;
                            if interval_secs == 0 {
                                warn!("Received rate metric from check with interval of zero. Skipping.");
                                continue;
                            }

                            Metric::rate(context, &points[..], Duration::from_secs(interval_secs))
                        }
                        MetricType::UNSPECIFIED => {
                            warn!("Received metric with unspecified type. Skipping.");
                            continue;
                        }
                    };

                    Event::Metric(metric)
                }
                Data::Log(log) => {
                    let status = proto_status_to_log_status(log.status);
                    Event::Log(Log::new(log.message).with_status(status))
                }
                Data::Event(event) => {
                    let tags = event.tags.into_iter().map(Tag::from).collect::<TagSet>();
                    Event::EventD(
                        EventD::new(event.title, event.text)
                            .with_timestamp(event.timestamp as u64)
                            .with_tags(tags.into_shared()),
                    )
                }
                Data::ServiceCheck(sc) => {
                    let status = match CheckStatus::try_from(sc.status as u8) {
                        Ok(status) => status,
                        Err(_) => {
                            warn!("Received service check with invalid status: {}. Skipping.", sc.status);
                            continue;
                        }
                    };
                    let tags = sc.tags.into_iter().map(Tag::from).collect::<TagSet>();
                    Event::ServiceCheck(
                        ServiceCheck::new(sc.name, status)
                            .with_timestamp(sc.timestamp as u64)
                            .with_message(MetaString::from(sc.message))
                            .with_tags(tags.into_shared()),
                    )
                }
            };

            if let Err(e) = self.events_tx.send(event).await {
                warn!("Failed to send metric event: {:?}", e);
            }
        }

        Ok(Response::new(SendCheckPayloadResponse {}))
    }
}

fn log_level_to_log_status(log_level: LogLevel) -> LogStatus {
    match log_level {
    LOG_LEVEL_UNSPECIFIED = 0;
    LOG_LEVEL_TRACE = 7;
    LOG_LEVEL_DEBUG = 10;
    LOG_LEVEL_INFO = 20;
    LOG_LEVEL_WARNING = 30;
    LOG_LEVEL_ERROR = 40;
    LOG_LEVEL_CRITICAL = 50;

        LogLevel::LOG_LEVEL_TRACE => LogStatus::Trace,
        LogLevel::LOG_LEVEL_DEBUG => LogStatus::Debug,
        LogLevel::LOG_LEVEL_INFO => LogStatus::Info,
        LogLevel::LOG_LEVEL_WARNING => LogStatus::Warning,
        LogLevel::LOG_LEVEL_ERROR => LogStatus::Error,
        LogLevel::LOG_LEVEL_CRITICAL => LogStatus::Emergency,
        _ => LogStatus::Info,
    }
}
