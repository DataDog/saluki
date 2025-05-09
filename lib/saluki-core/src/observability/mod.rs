//! Internal observability support.

use saluki_metrics::MetricsBuilder;

use crate::components::ComponentContext;

pub mod metrics;

/// Helper trait for working with [`MetricsBuilder`].
pub trait ComponentMetricsExt {
    /// Creates a new instance with default tags derived from the given component context.
    ///
    /// Sets the following default tags:
    /// - `component_id` (the component ID, `ComponentContext::component_id`)
    /// - `component_type` (the component type, `ComponentContext::component_type`)
    fn from_component_context(context: ComponentContext) -> Self;
}

impl ComponentMetricsExt for MetricsBuilder {
    fn from_component_context(context: ComponentContext) -> Self {
        MetricsBuilder::default()
            .add_default_tag(("component_id", context.component_id().to_string()))
            .add_default_tag(("component_type", context.component_type().as_str()))
    }
}
