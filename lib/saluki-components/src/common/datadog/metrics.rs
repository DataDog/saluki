//! Validation-only reconstruction of sources omitted from the partial task snapshot.

use saluki_context::tags::Tag;
use saluki_core::data_model::event::metric::{Metric, MetricValues};

pub(crate) fn emittable_scalar_point(point: f64) -> bool {
    point.is_finite()
}

pub(crate) fn has_emittable_scalar_point(metric: &Metric) -> bool {
    match metric.values() {
        MetricValues::Counter(points) | MetricValues::Rate(points, _) | MetricValues::Gauge(points) => {
            points.into_iter().any(|(_, value)| emittable_scalar_point(value))
        }
        MetricValues::Set(points) => points.into_iter().any(|(_, value)| emittable_scalar_point(value)),
        MetricValues::Distribution(_) | MetricValues::Histogram(_) => false,
    }
}

pub(crate) fn is_foldspace_series(metric: &Metric) -> bool {
    matches!(
        metric.values(),
        MetricValues::Counter(_) | MetricValues::Rate(_, _) | MetricValues::Gauge(_) | MetricValues::Set(_)
    )
}

pub(crate) fn is_v3_series_device_tag(tag: &Tag) -> bool {
    tag.name() == "device" && tag.value().is_some()
}

pub(crate) fn is_v3_series_resource_tag(tag: &Tag) -> bool {
    tag.name() == "dd.internal.resource" && tag.value().is_some()
}
