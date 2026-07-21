use std::time::Duration;

use agent_data_plane_config::domains;
use async_trait::async_trait;
use otlp_protos::opentelemetry::proto::collector::trace::v1::ExportTraceServiceRequest;
use prost::Message;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{
        decoders::{Decoder, DecoderBuilder, DecoderContext},
        ComponentContext,
    },
    data_model::{event::EventType, payload::PayloadType},
    topology::interconnect::EventBufferManager,
};
use saluki_error::{generic_error, GenericError};
use tokio::{
    select,
    time::{interval, MissedTickBehavior},
};
use tracing::{debug, error, warn};

use crate::common::otlp::traces::translator::OtlpTracesTranslator;
use crate::common::otlp::{
    build_metrics, config::TracesConfig, Metrics, OTLP_LOGS_GRPC_SERVICE_PATH, OTLP_METRICS_GRPC_SERVICE_PATH,
    OTLP_TRACES_GRPC_SERVICE_PATH,
};

/// Configuration for the OTLP decoder.
#[derive(Default)]
pub struct OtlpDecoderConfiguration {
    /// Resolved OTLP trace ingestion settings.
    traces: domains::otlp::Traces,
}

impl OtlpDecoderConfiguration {
    /// Creates a new `OtlpDecoderConfiguration` from the resolved OTLP trace configuration.
    pub fn from_configuration(traces: &domains::otlp::Traces) -> Self {
        Self { traces: traces.clone() }
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
        let traces_interner_size = std::num::NonZeroUsize::new(self.traces.string_interner_size as usize)
            .ok_or_else(|| generic_error!("otlp_config.traces.string_interner_size must be greater than 0"))?;
        let traces_config = TracesConfig {
            enable_otlp_compute_top_level_by_span_kind: self.traces.enable_compute_top_level_by_span_kind,
            ignore_missing_datadog_fields: self.traces.ignore_missing_datadog_fields,
            ..Default::default()
        };
        let traces_translator = OtlpTracesTranslator::new(traces_config, traces_interner_size);

        Ok(Box::new(OtlpDecoder {
            traces_translator,
            metrics,
        }))
    }
}

impl MemoryBounds for OtlpDecoderConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<OtlpDecoder>("decoder struct");
    }
}

/// OTLP decoder.
pub struct OtlpDecoder {
    traces_translator: OtlpTracesTranslator,
    metrics: Metrics,
}

#[async_trait]
impl Decoder for OtlpDecoder {
    async fn run(self: Box<Self>, mut context: DecoderContext) -> Result<(), GenericError> {
        let Self {
            mut traces_translator,
            metrics,
        } = *self;
        let mut health = context.take_health_handle();
        health.mark_ready();

        debug!("OTLP decoder started.");

        // Set a buffer flush interval of 100ms to ensure we flush buffered events periodically.
        let mut buffer_flush = interval(Duration::from_millis(100));
        buffer_flush.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut event_buffer_manager = EventBufferManager::default();

        loop {
            select! {
                maybe_payload = context.payloads().next() => {
                    let payload = match maybe_payload {
                        Some(payload) => payload,
                        None => {
                            debug!("Payloads stream closed, shutting down decoder.");
                            break;
                        }
                    };

                    let grpc_payload = match payload.try_into_grpc_payload() {
                        Some(grpc) => grpc,
                        None => {
                            warn!("Received non-gRPC payload in OTLP decoder. Dropping payload.");
                            continue;
                        }
                    };

                    match grpc_payload.service_path() {
                        path if path == &*OTLP_TRACES_GRPC_SERVICE_PATH => {
                            let (_, _, _, body) = grpc_payload.into_parts();
                            let request = match ExportTraceServiceRequest::decode(body.into_bytes()) {
                                Ok(req) => req,
                                Err(e) => {
                                    error!(error = %e, "Failed to decode OTLP trace request.");
                                    continue;
                                }
                            };

                            for resource_spans in request.resource_spans {
                                for trace_event in traces_translator.translate_spans(resource_spans, &metrics) {
                                    if let Some(event_buffer) = event_buffer_manager.try_push(trace_event) {
                                        if let Err(e) = context.dispatcher().dispatch(event_buffer).await {
                                            error!(error = %e, "Failed to dispatch trace events.");
                                        }
                                    }
                                }
                            }
                        }
                        path if path == &*OTLP_METRICS_GRPC_SERVICE_PATH => {
                            warn!("OTLP metrics decoding not yet implemented. Dropping metrics payload.");
                        }
                        path if path == &*OTLP_LOGS_GRPC_SERVICE_PATH => {
                            warn!("OTLP logs decoding not yet implemented. Dropping logs payload.");
                        }
                        path => {
                            warn!(service_path = path, "Received gRPC payload with unknown service path. Dropping payload.");
                        }
                    }
                },
                _ = buffer_flush.tick() => {
                    if let Some(event_buffer) = event_buffer_manager.consume() {
                        if let Err(e) = context.dispatcher().dispatch(event_buffer).await {
                            error!(error = %e, "Failed to dispatch buffered trace events.");
                        }
                    }
                },
                _ = health.live() => continue,
            }
        }

        if let Some(event_buffer) = event_buffer_manager.consume() {
            if let Err(e) = context.dispatcher().dispatch(event_buffer).await {
                error!(error = %e, "Failed to dispatch final trace events.");
            }
        }

        debug!("OTLP decoder stopped.");

        Ok(())
    }
}
