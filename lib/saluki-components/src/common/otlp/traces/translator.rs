use std::collections::hash_map::IntoIter;
use std::num::NonZeroUsize;
use std::sync::Arc;

use otlp_protos::opentelemetry::proto::common::v1::{self as otlp_common};
use otlp_protos::opentelemetry::proto::resource::v1::Resource as OtlpResource;
use otlp_protos::opentelemetry::proto::trace::v1::ResourceSpans;
use saluki_common::collections::FastHashMap;
use saluki_context::tags::{SharedTagSet, TagSet};
use saluki_core::data_model::event::trace::{Span as DdSpan, Trace, TraceSampling};
use saluki_core::data_model::event::Event;
use stringtheory::interning::GenericMapInterner;
use stringtheory::interning::Interner as _;
use stringtheory::MetaString;

use crate::common::datadog::SAMPLING_PRIORITY_METRIC_KEY;
use crate::common::otlp::config::TracesConfig;
use crate::common::otlp::traces::transform::{bytes_to_hex_lowercase, otel_span_to_dd_span, otlp_value_to_string};
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

pub fn resource_attributes_to_tagset(attributes: &[otlp_common::KeyValue], interner: &GenericMapInterner) -> TagSet {
    let mut tags = TagSet::with_capacity(attributes.len());
    for kv in attributes {
        if let Some(key_value) = &kv.value {
            if let Some(value) = &key_value.value {
                if let Some(string_value) = otlp_value_to_string(value) {
                    let tag_str = format!("{}:{}", kv.key, string_value);
                    let tag = interner
                        .try_intern(&tag_str)
                        .map(MetaString::from)
                        .unwrap_or_else(|| MetaString::from(tag_str));
                    tags.insert_tag(tag);
                }
            }
        }
    }
    tags
}

struct TraceEntry {
    spans: Vec<DdSpan>,
    priority: Option<i32>,
    trace_id_hex: Option<MetaString>,
}
pub struct OtlpTracesTranslator {
    config: TracesConfig,
    interner: GenericMapInterner,
}

impl OtlpTracesTranslator {
    pub fn new(config: TracesConfig, interner_size: NonZeroUsize) -> Self {
        let interner = GenericMapInterner::new(interner_size);
        Self { config, interner }
    }

    pub fn translate_spans(&self, resource_spans: ResourceSpans, metrics: &Metrics) -> impl Iterator<Item = Event> {
        let resource: OtlpResource = resource_spans.resource.unwrap_or_default();
        let resource_tags = resource_attributes_to_tagset(&resource.attributes, &self.interner).into_shared();
        let mut traces_by_id: FastHashMap<u64, TraceEntry> = FastHashMap::default();
        let ignore_missing_fields = self.config.ignore_missing_datadog_fields;

        for scope_spans in resource_spans.scope_spans {
            let scope = scope_spans.scope;
            let scope_ref = scope.as_ref();
            metrics.spans_received().increment(scope_spans.spans.len() as u64);
            for span in scope_spans.spans {
                let trace_id = convert_trace_id(&span.trace_id);
                let entry = traces_by_id.entry(trace_id).or_insert_with(|| TraceEntry {
                    spans: Vec::new(),
                    priority: None,
                    trace_id_hex: None,
                });

                if entry.trace_id_hex.is_none() {
                    entry.trace_id_hex = trace_id_hex_meta(&span.trace_id);
                }

                let trace_id_hex = entry.trace_id_hex.clone();
                let dd_span = otel_span_to_dd_span(
                    &span,
                    &resource,
                    scope_ref,
                    ignore_missing_fields,
                    self.config.enable_otlp_compute_top_level_by_span_kind,
                    &self.interner,
                    trace_id_hex,
                );

                // Track last-seen priority for this trace (overwrites previous values)
                if let Some(&priority) = dd_span.metrics().get(SAMPLING_PRIORITY_METRIC_KEY) {
                    entry.priority = Some(priority as i32);
                }

                entry.spans.push(dd_span);
            }
        }

        OtlpTraceEventsIter {
            resource_tags,
            entries: traces_by_id.into_iter(),
        }
    }
}

struct OtlpTraceEventsIter {
    resource_tags: SharedTagSet,
    entries: IntoIter<u64, TraceEntry>,
}

impl Iterator for OtlpTraceEventsIter {
    type Item = Event;

    fn next(&mut self) -> Option<Self::Item> {
        for (_, entry) in self.entries.by_ref() {
            if entry.spans.is_empty() {
                continue;
            }

            let mut trace = Trace::new(entry.spans, self.resource_tags.clone());

            // Set the trace-level sampling priority if one was found
            if let Some(priority) = entry.priority {
                trace.set_sampling(Some(TraceSampling::new(false, Some(priority), None, None)));
            }

            return Some(Event::Trace(trace));
        }

        None
    }
}

fn trace_id_hex_meta(trace_id: &[u8]) -> Option<MetaString> {
    if trace_id.is_empty() {
        return None;
    }

    let hex = bytes_to_hex_lowercase(trace_id);
    if hex.is_empty() {
        return None;
    }

    Some(MetaString::from(Arc::<str>::from(hex)))
}
