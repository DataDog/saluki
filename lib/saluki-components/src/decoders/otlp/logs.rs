use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use otlp_protos::opentelemetry::proto::collector::logs::v1::ExportLogsServiceRequest;
use prost::Message;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{
        decoders::{Decoder, DecoderBuilder, DecoderContext},
        BuildContext,
    },
    data_model::{
        event::{Event, EventType},
        payload::PayloadType,
    },
    topology::interconnect::EventBufferManager,
};
use saluki_env::workload::WorkloadProvider;
use saluki_error::GenericError;
use tokio::{
    select,
    time::{interval, MissedTickBehavior},
};
use tracing::{debug, error, warn};

use crate::common::otlp::origin::OtlpOriginTagResolver;
use crate::common::otlp::{build_metrics, Metrics, OTLP_LOGS_GRPC_SERVICE_PATH};
use crate::sources::otlp::logs::render::{CONTENT_BUFFER_RETENTION_LIMIT_BYTES, CONTENT_FIELDS_RETENTION_LIMIT_BYTES};
use crate::sources::otlp::logs::translator::OtlpLogsTranslator;

/// Configuration for the OTLP logs decoder.
///
/// The decoder translates OTLP logs export requests, as relayed by the OTLP relay, into logs. It translates them exactly
/// as the OTLP source does, so relayed logs can take the same path as logs the source receives.
pub struct OtlpLogsDecoderConfiguration {
    /// Resolves entity and global tags for a resource's container.
    origin_tag_resolver: OtlpOriginTagResolver,
}

impl OtlpLogsDecoderConfiguration {
    /// Creates a new `OtlpLogsDecoderConfiguration`.
    ///
    /// The workload provider supplies the entity and global tags that enrich translated logs.
    pub fn new<W>(workload_provider: W) -> Self
    where
        W: WorkloadProvider + Send + Sync + 'static,
    {
        Self {
            origin_tag_resolver: OtlpOriginTagResolver::new(Arc::new(workload_provider)),
        }
    }
}

#[async_trait]
impl DecoderBuilder for OtlpLogsDecoderConfiguration {
    fn input_payload_type(&self) -> PayloadType {
        PayloadType::Grpc
    }

    fn output_event_type(&self) -> EventType {
        EventType::Log
    }

    async fn build(&self, context: BuildContext) -> Result<Box<dyn Decoder + Send>, GenericError> {
        Ok(Box::new(OtlpLogsDecoder {
            logs_translator: OtlpLogsTranslator::new(self.origin_tag_resolver.clone()),
            metrics: build_metrics(context.component_context()),
        }))
    }
}

impl MemoryBounds for OtlpLogsDecoderConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<OtlpLogsDecoder>("decoder struct")
            .with_fixed_amount("logs render buffer", CONTENT_BUFFER_RETENTION_LIMIT_BYTES)
            .with_fixed_amount("logs render field list", CONTENT_FIELDS_RETENTION_LIMIT_BYTES);
    }
}

/// OTLP logs decoder.
pub struct OtlpLogsDecoder {
    logs_translator: OtlpLogsTranslator,
    metrics: Metrics,
}

