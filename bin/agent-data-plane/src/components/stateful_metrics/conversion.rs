//! Convert enriched series using the same value and resource semantics as the V3 encoder.

use foldspace_core::{
    LogicalMetricSeries, MetricOrigin as FoldspaceOrigin, MetricPoint, MetricResource, MetricSeriesType, MetricTagSet,
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

    let mut resources = Vec::new();
    if let Some(host) = metric.context().host().filter(|host| !host.is_empty()) {
        resources.push(MetricResource::new("host", host));
    }
    let mut seen = FastHashSet::default();
    let mut device = None;
    for tag in metric
        .context()
        .origin_tags()
        .into_iter()
        .chain(metric.context().tags())
    {
        if !seen.insert(tag) {
            continue;
        }
        match (tag.name(), tag.value()) {
            ("device", Some(value)) => device = (!value.is_empty()).then_some(value),
            ("dd.internal.resource", Some(value)) => {
                if let Some((kind, name)) = value.split_once(':') {
                    if !kind.is_empty() && !name.is_empty() {
                        resources.push(MetricResource::new(kind, name));
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(device) = device {
        let index = usize::from(metric.context().host().is_some_and(|host| !host.is_empty()));
        resources.insert(index, MetricResource::new("device", device));
    }

    seen.clear();
    let tags = metric
        .context()
        .tags()
        .into_iter()
        .chain(metric.context().origin_tags())
        .filter(|tag| seen.insert(*tag))
        .filter(|tag| !(matches!(tag.name(), "device" | "dd.internal.resource") && tag.value().is_some()))
        .map(|tag| tag.as_str().to_owned())
        .collect();
    let mut series = LogicalMetricSeries::new(metric.context().name().as_ref(), kind, points)
        .with_tags(MetricTagSet::standalone(tags))
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
