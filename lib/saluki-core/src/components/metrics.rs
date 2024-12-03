use metrics::{counter, gauge, histogram, Counter, Gauge, Histogram, Label, Level};

use super::ComponentContext;

// TODO: We might want to move this to `saluki-metrics`, since really the "apply component labels to all metrics" logic
// could just be achieved by having the builder generically take a set of labels to apply when constructed.
//
// We could potentially extend this even further to somehow integrate with `static_metrics!` such that we could
// instantiate the generated types with a metrics builder reference, allowing us to both use `static_metrics!` for its
// boilerplate reduction, and `MetricsBuilder` for its consistent labeling.

/// Component-specific metrics builder.
///
/// Provides a simple and ergonomic builder API for registering individual metrics that are scoped to a specific
/// component. This ensures that component-specific metrics are labeled sufficiently and consistently.
#[derive(Clone)]
pub struct MetricsBuilder {
    context: ComponentContext,
    labels: Vec<Label>,
}

impl MetricsBuilder {
    /// Creates a new `MetricsBuilder` with the given component context.
    pub fn from_component_context(context: ComponentContext) -> Self {
        Self {
            labels: get_component_labels(&context),
            context,
        }
    }

    fn with_labels<F, T, I, L>(&self, f: F, additional_labels: I) -> T
    where
        F: FnOnce(Vec<Label>) -> T,
        I: IntoIterator<Item = L>,
        L: Into<Label>,
    {
        let mut labels = Vec::new();
        labels.push(Label::new("component_id", self.context.component_id().to_string()));
        labels.push(Label::new("component_type", self.context.component_type()));
        for additional_label in additional_labels.into_iter() {
            labels.push(additional_label.into());
        }

        f(labels)
    }

    /// Sets a fixed set of labels to be applied to all metrics registered with this builder.
    ///
    /// These labels are in addition to the basic component labels (`component_id` and `component_type`) that are always
    /// added to metrics registered with this builder.
    pub fn with_fixed_labels<I, L>(mut self, fixed_labels: I) -> Self
    where
        I: IntoIterator<Item = L>,
        L: Into<Label>,
    {
        let mut labels = get_component_labels(&self.context);
        for fixed_label in fixed_labels.into_iter() {
            labels.push(fixed_label.into());
        }

        self.labels = labels;
        self
    }

    /// Registers a counter at debug verbosity.
    ///
    /// Labels are automatically added for the component identifier and type.
    pub fn register_debug_counter(&self, metric_name: &'static str) -> Counter {
        self.with_labels(
            |labels| counter!(level: Level::DEBUG, metric_name, labels),
            Vec::<Label>::new(),
        )
    }

    /// Registers a counter with additional labels at debug verbosity.
    ///
    /// Labels are automatically added for the component identifier and type, in addition to the provided labels.
    pub fn register_debug_counter_with_labels<I, L>(&self, metric_name: &'static str, additional_labels: I) -> Counter
    where
        I: IntoIterator<Item = L>,
        L: Into<Label>,
    {
        self.with_labels(
            |labels| counter!(level: Level::DEBUG, metric_name, labels),
            additional_labels,
        )
    }

    /// Registers a gauge at debug verbosity.
    ///
    /// Labels are automatically added for the component identifier and type.
    pub fn register_debug_gauge(&self, metric_name: &'static str) -> Gauge {
        self.with_labels(
            |labels| gauge!(level: Level::DEBUG, metric_name, labels),
            Vec::<Label>::new(),
        )
    }

    /// Registers a gauge with additional labels at debug verbosity.
    ///
    /// Labels are automatically added for the component identifier and type, in addition to the provided labels.
    pub fn register_debug_gauge_with_labels<I, L>(&self, metric_name: &'static str, additional_labels: I) -> Gauge
    where
        I: IntoIterator<Item = L>,
        L: Into<Label>,
    {
        self.with_labels(
            |labels| gauge!(level: Level::DEBUG, metric_name, labels),
            additional_labels,
        )
    }

    /// Registers a histogram at debug verbosity.
    ///
    /// Labels are automatically added for the component identifier and type.
    pub fn register_debug_histogram(&self, metric_name: &'static str) -> Histogram {
        self.with_labels(
            |labels| histogram!(level: Level::DEBUG, metric_name, labels),
            Vec::<Label>::new(),
        )
    }

    /// Registers a histogram with additional labels at debug verbosity.
    ///
    /// Labels are automatically added for the component identifier and type, in addition to the provided labels.
    pub fn register_debug_histogram_with_labels<I, L>(
        &self, metric_name: &'static str, additional_labels: I,
    ) -> Histogram
    where
        I: IntoIterator<Item = L>,
        L: Into<Label>,
    {
        self.with_labels(
            |labels| histogram!(level: Level::DEBUG, metric_name, labels),
            additional_labels,
        )
    }
}

fn get_component_labels(context: &ComponentContext) -> Vec<Label> {
    vec![
        Label::new("component_id", context.component_id().to_string()),
        Label::from_static_parts("component_type", context.component_type()),
    ]
}