#[async_trait]
impl Decoder for OtlpLogsDecoder {
    async fn run(self: Box<Self>, mut context: DecoderContext) -> Result<(), GenericError> {
        let Self {
            mut logs_translator,
            metrics,
        } = *self;
        let mut health = context.take_health_handle();
        health.mark_ready();

        debug!("OTLP logs decoder started.");

        // Flush buffered events periodically, so that a partial buffer doesn't wait for the next payload.
        let mut buffer_flush = interval(Duration::from_millis(100));
        buffer_flush.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut event_buffer_manager = EventBufferManager::default();

        loop {
            select! {
                maybe_payload = context.payloads().next() => {
                    let Some(payload) = maybe_payload else {
                        debug!("Payloads stream closed, shutting down decoder.");
                        break;
                    };

                    let Some(grpc_payload) = payload.try_into_grpc_payload() else {
                        warn!("Received non-gRPC payload in OTLP logs decoder. Dropping payload.");
                        continue;
                    };

                    if grpc_payload.service_path() != &*OTLP_LOGS_GRPC_SERVICE_PATH {
                        warn!(
                            service_path = grpc_payload.service_path(),
                            "Received gRPC payload for another service in OTLP logs decoder. Dropping payload."
                        );
                        continue;
                    }

                    let (_, _, _, body) = grpc_payload.into_parts();
                    let request = match ExportLogsServiceRequest::decode(body.into_bytes()) {
                        Ok(request) => request,
                        Err(e) => {
                            error!(error = %e, "Failed to decode OTLP logs request.");
                            continue;
                        }
                    };

                    for resource_logs in request.resource_logs {
                        for log in logs_translator.translate_logs(resource_logs, &metrics) {
                            if let Some(event_buffer) = event_buffer_manager.try_push(Event::Log(log)) {
                                if let Err(e) = context.dispatcher().dispatch(event_buffer).await {
                                    error!(error = %e, "Failed to dispatch log events.");
                                }
                            }
                        }
                    }
                },
                _ = buffer_flush.tick() => {
                    if let Some(event_buffer) = event_buffer_manager.consume() {
                        if let Err(e) = context.dispatcher().dispatch(event_buffer).await {
                            error!(error = %e, "Failed to dispatch buffered log events.");
                        }
                    }
                },
                _ = health.live() => continue,
            }
        }

        if let Some(event_buffer) = event_buffer_manager.consume() {
            if let Err(e) = context.dispatcher().dispatch(event_buffer).await {
                error!(error = %e, "Failed to dispatch final log events.");
            }
        }

        debug!("OTLP logs decoder stopped.");

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use otlp_protos::opentelemetry::proto::common::v1::{any_value, AnyValue, KeyValue};
    use otlp_protos::opentelemetry::proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use otlp_protos::opentelemetry::proto::resource::v1::Resource;
    use saluki_common::buf::FrozenChunkedBytesBuffer;
    use saluki_core::components::test_util::TestComponentDriver;
    use saluki_core::data_model::event::{log::Log, Event};
    use saluki_core::data_model::payload::{GrpcPayload, Payload, PayloadMetadata};
    use saluki_env::workload::providers::NoopWorkloadProvider;
    use stringtheory::MetaString;

    use super::*;
    use crate::common::otlp::OTLP_TRACES_GRPC_SERVICE_PATH;

    fn string_attribute(key: &str, value: &str) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue(value.to_string())),
            }),
        }
    }

    fn logs_request() -> ExportLogsServiceRequest {
        let record = |body: &str| LogRecord {
            time_unix_nano: 1_762_862_400_000_000_000,
            severity_text: "WARN".to_string(),
            body: Some(AnyValue {
                value: Some(any_value::Value::StringValue(body.to_string())),
            }),
            attributes: vec![string_attribute("user.id", "42")],
            ..Default::default()
        };
        ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource {
                    attributes: vec![
                        string_attribute("service.name", "checkout"),
                        string_attribute("host.name", "web-1"),
                    ],
                    ..Default::default()
                }),
                scope_logs: vec![ScopeLogs {
                    log_records: vec![record("first"), record("second")],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
    }

    fn grpc_payload(service_path: MetaString, body: Vec<u8>) -> Payload {
        Payload::Grpc(GrpcPayload::new(
            PayloadMetadata::from_event_count(1),
            MetaString::empty(),
            service_path,
            FrozenChunkedBytesBuffer::from(Bytes::from(body)),
        ))
    }

    /// Clears the time at which the log was translated, which differs between two translations.
    fn without_ingestion_timestamp(log: Log) -> Log {
        log.with_ingestion_timestamp(0)
    }

    async fn decode(payloads: Vec<Payload>) -> Vec<Event> {
        let (control, mut output) =
            TestComponentDriver::decoder(OtlpLogsDecoderConfiguration::new(NoopWorkloadProvider))
                .await
                .expect("decoder should build");
        for payload in payloads {
            control.send(payload).await;
        }
        let (result, events) = tokio::join!(control.shutdown(), output.collect_events());
        result.expect("decoder should stop cleanly");
        events
    }

    #[tokio::test]
    async fn relayed_logs_are_translated_like_the_source_translates_them() {
        let request = logs_request();
        let metrics = Metrics::for_tests();
        let mut translator = OtlpLogsTranslator::new(OtlpOriginTagResolver::new(Arc::new(NoopWorkloadProvider)));
        let expected: Vec<Log> = request
            .resource_logs
            .clone()
            .into_iter()
            .flat_map(|resource_logs| translator.translate_logs(resource_logs, &metrics).collect::<Vec<_>>())
            .map(without_ingestion_timestamp)
            .collect();
        assert_eq!(expected.len(), 2);

        let events = decode(vec![grpc_payload(OTLP_LOGS_GRPC_SERVICE_PATH, request.encode_to_vec())]).await;
        let logs: Vec<Log> = events
            .into_iter()
            .map(|event| match event {
                Event::Log(log) => without_ingestion_timestamp(log),
                event => panic!("expected a log, got {event:?}"),
            })
            .collect();

        assert_eq!(logs, expected);
    }

    #[tokio::test]
    async fn payloads_that_are_not_logs_requests_are_dropped() {
        let request = logs_request();
        let events = decode(vec![
            // A logs request sent to another service's path is still for that service.
            grpc_payload(OTLP_TRACES_GRPC_SERVICE_PATH, request.encode_to_vec()),
            grpc_payload(OTLP_LOGS_GRPC_SERVICE_PATH, b"not a protobuf message".to_vec()),
            grpc_payload(OTLP_LOGS_GRPC_SERVICE_PATH, request.encode_to_vec()),
        ])
        .await;

        // Only the last payload is a logs request, and the decoder keeps going after the others.
        assert_eq!(events.len(), 2);
    }
}
