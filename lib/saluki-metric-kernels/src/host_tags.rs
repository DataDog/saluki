//! Pure host-tag enrichment kernel for metrics.

use std::sync::Arc;

use saluki_core::data_model::{
    event::metric::Metric,
    tags::{SharedTagSet, Tag},
};
use stringtheory::MetaString;

/// Prefix of the infrastructure-mode tag mark that must not be attached to customer metrics.
const INFRA_MODE_TAG_PREFIX: &str = "infra_mode:";

/// Converts raw system host tags into the [`SharedTagSet`] attached to metrics.
///
/// Drops `infra_mode:*` tags so they do not move metrics out of the customer namespace.
pub fn filter_metric_host_tags<I, S>(system_tags: I) -> SharedTagSet
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    system_tags
        .into_iter()
        .filter(|tag| !tag.as_ref().starts_with(INFRA_MODE_TAG_PREFIX))
        .map(|s| Arc::from(s.as_ref()))
        .map(MetaString::from)
        .map(Tag::from)
        .collect()
}

/// Pure kernel that merges a set of host tags into metrics while active.
#[derive(Clone, Debug, Default)]
pub struct HostTagsKernel {
    host_tags: Option<SharedTagSet>,
}

impl HostTagsKernel {
    /// Creates a new `HostTagsKernel` with optional initial host tags.
    pub fn new(host_tags: Option<SharedTagSet>) -> Self {
        Self { host_tags }
    }

    /// Creates a new `HostTagsKernel` from raw system host tag strings, stripping `infra_mode:*` tags.
    pub fn from_system_tags<I, S>(system_tags: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let tags = filter_metric_host_tags(system_tags);
        Self {
            host_tags: (!tags.is_empty()).then_some(tags),
        }
    }

    /// Returns `true` when host tags are currently active.
    pub fn is_active(&self) -> bool {
        self.host_tags.is_some()
    }

    /// Clears the host tags so subsequent calls become no-ops.
    pub fn clear(&mut self) {
        self.host_tags = None;
    }

    /// Replaces the active host tags.
    pub fn set_host_tags(&mut self, host_tags: Option<SharedTagSet>) {
        self.host_tags = host_tags;
    }

    /// Merges the active host tags into `metric`.
    pub fn apply(&self, metric: &mut Metric) {
        let Some(host_tags) = self.host_tags.as_ref() else {
            return;
        };

        metric.context_mut().mutate_tags(|tags| tags.merge_shared(host_tags));
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::event::metric::context::Context;

    use super::*;

    #[test]
    fn attaches_host_tags_until_cleared() {
        let mut kernel = HostTagsKernel::from_system_tags(["env:prod", "role:web"]);
        assert!(kernel.is_active());

        let mut m1 = Metric::gauge(Context::from_static_parts("test", &[]), 1.0);
        kernel.apply(&mut m1);
        assert!(m1.context().tags().has_tag(Tag::from("env:prod")));
        assert!(m1.context().tags().has_tag(Tag::from("role:web")));

        kernel.clear();
        assert!(!kernel.is_active());

        let mut m2 = Metric::gauge(Context::from_static_parts("test", &[]), 1.0);
        kernel.apply(&mut m2);
        assert_eq!(m2.context().tags().len(), 0);
    }

    #[test]
    fn strips_infra_mode_tags_only() {
        let tags = filter_metric_host_tags([
            "env:prod",
            "infra_mode:cloud_cost_only",
            "infra_mode:",
            "infra_mode",
            "infra_mode_extra:x",
        ]);

        assert_eq!(tags.len(), 3);
        assert!(tags.has_tag(Tag::from("env:prod")));
        assert!(tags.has_tag(Tag::from("infra_mode")));
        assert!(tags.has_tag(Tag::from("infra_mode_extra:x")));
    }
}
