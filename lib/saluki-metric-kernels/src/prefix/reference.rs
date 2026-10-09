//! A frozen copy of the DogStatsD prefix filter as it was before namespace prefixing moved into this crate.
//!
//! The functions below are the original `agent-data-plane` transform (`dogstatsd_prefix_filter`), copied verbatim with
//! only their imports and visibility adjusted. The live filterlist view, the component builder, the memory bounds, and
//! the telemetry that only filterlist updates record are left out, since they carry no per-metric semantics; the
//! builder's normalization of the prefix is kept as [`DogStatsDPrefixFilter::new`], and the run loop's per-buffer step
//! as [`DogStatsDPrefixFilter::transform_buffer`]. The property tests compare
//! [`NamespacePrefix`][crate::prefix::NamespacePrefix], followed by the filterlist as a plan's `split` applies it, with
//! this copy, so that the kernel keeps the native behavior while the legacy component calls the kernel. Do not change
//! it.

use metrics::Counter;
use saluki_core::{data_model::event::metric::Metric, topology::EventsBuffer};
use tracing::debug;

use crate::matcher::SortedSetMatcher;

#[derive(Clone)]
struct FilterlistTelemetry {
    listener_filtered_points: Counter,
}

impl FilterlistTelemetry {
    fn noop() -> Self {
        Self {
            listener_filtered_points: Counter::noop(),
        }
    }

    fn increment_listener_filtered_points(&self) {
        self.listener_filtered_points.increment(1);
    }
}

pub(super) struct DogStatsDPrefixFilter {
    metric_prefix: String,
    metric_prefix_blocklist: Vec<String>,
    matcher: SortedSetMatcher,
    telemetry: FilterlistTelemetry,
}

impl DogStatsDPrefixFilter {
    /// The original builder's construction, without the live filterlist: `values` and `match_prefix` are the
    /// filterlist's current value.
    pub(super) fn new(
        metric_prefix: &str, metric_prefix_blocklist: Vec<String>, values: &[String], match_prefix: bool,
    ) -> Self {
        // Ensure our metric prefix has a trailing period so that we don't have to check for, and possibly add it, when we're
        // actually processing metrics.
        let mut metric_prefix = metric_prefix.to_string();
        if !metric_prefix.is_empty() && !metric_prefix.ends_with(".") {
            metric_prefix.push('.');
        }
        Self {
            metric_prefix,
            metric_prefix_blocklist,
            matcher: SortedSetMatcher::new(values.iter().map(String::as_str), match_prefix),
            telemetry: FilterlistTelemetry::noop(),
        }
    }

    pub(super) fn process_metric(&self, metric: &mut Metric) -> bool {
        let metric_name = metric.context().name().as_ref();

        if self.metric_prefix.is_empty() {
            if self.matcher.matches_intake_name(metric_name) {
                self.telemetry.increment_listener_filtered_points();
                debug!("Metric {} excluded due to blocklist.", metric_name);
                return false;
            }
        } else {
            // We don't want to prefix the metric if it has a prefix that is on our _prefix_ blocklist,
            // which ensures we don't prefix metrics that are already prefixed.
            let new_metric_name = if self.has_excluded_prefix(metric_name) {
                metric.context().name().clone()
            } else {
                let mut prefixed_metric_name = self.metric_prefix.clone();
                prefixed_metric_name.push_str(metric_name);
                prefixed_metric_name.into()
            };

            if self.matcher.matches_intake_name(&new_metric_name) {
                self.telemetry.increment_listener_filtered_points();
                debug!("Metric {} excluded due to blocklist.", new_metric_name);
                return false;
            }

            // Update metric with new name.
            let new_context = metric.context().with_name(new_metric_name);
            let existing_context = metric.context_mut();
            *existing_context = new_context;
        }

        true
    }

    fn has_excluded_prefix(&self, metric_name: &str) -> bool {
        !self.metric_prefix.is_empty()
            && self
                .metric_prefix_blocklist
                .iter()
                .any(|prefix| metric_name.starts_with(prefix))
    }

    /// The run loop's processing of one buffer, before it is dispatched.
    pub(super) fn transform_buffer(&self, events: &mut EventsBuffer) {
        events.remove_if(|event| match event.try_as_metric_mut() {
            // `process_metric` returns `true` if the metric should be kept, so we have to invert that
            // here to match the predicate structure, which will _remove_ the event if `true` is returned.
            Some(metric) => !self.process_metric(metric),
            None => true,
        });
    }
}
