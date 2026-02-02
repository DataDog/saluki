use std::num::NonZeroUsize;

use otlp_protos::opentelemetry::proto::common::v1::{self as otlp_common};
use otlp_protos::opentelemetry::proto::resource::v1::Resource as OtlpResource;
use otlp_protos::opentelemetry::proto::trace::v1::ResourceSpans;
use saluki_common::collections::FastHashMap;
use saluki_common::strings::StringBuilder;
use saluki_context::tags::TagSet;
use saluki_core::data_model::event::trace::{Span as DdSpan, Trace, TraceSampling};
use saluki_core::data_model::event::Event;
use stringtheory::interning::GenericMapInterner;

use crate::common::datadog::SAMPLING_PRIORITY_METRIC_KEY;
use crate::common::otlp::config::TracesConfig;
use crate::common::otlp::traces::transform::otel_span_to_dd_span;
use crate::common::otlp::traces::transform::otlp_value_to_string;
use crate::common::otlp::Metrics;

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

fn resource_attributes_to_tagset(
    attributes: &[otlp_common::KeyValue], string_builder: &mut StringBuilder<GenericMapInterner>,
) -> TagSet {
    let mut tags = TagSet::with_capacity(attributes.len());
    for kv in attributes {
        if let Some(key_value) = &kv.value {
            if let Some(value) = &key_value.value {
                if let Some(string_value) = otlp_value_to_string(value) {
                    string_builder.clear();
                    let _ = string_builder.push_str(kv.key.as_str());
                    let _ = string_builder.push(':');
                    let _ = string_builder.push_str(string_value.as_str());
                    tags.insert_tag(string_builder.to_meta_string());
                }
            }
        }
    }
    tags
}

pub struct OtlpTracesTranslator {
    config: TracesConfig,
    interner: GenericMapInterner,
    string_builder: StringBuilder<GenericMapInterner>,
}

impl OtlpTracesTranslator {
    pub fn new(config: TracesConfig, interner_size: NonZeroUsize) -> Self {
        let interner = GenericMapInterner::new(interner_size);
        let string_builder = StringBuilder::new().with_interner(interner.clone());
        Self {
            config,
            interner,
            string_builder,
        }
    }

    pub fn translate_resource_spans(&mut self, resource_spans: ResourceSpans, metrics: &Metrics) -> Vec<Event> {
        let resource: OtlpResource = resource_spans.resource.unwrap_or_default();
        let ignore_missing_fields = self.config.ignore_missing_datadog_fields;
        let compute_top_level = self.config.enable_otlp_compute_top_level_by_span_kind;
        let interner = &self.interner;
        let string_builder = &mut self.string_builder;
        let resource_tags: TagSet = resource_attributes_to_tagset(&resource.attributes, string_builder);
        let mut traces_by_id: FastHashMap<u64, Vec<DdSpan>> = FastHashMap::default();
        let mut priorities_by_id: FastHashMap<u64, i32> = FastHashMap::default();

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
                    compute_top_level,
                    interner,
                    string_builder,
                );

                // Track last-seen priority for this trace (overwrites previous values)
                if let Some(&priority) = dd_span.metrics().get(SAMPLING_PRIORITY_METRIC_KEY) {
                    priorities_by_id.insert(trace_id, priority as i32);
                }

                traces_by_id.entry(trace_id).or_default().push(dd_span);
            }
        }

        traces_by_id
            .into_iter()
            .filter_map(|(trace_id, spans)| {
                if spans.is_empty() {
                    None
                } else {
                    let mut trace = Trace::new(spans, resource_tags.clone());

                    // Set the trace-level sampling priority if one was found
                    if let Some(&priority) = priorities_by_id.get(&trace_id) {
                        trace.set_sampling(Some(TraceSampling::new(false, Some(priority), None, None)));
                    }

                    Some(Event::Trace(trace))
                }
            })
            .collect()
    }
}
