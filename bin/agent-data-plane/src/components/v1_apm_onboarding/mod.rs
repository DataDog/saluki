use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::{
    collections::{FastHashSet, PrehashedHashMap},
    strings::unsigned_integer_to_string,
};
use saluki_core::{
    components::{transforms::*, ComponentContext},
    data_model::event::trace::v1::{V1AnyValue, V1KeyValue, V1Span, V1TraceChunk},
    topology::EventsBuffer,
};
use saluki_error::GenericError;
use stringtheory::MetaString;
use tracing::debug;

use super::install_info::InstallInfo;

static META_TAG_INSTALL_ID: MetaString = MetaString::from_static("_dd.install.id");
static META_TAG_INSTALL_TYPE: MetaString = MetaString::from_static("_dd.install.type");
static META_TAG_INSTALL_TIME: MetaString = MetaString::from_static("_dd.install.time");

/// V1 APM Onboarding synchronous transform.
///
/// Enriches V1 trace chunks on a service-by-service basis with metadata indicating that a given
/// service has been onboarded to Datadog APM. This is the `Event::V1Trace` counterpart to
/// `ApmOnboarding`.
#[derive(Default)]
pub struct V1ApmOnboardingConfiguration;

#[async_trait]
impl SynchronousTransformBuilder for V1ApmOnboardingConfiguration {
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        let install_info = match InstallInfo::load_or_create().await {
            Ok(info) => Some(info),
            Err(e) => {
                debug!(error = %e, "Failed to load or create install info. Skipping.");
                None
            }
        };

        Ok(Box::new(V1ApmOnboarding::from_install_info(install_info)))
    }
}

impl MemoryBounds for V1ApmOnboardingConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<V1ApmOnboarding>("component struct");
    }
}

pub struct V1ApmOnboarding {
    install_info: Option<InstallInfo>,
    first_span_by_service: FastHashSet<MetaString>,
}

impl V1ApmOnboarding {
    fn from_install_info(install_info: Option<InstallInfo>) -> Self {
        Self {
            install_info,
            first_span_by_service: FastHashSet::default(),
        }
    }

    fn enrich_chunk(&mut self, chunk: &mut V1TraceChunk) {
        let root_span = match get_root_span_from_chunk_mut(chunk) {
            Some(s) => s,
            None => {
                debug!("Failed to get the root span of the V1 trace chunk.");
                return;
            }
        };

        let service = root_span.service.clone();
        if !self.first_span_by_service.contains(&service) {
            self.first_span_by_service.insert(service);
            let install_info = self.install_info.as_ref().unwrap();
            add_onboarding_metadata_to_v1_span(root_span, install_info);
        }
    }
}

impl SynchronousTransform for V1ApmOnboarding {
    fn transform_buffer(&mut self, event_buffer: &mut EventsBuffer) {
        if self.install_info.is_none() {
            return;
        }

        for event in event_buffer {
            if let Some(v1_trace) = event.try_as_v1_trace_mut() {
                self.enrich_chunk(&mut v1_trace.chunk);
            }
        }
    }
}

fn get_root_span_from_chunk_mut(chunk: &mut V1TraceChunk) -> Option<&mut V1Span> {
    let spans = &mut chunk.spans;
    if spans.is_empty() {
        return None;
    }

    let mut parent_to_child = PrehashedHashMap::default();

    for (idx, span) in spans.iter().enumerate().rev() {
        if span.parent_id == 0 {
            return Some(&mut spans[idx]);
        }
        parent_to_child.insert(span.parent_id, idx);
    }

    for span in spans.iter() {
        parent_to_child.remove(&span.span_id);
    }

    if parent_to_child.len() != 1 {
        debug!("Failed to reliably identify a root span for a V1 trace chunk.");
    }

    if let Some(root_span_idx) = parent_to_child.values().next() {
        return Some(&mut spans[*root_span_idx]);
    }

    spans.last_mut()
}

fn add_onboarding_metadata_to_v1_span(span: &mut V1Span, install_info: &InstallInfo) {
    let install_time = unsigned_integer_to_string(install_info.install_time);
    add_v1_attribute_if_missing(span, META_TAG_INSTALL_ID.clone(), install_info.install_id.clone());
    add_v1_attribute_if_missing(span, META_TAG_INSTALL_TYPE.clone(), install_info.install_type.clone());
    add_v1_attribute_if_missing(span, META_TAG_INSTALL_TIME.clone(), install_time);
}

fn add_v1_attribute_if_missing(span: &mut V1Span, key: MetaString, value: MetaString) {
    if !span.attributes.iter().any(|kv| kv.key == key) {
        span.attributes.push(V1KeyValue {
            key,
            value: V1AnyValue::String(value),
        });
    }
}
