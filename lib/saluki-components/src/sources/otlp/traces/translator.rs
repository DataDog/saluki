use otlp_protos::opentelemetry::proto::resource::v1::Resource as OtlpResource;
use otlp_protos::opentelemetry::proto::trace::v1::ResourceSpans;
use saluki_common::collections::FastHashMap;
use saluki_context::tags::TagSet;
use saluki_core::data_model::event::trace::{Span as DdSpan, Trace};
use saluki_core::data_model::event::Event;

use super::config::OtlpTracesTranslatorConfig;
use crate::sources::otlp::traces::transform::otel_span_to_dd_span;
use crate::sources::otlp::Metrics;
use otlp_protos::opentelemetry::proto::common::v1::{self as otlp_common};
use crate::sources::otlp::traces::transform::otlp_value_to_string;


pub fn convert_trace_id(trace_id: &[u8]) -> u64 {
    if trace_id.len() < 8 {
        return 0;
    }
    u64::from_be_bytes((&trace_id[(trace_id.len() - 8)..]).try_into().unwrap_or_default())
}

pub fn convert_span_id(span_id: &[u8]) -> u64 {
    if span_id.len() != 8 {
        return 0;
    }
    u64::from_be_bytes(span_id.try_into().unwrap_or_default())
}

pub fn resource_attributes_to_tagset(attributes: &[otlp_common::KeyValue]) -> TagSet {
    let mut tags = TagSet::with_capacity(attributes.len());
    for kv in attributes {
        if let Some(key_value) = &kv.value{
            if let Some(value) = &key_value.value {
                if let Some(string_value) = otlp_value_to_string(value) {
                    tags.insert_tag(format!("{}:{}", kv.key, string_value));
                }
            }
        }
    }
    tags
}

pub struct OtlpTracesTranslator {
    config: OtlpTracesTranslatorConfig,
}

impl OtlpTracesTranslator {
    pub fn new(config: OtlpTracesTranslatorConfig) -> Self {
        Self { config }
    }

    pub fn translate_resource_spans(&self, resource_spans: ResourceSpans, metrics: &Metrics) -> Vec<Event> {
        let resource: OtlpResource = resource_spans.resource.unwrap_or_default();
        let resource_tags: TagSet = resource_attributes_to_tagset(&resource.attributes);
        let mut traces_by_id: FastHashMap<u64, Vec<DdSpan>> = FastHashMap::default();
        let ignore_missing_fields = self.config.ignore_missing_datadog_fields;

        for scope_spans in resource_spans.scope_spans {
            let scope = scope_spans.scope;
            let scope_ref = scope.as_ref();
            metrics.spans_received().increment(scope_spans.spans.len() as u64);
            for span in scope_spans.spans {
                let trace_id = convert_trace_id(&span.trace_id);
                let dd_span = otel_span_to_dd_span(
                    &span,
                    &resource,
                    scope_ref,
                    ignore_missing_fields,
                    self.config.compute_top_level_by_span_kind,
                );
                traces_by_id.entry(trace_id).or_default().push(dd_span);
            }
        }

        traces_by_id
            .into_iter()
            .filter_map(|(_, spans)| {
                if spans.is_empty() {
                    None
                } else {
                    Some(Event::Trace(Trace::new(spans, resource_tags.clone())))
                }
            })
            .collect()
    }
}
