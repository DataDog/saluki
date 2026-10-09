//! A frozen copy of host tag enrichment as it was before applying host tags moved into this crate.
//!
//! The functions below are the original `agent-data-plane` transform (`host_tags`), copied verbatim with only their
//! imports and visibility adjusted. The component builder, which fetches the tags, and the memory bounds are left out,
//! since they carry no per-metric semantics. The property tests compare
//! [`HostTags`][crate::host_enrichment::HostTags] with this copy, so that the kernel keeps the native behavior while
//! the legacy component calls the kernel. Do not change it.

use std::time::{Duration, Instant};

use saluki_core::{
    components::transforms::SynchronousTransform,
    data_model::{event::metric::Metric, tags::SharedTagSet},
    topology::EventsBuffer,
};

pub(super) struct HostTagsEnrichment {
    pub(super) start: Instant,
    pub(super) expected_tags_duration: Duration,
    pub(super) host_tags: Option<SharedTagSet>,
}

impl HostTagsEnrichment {
    fn enrich_metric(&mut self, metric: &mut Metric) {
        let host_tags = match self.host_tags.as_ref() {
            Some(host_tags) => host_tags,
            None => return,
        };

        metric.context_mut().mutate_tags(|tags| tags.merge_shared(host_tags));
    }
}

impl SynchronousTransform for HostTagsEnrichment {
    fn transform_buffer(&mut self, event_buffer: &mut EventsBuffer) {
        // Skip adding host tags if duration has elapsed.
        if self.start.elapsed() >= self.expected_tags_duration {
            self.host_tags = None;
            return;
        }

        for event in event_buffer {
            if let Some(metric) = event.try_as_metric_mut() {
                self.enrich_metric(metric);
            }
        }
    }
}
