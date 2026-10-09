//! Host enrichment, which adds what the host knows about itself to metrics.
//!
//! - [`HostEnrichment`] is the metric part of native host enrichment, which leaves metrics unchanged: it only sets the
//!   host of events and service checks, which are not metrics.
//! - [`HostTags`] adds the host's tags to metrics for a while after the process starts, until the remote endpoint is
//!   expected to add them itself.

use saluki_core::{
    data_model::{event::metric::Metric, tags::SharedTagSet},
    execution::{AdmissionTime, Kernel, Progress, Turn},
    topology::EventsBuffer,
};

/// The host enrichment kernel for metrics: it leaves every metric unchanged.
///
/// Native host enrichment sets the host of events and service checks that have none, and never touches metrics; the
/// native transform's tests pin that it leaves a metric's host as it is, whether unset, empty, or set.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostEnrichment;

impl Kernel for HostEnrichment {
    fn consume(&mut self, _turn: &mut Turn<'_>, _input: &mut EventsBuffer) -> Progress {
        Progress::Done
    }
}

/// Adds the host's tags to metrics admitted before a deadline.
///
/// The tags are appended to each metric's instrumented tags as they are, without removing values the metric already
/// has; origin tags, the host, the name, values, and metadata are left unchanged.
///
/// As a kernel, it decides per batch by the batch's admission time: a batch admitted before the deadline gains the
/// tags, and any other batch passes unchanged. A batch without an admission time, which a plan never runs, passes
/// unchanged too. Hosts that decide by their own clock instead, as the native transform does, call
/// [`apply_buffer`][Self::apply_buffer] themselves.
#[derive(Clone, Debug)]
pub struct HostTags {
    tags: SharedTagSet,
    deadline: AdmissionTime,
}

impl HostTags {
    /// Creates a kernel adding `tags` to metrics admitted before `deadline`.
    pub fn new(tags: SharedTagSet, deadline: AdmissionTime) -> Self {
        Self { tags, deadline }
    }

    /// Returns `true` if a batch admitted at `admitted_at` gains the host's tags: it was admitted before the deadline.
    pub fn applies_to(&self, admitted_at: Option<AdmissionTime>) -> bool {
        admitted_at.is_some_and(|admitted_at| admitted_at < self.deadline)
    }

    /// Adds the host's tags to one metric.
    pub fn apply(&self, metric: &mut Metric) {
        apply_tags(&self.tags, metric);
    }

    /// Adds the host's tags to every metric in a buffer, in place. Other events are left as they are.
    pub fn apply_buffer(&self, events: &mut EventsBuffer) {
        for event in events {
            if let Some(metric) = event.try_as_metric_mut() {
                self.apply(metric);
            }
        }
    }
}

/// Adds `tags` to the instrumented tags of `metric`, as native host tag enrichment does.
pub fn apply_tags(tags: &SharedTagSet, metric: &mut Metric) {
    metric.context_mut().mutate_tags(|existing| existing.merge_shared(tags));
}

impl Kernel for HostTags {
    fn consume(&mut self, turn: &mut Turn<'_>, input: &mut EventsBuffer) -> Progress {
        if self.applies_to(turn.admitted_at()) {
            self.apply_buffer(input);
        }
        Progress::Done
    }
}

#[cfg(test)]
mod reference;
#[cfg(test)]
mod tests;
