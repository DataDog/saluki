use otlp_protos::opentelemetry::proto::trace::v1::ResourceSpans;
use saluki_common::collections::FastHashMap;
use saluki_core::data_model::event::trace::Trace;
use stringtheory::MetaString;

use super::sampler::SamplingPriority;
use crate::sources::otlp::attributes::source::SourceKind;
use crate::sources::otlp::attributes::{resource_to_source, tags_from_attributes};
use crate::sources::otlp::traces::transform::otel_span_to_dd_span;

pub fn convert_trace_id(trace_id: &[u8]) -> u64 {
    if trace_id.len() < 8 {
        return 0;
    }

    u64::from_be_bytes((&trace_id[(trace_id.len() - 8)..]).try_into().unwrap())
}

pub fn convert_span_id(span_id: &[u8]) -> u64 {
    if span_id.len() != 8 {
        return 0;
    }

    u64::from_be_bytes(span_id.try_into().unwrap())
}

#[allow(unused)]
pub struct OtlpTracesTranslator {
    // ProbabilisticSampling specifies the percentage of traces to ingest. Exceptions are made for errors
    // and rare traces (outliers) if "RareSamplerEnabled" is true. Invalid values are equivalent to 100.
    // If spans have the "sampling.priority" attribute set, probabilistic sampling is skipped and the user's
    // decision is followed.
    probabilistic_sampling: f64,

    // IgnoreMissingDatadogFields specifies whether we should recompute DD span fields if the corresponding "datadog."
    // namespaced span attributes are missing. If it is false (default), we will use the incoming "datadog." namespaced
    // OTLP span attributes to construct the DD span, and if they are missing, we will recompute them from the other
    // OTLP semantic convention attributes. If it is true, we will only populate a field if its associated "datadog."
    // OTLP span attribute exists, otherwise we will leave it empty.
    ignore_missing_fields: bool,
}

impl OtlpTracesTranslator {
    #[allow(unused)]
    pub fn new(probabilistic_sampling: f64, ignore_missing_fields: bool) -> Self {
        Self {
            probabilistic_sampling,
            ignore_missing_fields,
        }
    }

    #[allow(unused)]
    pub fn translate_resource_spans(resource_spans: ResourceSpans) {
        let mut traces_map: FastHashMap<u64, Trace> = FastHashMap::default();
        let mut priority_map: FastHashMap<u64, SamplingPriority> = FastHashMap::default();
        let resource = resource_spans.resource.unwrap_or_default();
        let resource_attributes = &resource.attributes;
        let source = resource_to_source(&resource);
        for scope_span in resource_spans.scope_spans {
            let instrumentation_scope = scope_span.scope;
            for otel_span in scope_span.spans {
                let trace_id = convert_trace_id(&otel_span.trace_id);
                let trace = traces_map.entry(trace_id).or_insert_with(|| Trace::new(vec![]));
                let dd_span = otel_span_to_dd_span(otel_span, &resource, instrumentation_scope.clone());
                trace.spans_mut().push(dd_span);
            }
        }

        let host = match &source {
            Some(src) if matches!(src.kind, SourceKind::HostnameKind) => {
                Some(MetaString::from(src.identifier.as_str()))
            }
            _ => None,
        };
        let tags = tags_from_attributes(&resource_attributes);

        return;
    }
}
