//! Size estimates for traces held by the container-tag buffer.
//!
//! Shared strings and attributes count for each trace. This overestimates shared allocations rather than excluding their contents.

use std::mem::size_of;

use saluki_common::collections::FastHashMap;
use saluki_core::data_model::event::trace::{AttributeValue, Span, SpanEvent, SpanLink, Trace};
use stringtheory::MetaString;

pub(super) fn estimated_size(trace: &Trace) -> usize {
    let payload = &trace.payload;
    let strings = [
        trace.origin.as_ref(),
        trace.decision_maker.as_deref().unwrap_or_default(),
        payload.container_id.as_ref(),
        payload.language_name.as_ref(),
        payload.language_version.as_ref(),
        payload.tracer_version.as_ref(),
        payload.runtime_id.as_ref(),
        payload.env.as_ref(),
        payload.hostname.as_ref(),
        payload.app_version.as_ref(),
    ];
    let mut size = strings
        .iter()
        .fold(size_of::<Trace>(), |size, value| size.saturating_add(value.len()));
    size = size.saturating_add(attributes_size(&trace.attributes));
    for tag in payload.resolved_container_tags() {
        size = size.saturating_add(size_of::<MetaString>()).saturating_add(tag.len());
    }
    for span in trace.spans() {
        size = size.saturating_add(size_of::<Span>());
        for value in [
            span.service(),
            span.name(),
            span.resource(),
            span.span_type(),
            &span.env,
            &span.version,
            &span.component,
        ] {
            size = size.saturating_add(value.len());
        }
        size = size.saturating_add(attributes_size(&span.attributes));
        for link in span.span_links() {
            size = size
                .saturating_add(size_of::<SpanLink>())
                .saturating_add(link.tracestate().len())
                .saturating_add(attributes_size(link.attributes()));
        }
        for event in span.span_events() {
            size = size
                .saturating_add(size_of::<SpanEvent>())
                .saturating_add(event.name().len())
                .saturating_add(attributes_size(event.attributes()));
        }
    }
    size
}

fn attributes_size(attributes: &FastHashMap<MetaString, AttributeValue>) -> usize {
    let entries = attributes
        .capacity()
        .saturating_mul(size_of::<(MetaString, AttributeValue)>() + 1);
    attributes.iter().fold(entries, |size, (key, value)| {
        size.saturating_add(key.len()).saturating_add(value_size(value))
    })
}

/// Returns the variable-sized contents of an attribute, excluding its inline representation.
fn value_size(value: &AttributeValue) -> usize {
    match value {
        AttributeValue::String(value) => value.len(),
        AttributeValue::Bool(_) | AttributeValue::Int(_) | AttributeValue::Float(_) => 0,
        AttributeValue::Bytes(value) => value.capacity(),
        AttributeValue::Array(values) => values.iter().fold(
            values.capacity().saturating_mul(size_of::<AttributeValue>()),
            |size, value| size.saturating_add(value_size(value)),
        ),
        AttributeValue::KeyValueList(values) => values.iter().fold(
            values
                .capacity()
                .saturating_mul(size_of::<(MetaString, AttributeValue)>()),
            |size, (key, value)| size.saturating_add(key.len()).saturating_add(value_size(value)),
        ),
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::event::trace::TraceIngestInfo;

    use super::*;

    #[test]
    fn estimates_include_variable_sized_data_at_every_trace_level() {
        // The buffer cap must account for large attributes, not only their inline enum representation (#2396).
        let blob = || AttributeValue::Bytes(vec![0; 16_384]);
        let attributes = || {
            [(MetaString::from_static("data"), blob())]
                .into_iter()
                .collect::<FastHashMap<_, _>>()
        };
        let empty = Trace::new(vec![], TraceIngestInfo::Datadog);
        let baseline = estimated_size(&empty);
        let mut traces = Vec::new();

        let mut resource = empty.clone();
        resource.attributes = std::sync::Arc::new(attributes());
        traces.push(resource);
        for span in [
            Span::default().with_resource("r".repeat(16_384)),
            Span::default().with_attributes(attributes()),
            Span::default().with_attributes(
                [(
                    MetaString::from_static("array"),
                    AttributeValue::Array(vec![AttributeValue::KeyValueList(vec![(
                        MetaString::from_static("nested"),
                        blob(),
                    )])]),
                )]
                .into_iter()
                .collect::<FastHashMap<_, _>>(),
            ),
            Span::default().with_span_links(vec![SpanLink::default().with_attributes(attributes())]),
            Span::default().with_span_events(vec![SpanEvent::default().with_attributes(attributes())]),
        ] {
            traces.push(Trace::new(vec![span], TraceIngestInfo::Datadog));
        }
        let mut payload = empty.clone();
        payload.payload.hostname = MetaString::from("h".repeat(16_384));
        traces.push(payload);
        let mut tags = empty;
        tags.payload.container_tags = Some(std::sync::Arc::new(vec![MetaString::from("t".repeat(16_384))]));
        traces.push(tags);

        for (case, trace) in traces.iter().enumerate() {
            assert!(estimated_size(trace) >= baseline + 16_384, "case {case}");
        }
    }
}
