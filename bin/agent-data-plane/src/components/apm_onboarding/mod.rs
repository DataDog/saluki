use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::{
    collections::{FastHashSet, PrehashedHashMap},
    strings::unsigned_integer_to_string,
};
use saluki_core::{
    components::{transforms::*, ComponentContext},
    data_model::event::trace::{Span, Trace},
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
        debug!(
            trace_id = spans[0].trace_id(),
            "Failed to reliably identify a root span for a trace."
        );
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
    if !span.meta().contains_key(key) {
        span.meta_mut().insert(key.clone(), value.clone());
    }
}
