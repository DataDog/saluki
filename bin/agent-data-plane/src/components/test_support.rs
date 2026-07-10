//! Shared test fixtures for the agent-data-plane trace-processing components.
//!
//! The `make_span`/`make_trace` builders were previously copy-pasted verbatim across the OTTL
//! transform and filter processor test modules. Centralizing them here keeps the fixtures from
//! drifting apart and gives new trace components (for example, `apm_onboarding`) one place to reuse.

use std::collections::HashMap;
use std::sync::Arc;

use saluki_common::collections::FastHashMap;
use saluki_core::data_model::event::trace::{AttributeValue, Span, Trace};
use stringtheory::MetaString;

/// Builds a span whose attributes come from `meta` as string values.
///
/// The span is created with service `"svc"` and `parent_id` 0 (that is, a root span). When a test
/// needs a specific topology or service, chain the `Span` builder methods (`with_service`,
/// `with_parent_id`, and so on) onto the result.
pub(crate) fn make_span(_trace_id: u64, span_id: u64, meta: HashMap<String, String>) -> Span {
    let mut attr_map = FastHashMap::default();
    for (k, v) in meta {
        attr_map.insert(MetaString::from(k), AttributeValue::String(MetaString::from(v)));
    }
    Span::new("svc", "op", "res", "web", span_id, 0, 0, 0, 0).with_attributes(attr_map)
}

/// Builds a trace from `spans`, optionally seeding resource attributes from `key:value` strings.
pub(crate) fn make_trace(spans: Vec<Span>, resource_tags: Option<Vec<&'static str>>) -> Trace {
    let mut trace = Trace::new(spans);
    if let Some(tags) = resource_tags {
        let mut attrs = FastHashMap::default();
        for t in tags {
            if let Some((k, v)) = t.split_once(':') {
                attrs.insert(MetaString::from(k), AttributeValue::String(MetaString::from(v)));
            }
        }
        trace.attributes = Arc::new(attrs);
    }
    trace
}
