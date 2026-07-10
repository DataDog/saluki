use async_trait::async_trait;
use saluki_common::{
    collections::{FastHashSet, PrehashedHashMap},
    strings::unsigned_integer_to_string,
};
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{transforms::*, ComponentContext},
    data_model::event::trace::{AttributeValue, Span, Trace},
    topology::EventsBuffer,
};
use saluki_error::GenericError;
use stringtheory::MetaString;
use tracing::debug;

mod install_info;
use self::install_info::InstallInfo;

static META_TAG_INSTALL_ID: MetaString = MetaString::from_static("_dd.install.id");
static META_TAG_INSTALL_TYPE: MetaString = MetaString::from_static("_dd.install.type");
static META_TAG_INSTALL_TIME: MetaString = MetaString::from_static("_dd.install.time");

/// APM Onboarding synchronous transform.
///
/// Enriches traces on a service-by-service basis with metadata that indicates that a given service has been onboarded
/// to Datadog APM.
#[derive(Default)]
pub struct ApmOnboardingConfiguration;

#[async_trait]
impl SynchronousTransformBuilder for ApmOnboardingConfiguration {
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        let install_info = match InstallInfo::load_or_create().await {
            Ok(info) => Some(info),
            Err(e) => {
                debug!(error = %e, "Failed to load or create install info. Skipping.");
                None
            }
        };

        Ok(Box::new(ApmOnboarding::from_install_info(install_info)))
    }
}

impl MemoryBounds for ApmOnboardingConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // Capture the size of the heap allocation when the component is built.
        builder.minimum().with_single_value::<ApmOnboarding>("component struct");
    }
}

pub struct ApmOnboarding {
    install_info: Option<InstallInfo>,
    first_span_by_service: FastHashSet<MetaString>,
}

impl ApmOnboarding {
    fn from_install_info(install_info: Option<InstallInfo>) -> Self {
        Self {
            install_info,
            first_span_by_service: FastHashSet::default(),
        }
    }

    fn enrich_trace(&mut self, trace: &mut Trace) {
        // Find the root span of the trace.
        let root_span = match get_root_span_from_trace_mut(trace) {
            Some(root_span) => root_span,
            None => {
                debug!("Failed to get the root span of the trace.");
                return;
            }
        };

        // If we haven't yet seen a trace for the service this root span belongs to, we track that and attach the onboarding metadata
        // to the span's meta fields.
        //
        // We only attach the onboarding metadata if it isn't already present, as tracers can sometimes set this themselves.
        let service = root_span.service();
        if !self.first_span_by_service.contains(service) {
            self.first_span_by_service.insert(service.into());

            let install_info = self.install_info.as_ref().unwrap();
            add_onboarding_metadata_to_span(root_span, install_info);
        }
    }
}

impl SynchronousTransform for ApmOnboarding {
    fn transform_buffer(&mut self, event_buffer: &mut EventsBuffer) {
        // Fast path for when we failed to load/create the installation info.
        if self.install_info.is_none() {
            return;
        }

        for event in event_buffer {
            if let Some(trace) = event.try_as_trace_mut() {
                self.enrich_trace(trace)
            }
        }
    }
}

fn get_root_span_from_trace_mut(trace: &mut Trace) -> Option<&mut Span> {
    let trace_id = trace.trace_id_low;
    let spans = trace.spans_mut();
    if spans.is_empty() {
        return None;
    }

    let mut parent_to_child = PrehashedHashMap::default();

    // Iterate over the spans to build the parent-child relationship map, but in reverse order.
    //
    // This lets us optimize for the common case where tracers report the root span last.
    for (span_idx, span) in spans.iter().enumerate().rev() {
        if span.parent_id() == 0 {
            return Some(&mut spans[span_idx]);
        }

        // We don't care about overwriting here, so long as we have all parent IDs accounted for.
        parent_to_child.insert(span.parent_id(), span_idx);
    }

    // Range back over all spans, and remove entries from the parent-child map based on the span ID,
    // which with a well-formed trace should leave us with just the root span, as nothing else in the
    // trace should be referring to it.
    for span in spans.iter() {
        parent_to_child.remove(&span.span_id());
    }

    if parent_to_child.len() != 1 {
        debug!(trace_id, "Failed to reliably identify a root span for a trace.");
    }

    // Grab the root span from the parent-child map.
    //
    // If the trace is well-formed, there's only a single entry left. Otherwise, we pick a random span
    // by virtue of map iteration order being arbitrary.
    if let Some(root_span_idx) = parent_to_child.values().next() {
        return Some(&mut spans[*root_span_idx]);
    }

    // Things have gone very wrong and so we just take the last span in the trace.
    spans.last_mut()
}

