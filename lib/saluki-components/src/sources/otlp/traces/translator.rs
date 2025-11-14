

use crate::sources::otlp::traces::transform::otel_span_to_dd_span;
use otlp_protos::opentelemetry::proto::trace::v1::ResourceSpans;
use saluki_common::collections::FastHashMap;
use saluki_core::data_model::event::trace::{Trace};


fn convert_trace_id(trace_id: &[u8]) -> u64 {
    if trace_id.len() < 8 {
        return 0;
    }

    u64::from_be_bytes((&trace_id[(trace_id.len() - 8)..]).try_into().unwrap())
}

fn convert_span_id(span_id: &[u8]) -> u64 {
    if span_id.len() != 8 {
        return 0;
    }

    u64::from_be_bytes(span_id.try_into().unwrap())
}

pub struct OtlpTracesTranslator {

}

impl OtlpTracesTranslator {
    pub fn new() -> Self {
        Self {}
    }

    pub fn translate_resource_spans(resource_spans: ResourceSpans) {
        let resource = &resource_spans.resource.unwrap_or_default();
        let resource_attributes = resource.attributes;

        let mut traces_by_id = FastHashMap::<u64, Trace>::default();
        for scope_span in resource_spans.scope_spans {
            let scope = &scope_span.scope;
            let spans = scope_span.spans;
            for otel_span in spans {
                let span_id = &otel_span.span_id;
                let trace_id = &otel_span.trace_id;
                let trace_id_uint64 = convert_trace_id(trace_id);
                let trace = traces_by_id.entry(trace_id_uint64)
    .or_insert_with(|| Trace::new(vec![]));
                let dd_span = otel_span_to_dd_span(otel_span, resource, scope);
                trace.spans_mut().push(dd_span);

            }
        }
        return;
    }
}