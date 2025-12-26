use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use otlp_protos::opentelemetry::proto::collector::trace::v1::ExportTraceServiceRequest;
use prost::Message;
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{
        decoders::{Decoder, DecoderBuilder, DecoderContext},
        ComponentContext,
    },
    data_model::{event::EventType, payload::PayloadType},
    topology::interconnect::EventBufferManager,
};
use saluki_error::{ErrorContext as _, GenericError};
use serde::Deserialize;
use tracing::{debug, error};

use crate::common::otlp::traces::translator::OtlpTracesTranslator;
use crate::common::otlp::{
    build_metrics, config::TracesConfig, Metrics, OTLP_LOGS_GRPC_SERVICE_PATH, OTLP_METRICS_GRPC_SERVICE_PATH,
    OTLP_TRACES_GRPC_SERVICE_PATH,
};

/// Configuration for the OTLP decoder.
#[derive(Deserialize, Default)]
pub struct OtlpDecoderConfiguration {
    #[serde(default)]
    otlp_config: OtlpDecoderConfig,
}

/// OTLP configuration for the decoder.
#[derive(Deserialize, Default)]
struct OtlpDecoderConfig {
    #[serde(default)]
    traces: TracesConfig,
}

impl OtlpDecoderConfiguration {
    /// Creates a new `OtlpDecoderConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        config
            .as_typed()
            .error_context("Failed to load OTLP decoder configuration")
    }
}

#[async_trait]
impl DecoderBuilder for OtlpDecoderConfiguration {
    fn input_payload_type(&self) -> PayloadType {
        PayloadType::Grpc
    }

    fn output_event_type(&self) -> EventType {
        EventType::Trace
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Decoder + Send>, GenericError> {
        let metrics = build_metrics(&context);
        let translator = OtlpTracesTranslator::new(self.otlp_config.traces.clone());

        Ok(Box::new(OtlpTracesDecoder { translator, metrics }))
    }
}

impl MemoryBounds for OtlpDecoderConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<OtlpTracesDecoder>("decoder struct");
    }
}

/// OTLP traces decoder.
pub struct OtlpTracesDecoder {
    translator: OtlpTracesTranslator,
    metrics: Metrics,
}

#[async_trait]
impl Decoder for OtlpTracesDecoder {
    async fn run(self: Box<Self>, mut context: DecoderContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();

        debug!("OTLP traces decoder started.");

        let mut event_buffer_manager = EventBufferManager::default();

        loop {
            let payload = match context.payloads().next().await {
                Some(payload) => payload,
                None => {
                    debug!("Payloads stream closed, shutting down decoder.");
                    break;
                }
            };

            let grpc_payload = match payload.try_into_grpc_payload() {
                Some(grpc) => grpc,
                None => {
                    error!("Received non-gRPC payload in OTLP decoder.");
                    continue;
                }
            };

            match grpc_payload.service_path() {
                path if path == &*OTLP_TRACES_GRPC_SERVICE_PATH => {
                    let body = grpc_payload.body().clone();

                    let request = match ExportTraceServiceRequest::decode(body) {
                        Ok(req) => req,
                        Err(e) => {
                            error!(error = %e, "Failed to decode OTLP trace request.");
                            continue;
                        }
                    };

                    for resource_spans in request.resource_spans {
                        let trace_events = self.translator.translate_resource_spans(resource_spans, &self.metrics);
                        for trace_event in trace_events {
                            if let Some(event_buffer) = event_buffer_manager.try_push(trace_event) {
                                if let Err(e) = context.dispatcher().dispatch(event_buffer).await {
                                    error!(error = %e, "Failed to dispatch trace events.");
                                }
                            }
                        }
                    }
                }
                path if path == &*OTLP_METRICS_GRPC_SERVICE_PATH => {
                    error!("OTLP metrics decoding not yet implemented.");
                }
                path if path == &*OTLP_LOGS_GRPC_SERVICE_PATH => {
                    error!("OTLP logs decoding not yet implemented.");
                }
                path => {
                    error!(service_path = path, "Received gRPC payload with unknown service path.");
                }
            }
        }

        if let Some(event_buffer) = event_buffer_manager.consume() {
            if let Err(e) = context.dispatcher().dispatch(event_buffer).await {
                error!(error = %e, "Failed to dispatch final trace events.");
            }
        }

        debug!("OTLP traces decoder stopped.");

        Ok(())
    }
}
