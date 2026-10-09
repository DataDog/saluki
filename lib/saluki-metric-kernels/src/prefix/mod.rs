//! Namespace prefixing, which puts a configured namespace in front of metric names.
//!
//! [`NamespacePrefix`] renames each metric to `<namespace>.<name>`, unless its raw name starts with one of the
//! exclusions, which are compared against the name before prefixing. An empty namespace disables prefixing.
//!
//! DogStatsD listener filtering, which drops metrics whose intake name, after prefixing, is on the metric filterlist,
//! is not part of this kernel: a plan expresses it as a `split` on the intake name followed by a `drop`, and the legacy
//! component combines this kernel with a [`SortedSetMatcher`][crate::matcher::SortedSetMatcher].

use saluki_core::{
    data_model::event::metric::Metric,
    execution::{Kernel, Progress, Turn},
    topology::EventsBuffer,
};
use stringtheory::MetaString;

use crate::params::NamespacePrefixParams;

/// Puts a namespace in front of metric names.
///
/// Prefixing never fails and keeps everything but the name: host, tags, origin tags, values, and metadata pass
/// unchanged. Two names can become one: an excluded name can equal another name's prefixed form.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NamespacePrefix {
    /// The namespace followed by a `.`, or empty when prefixing is disabled.
    prefix: String,
    exclusions: Vec<String>,
}

impl NamespacePrefix {
    /// Creates a namespace prefix from its parameters.
    ///
    /// A `.` is appended to a non-empty namespace that does not already end with one, so that `tenant` and `tenant.`
    /// prefix alike.
    pub fn new(params: &NamespacePrefixParams) -> Self {
        let mut prefix = params.namespace.clone();
        if !prefix.is_empty() && !prefix.ends_with('.') {
            prefix.push('.');
        }
        Self {
            prefix,
            exclusions: params.exclusions.clone(),
        }
    }

    /// Returns `true` if names are prefixed, which is when the namespace is not empty.
    pub fn is_enabled(&self) -> bool {
        !self.prefix.is_empty()
    }

    /// Returns the name a metric named `name` carries after prefixing, or `None` when prefixing is disabled.
    ///
    /// A name starting with one of the exclusions keeps its name, so the returned name is `name` itself. Any other name
    /// becomes a new string: the namespace, its `.`, and `name`.
    pub fn prefixed_name(&self, name: &MetaString) -> Option<MetaString> {
        if self.prefix.is_empty() {
            return None;
        }

        if self.is_excluded(name) {
            return Some(name.clone());
        }

        let mut prefixed = self.prefix.clone();
        prefixed.push_str(name);
        Some(prefixed.into())
    }

    fn is_excluded(&self, name: &str) -> bool {
        self.exclusions.iter().any(|prefix| name.starts_with(prefix.as_str()))
    }

    /// Renames a metric, replacing its context with one that differs only by its name.
    ///
    /// The context is replaced even when `name` equals the current name, as the native transform does for excluded
    /// names.
    pub fn rename(metric: &mut Metric, name: MetaString) {
        let new_context = metric.context().with_name(name);
        *metric.context_mut() = new_context;
    }

    /// Prefixes the name of one metric, as [`prefixed_name`][Self::prefixed_name] decides.
    pub fn apply(&self, metric: &mut Metric) {
        if let Some(name) = self.prefixed_name(metric.context().name()) {
            Self::rename(metric, name);
        }
    }

    /// Prefixes the name of every metric in a buffer, in place. Other events are left as they are.
    pub fn apply_buffer(&self, events: &mut EventsBuffer) {
        if !self.is_enabled() {
            return;
        }
        for event in events {
            if let Some(metric) = event.try_as_metric_mut() {
                self.apply(metric);
            }
        }
    }
}

impl Kernel for NamespacePrefix {
    fn consume(&mut self, _turn: &mut Turn<'_>, input: &mut EventsBuffer) -> Progress {
        self.apply_buffer(input);
        Progress::Done
    }
}

#[cfg(test)]
mod reference;
#[cfg(test)]
mod tests;