fn add_onboarding_metadata_to_span(span: &mut Span, install_info: &InstallInfo) {
    let install_time = unsigned_integer_to_string(install_info.install_time);
    add_meta_entry_if_missing(span, &META_TAG_INSTALL_ID, &install_info.install_id);
    add_meta_entry_if_missing(span, &META_TAG_INSTALL_TYPE, &install_info.install_type);
    add_meta_entry_if_missing(span, &META_TAG_INSTALL_TIME, &install_time);
}

fn add_meta_entry_if_missing(span: &mut Span, key: &MetaString, value: &MetaString) {
    if !span.attributes.contains_key(key) {
        span.attributes
            .insert(key.clone(), AttributeValue::String(value.clone()));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use saluki_common::collections::FastHashMap;
    use saluki_core::data_model::event::Event;

    use super::*;

    fn span(service: &str, span_id: u64, parent_id: u64) -> Span {
        Span::new(service, "op", "res", "web", span_id, parent_id, 0, 0, 0)
    }

    fn test_install_info() -> InstallInfo {
        InstallInfo {
            install_id: MetaString::from("install-123"),
            install_type: MetaString::from("manual"),
            install_time: 1_234_567_890,
        }
    }

    /// Collects the string-valued attributes of the span with `span_id` from every trace in `buffer`.
    fn span_attrs_by_id(buffer: &EventsBuffer, span_id: u64) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for event in buffer {
            if let Event::Trace(trace) = event {
                for span in trace.spans() {
                    if span.span_id() == span_id {
                        for (k, v) in span.attributes.iter() {
                            if let AttributeValue::String(s) = v {
                                out.insert(k.as_ref().to_string(), s.as_ref().to_string());
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// Pushes `trace` through `onboarding.transform_buffer` and returns the resulting buffer.
    fn enrich(onboarding: &mut ApmOnboarding, trace: Trace) -> EventsBuffer {
        let mut buffer = EventsBuffer::default();
        assert!(
            buffer.try_push(Event::Trace(trace)).is_none(),
            "buffer should have capacity"
        );
        onboarding.transform_buffer(&mut buffer);
        buffer
    }

    #[test]
    fn root_span_lookup_returns_none_for_empty_trace() {
        let mut trace = Trace::new(vec![]);
        assert!(get_root_span_from_trace_mut(&mut trace).is_none());
    }

    #[test]
    fn root_span_lookup_finds_parentless_span_reported_last() {
        // Common case: the root (parent_id 0) is reported last, so the reverse scan returns it immediately.
        let mut trace = Trace::new(vec![span("web", 2, 1), span("web", 1, 0)]);
        let root = get_root_span_from_trace_mut(&mut trace).expect("a root span should be found");
        assert_eq!(root.span_id(), 1, "the parent_id==0 span is the root");
    }

    #[test]
    fn root_span_lookup_uses_dangling_parent_when_no_parentless_span() {
        // No span has parent_id 0; the root's parent points outside the trace (id 99). That single dangling
        // reference is what identifies the root.
        let mut trace = Trace::new(vec![span("web", 1, 99), span("web", 2, 1)]);
        let root = get_root_span_from_trace_mut(&mut trace).expect("a root span should be found");
        assert_eq!(root.span_id(), 1);
    }

    #[test]
    fn root_span_lookup_falls_back_to_last_span_for_malformed_trace() {
        // A parent/child cycle has no parent_id==0 span and no dangling parent, so nothing identifies the
        // root: the function falls back to the last span in the trace.
        let mut trace = Trace::new(vec![span("web", 1, 2), span("web", 2, 1)]);
        let root = get_root_span_from_trace_mut(&mut trace).expect("fallback should return the last span");
        assert_eq!(root.span_id(), 2, "a malformed trace falls back to the last span");
    }

    #[test]
    fn enriches_only_the_root_span_of_a_new_service() {
        let mut onboarding = ApmOnboarding::from_install_info(Some(test_install_info()));
        // Child reported first, root (parent_id 0) reported last.
        let buffer = enrich(&mut onboarding, Trace::new(vec![span("web", 2, 1), span("web", 1, 0)]));

        let root_attrs = span_attrs_by_id(&buffer, 1);
        assert_eq!(
            root_attrs.get("_dd.install.id").map(String::as_str),
            Some("install-123")
        );
        assert_eq!(root_attrs.get("_dd.install.type").map(String::as_str), Some("manual"));
        assert_eq!(
            root_attrs.get("_dd.install.time").map(String::as_str),
            Some("1234567890")
        );

        assert!(
            span_attrs_by_id(&buffer, 2).is_empty(),
            "non-root spans must not be enriched"
        );
    }

    #[test]
    fn only_the_first_trace_per_service_is_enriched() {
        let mut onboarding = ApmOnboarding::from_install_info(Some(test_install_info()));

        let first = enrich(&mut onboarding, Trace::new(vec![span("web", 1, 0)]));
        assert!(span_attrs_by_id(&first, 1).contains_key("_dd.install.id"));

        // The same service again: its root is not enriched a second time.
        let second = enrich(&mut onboarding, Trace::new(vec![span("web", 3, 0)]));
        assert!(
            span_attrs_by_id(&second, 3).is_empty(),
            "a service already seen must not be enriched again"
        );

        // A different service is still enriched.
        let third = enrich(&mut onboarding, Trace::new(vec![span("api", 4, 0)]));
        assert!(span_attrs_by_id(&third, 4).contains_key("_dd.install.id"));
    }

    #[test]
    fn existing_install_metadata_is_preserved() {
        let mut onboarding = ApmOnboarding::from_install_info(Some(test_install_info()));

        // A tracer already set the install id; the transform must not overwrite it, but still fills in the
        // entries that are missing.
        let mut attrs = FastHashMap::default();
        attrs.insert(
            META_TAG_INSTALL_ID.clone(),
            AttributeValue::String(MetaString::from("tracer-set-id")),
        );
        let root = Span::new("web", "op", "res", "web", 1, 0, 0, 0, 0).with_attributes(attrs);

        let buffer = enrich(&mut onboarding, Trace::new(vec![root]));
        let root_attrs = span_attrs_by_id(&buffer, 1);
        assert_eq!(
            root_attrs.get("_dd.install.id").map(String::as_str),
            Some("tracer-set-id"),
            "an install id already set by a tracer must not be overwritten"
        );
        assert_eq!(root_attrs.get("_dd.install.type").map(String::as_str), Some("manual"));
        assert_eq!(
            root_attrs.get("_dd.install.time").map(String::as_str),
            Some("1234567890")
        );
    }

    #[test]
    fn missing_install_info_leaves_traces_untouched() {
        let mut onboarding = ApmOnboarding::from_install_info(None);
        let buffer = enrich(&mut onboarding, Trace::new(vec![span("web", 1, 0)]));
        assert!(
            span_attrs_by_id(&buffer, 1).is_empty(),
            "with no install info, transform_buffer must be a no-op"
        );
    }

    #[test]
    fn empty_trace_is_skipped_and_later_traces_still_enriched() {
        // A trace with no spans has no root span, so `enrich_trace` returns early (before touching
        // `install_info`); the loop must continue and still enrich the following non-empty trace.
        let mut onboarding = ApmOnboarding::from_install_info(Some(test_install_info()));
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(Trace::new(vec![]))).is_none());
        assert!(buffer
            .try_push(Event::Trace(Trace::new(vec![span("web", 1, 0)])))
            .is_none());
        onboarding.transform_buffer(&mut buffer);

        assert!(span_attrs_by_id(&buffer, 1).contains_key("_dd.install.id"));
    }
}
