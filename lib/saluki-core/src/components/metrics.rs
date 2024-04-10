use metrics::{counter, gauge, histogram, Counter, Gauge, Histogram, Label};

use super::ComponentContext;

#[derive(Clone)]
pub struct MetricsBuilder {
    context: ComponentContext,
}

impl MetricsBuilder {
    pub fn from_component_context(context: ComponentContext) -> Self {
        Self { context }
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

    pub fn register_counter(&self, metric_name: &'static str) -> Counter {
        self.with_labels(|labels| counter!(metric_name, labels), Vec::<Label>::new())
    }

    pub fn register_counter_with_labels<I, L>(&self, metric_name: &'static str, additional_labels: I) -> Counter
    where
        I: IntoIterator<Item = L>,
        L: Into<Label>,
    {
        self.with_labels(|labels| counter!(metric_name, labels), additional_labels)
    }

    #[allow(unused)]
    pub fn register_gauge(&self, metric_name: &'static str) -> Gauge {
        self.with_labels(|labels| gauge!(metric_name, labels), Vec::<Label>::new())
    }

    #[allow(unused)]
    pub fn register_gauge_with_labels<I, L>(&self, metric_name: &'static str, additional_labels: I) -> Gauge
    where
        I: IntoIterator<Item = L>,
        L: Into<Label>,
    {
        self.with_labels(|labels| gauge!(metric_name, labels), additional_labels)
    }

    pub fn register_histogram(&self, metric_name: &'static str) -> Histogram {
        self.with_labels(|labels| histogram!(metric_name, labels), Vec::<Label>::new())
    }

    pub fn register_histogram_with_labels<I, L>(&self, metric_name: &'static str, additional_labels: I) -> Histogram
    where
        I: IntoIterator<Item = L>,
        L: Into<Label>,
    {
        self.with_labels(|labels| histogram!(metric_name, labels), additional_labels)
    }
}
