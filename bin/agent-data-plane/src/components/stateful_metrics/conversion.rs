//! Convert enriched series into logical metrics, preserving values and resource metadata.

use foldspace_core::{
    LogicalMetricSeries, MetricOrigin as FoldspaceOrigin, MetricPoint, MetricResource, MetricSeriesType, MetricTagSet,
    MetricTags,
};
use saluki_common::collections::FastHashSet;
use saluki_core::data_model::event::metric::{Metric, MetricOrigin, MetricValues};

pub(super) fn convert(metric: &Metric) -> Option<LogicalMetricSeries> {
    let (kind, points, interval) = match metric.values() {
        MetricValues::Counter(points) => (MetricSeriesType::Count, points.into_iter(), None),
        MetricValues::Gauge(points) => (MetricSeriesType::Gauge, points.into_iter(), None),
        MetricValues::Rate(points, interval) => (MetricSeriesType::Rate, points.into_iter(), Some(interval)),
        // Sets, histograms, and distributions retain their existing HTTP serialization.
        _ => return None,
    };
    let points: Vec<MetricPoint> = points
        .filter_map(|(timestamp, value)| {
            let value = interval
                .filter(|interval| !interval.is_zero())
                .map_or(value, |interval| value / interval.as_secs_f64());
            value
                .is_finite()
                .then(|| MetricPoint::new(timestamp.map_or(0, |ts| ts.get() as i64), value))
        })
        .collect();
    if points.is_empty() {
        return None;
    }

    let context = metric.context();
    let host = context.host().filter(|host| !host.is_empty());
    let mut resources = Vec::new();
    if let Some(host) = host {
        resources.push(MetricResource::new("host", host));
    }
    let (origin_tags, tags) = (context.origin_tags(), context.tags());
    let mut values = Vec::with_capacity(origin_tags.len() + tags.len());
    // Only the rare device and resource tags are deduplicated as they are read, by exact tag string: the last
    // distinct `device` tag wins and resources keep first-seen order, origin tags first. The sort below
    // deduplicates the rest.
    let mut seen_resource_tags = FastHashSet::default();
    let mut device = None;
    for tag in origin_tags.into_iter().chain(tags) {
        let tag_str = tag.as_str();
        match tag_str.split_once(':') {
            Some(("device", value)) => {
                if seen_resource_tags.insert(tag_str) {
                    device = (!value.is_empty()).then_some(value);
                }
            }
            Some(("dd.internal.resource", value)) => {
                if seen_resource_tags.insert(tag_str) {
                    if let Some((kind, name)) = value.split_once(':').filter(|(k, n)| !k.is_empty() && !n.is_empty()) {
                        resources.push(MetricResource::new(kind, name));
                    }
                }
            }
            // Interned, shared, and inlined tags clone without copying their strings.
            _ => values.push(tag.clone()),
        }
    }
    if let Some(device) = device {
        resources.push(MetricResource::new("device", device));
    }
    values.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    values.dedup_by(|a, b| a.as_str() == b.as_str());
    let mut series = LogicalMetricSeries::new(context.name().as_ref(), kind, points)
        // Already sorted, so the constructor's sort only verifies the order.
        .with_tags(MetricTagSet {
            prefix: MetricTags::default(),
            values: MetricTags::shared(values),
        })
        .with_resources(resources)
        .with_interval(interval.map_or(0, |interval| interval.as_secs()));
    if let Some(unit) = metric.metadata().unit() {
        series.set_unit(unit);
    }
    match metric.metadata().origin() {
        Some(MetricOrigin::SourceType(source)) => series.set_source_type_name(source.to_string()),
        Some(MetricOrigin::OriginMetadata {
            product,
            subproduct,
            product_detail,
        }) => {
            series.set_origin(FoldspaceOrigin::new(
                *product as i32,
                *subproduct as i32,
                *product_detail as i32,
            ));
        }
        None => {}
    }
    Some(series)
}
