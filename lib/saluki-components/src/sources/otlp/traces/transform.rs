use saluki_core::data_model::event::trace::Span as dd_span;
use otlp_protos::opentelemetry::proto::{common::v1::InstrumentationScope, resource::v1::Resource, trace::v1::Span as otel_span};


pub fn otel_span_to_dd_span(otel_span: otel_span, otel_resource: Resource, instrumentation_scope: Option<InstrumentationScope>) -> dd_span {

}