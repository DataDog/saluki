use std::sync::Arc;
use std::time::Duration;

use crate::sources::checks::execution_context::ExecutionContext;
use saluki_context::{tags::TagSet, Context};
use saluki_core::data_model::event::eventd::{AlertType, EventD, Priority};
use saluki_core::data_model::event::metric::{Metric, MetricMetadata, MetricValues};
use saluki_core::data_model::event::service_check::{CheckStatus, ServiceCheck};
use saluki_core::data_model::event::Event;
use stringtheory::MetaString;

use integration_check::sink::{
    event, histogram,
    metric::{Metric as RsMetric, Type},
    service_check,
};

pub fn metric_to_event(metric: RsMetric, execution_context: &ExecutionContext) -> Event {
    let mut tags = TagSet::default();
    metric
        .tags
        .iter()
        .for_each(|(k, v)| tags.insert_tag(format!("{k}:{v}")));
    let context = Context::from_parts(metric.name, tags.into_shared());

    let metadata = MetricMetadata::default()
        .with_hostname(Some(execution_context.hostname().clone()))
        .with_source_type(Some(Arc::from("check")));

    let values = match metric.metric_type {
        Type::Gauge => MetricValues::gauge(metric.value),
        Type::Rate => MetricValues::rate(metric.value, Duration::from_secs(1)), // FIXME interval?
        Type::Count => MetricValues::counter(metric.value),
        Type::MonotonicCount => MetricValues::gauge(0.), // FIXME
        Type::Counter => MetricValues::counter(metric.value),
        Type::Histrogram => MetricValues::histogram(metric.value),
        Type::Historate => MetricValues::gauge(0.), // FIXME
    };

    Event::Metric(Metric::from_parts(context, values, metadata))
}

/// Converts a `dd_rs_checks` service check to a Saluki `Event`.
pub fn service_check_to_event(check: service_check::ServiceCheck, execution_context: &ExecutionContext) -> Event {
    let mut tags = TagSet::default();
    check.tags.iter().for_each(|(k, v)| tags.insert_tag(format!("{k}:{v}")));

    let status = match check.status {
        service_check::Status::Ok => CheckStatus::Ok,
        service_check::Status::Warning => CheckStatus::Warning,
        service_check::Status::Critical => CheckStatus::Critical,
        service_check::Status::Unknown => CheckStatus::Unknown,
    };

    let message = if check.message.is_empty() {
        None
    } else {
        Some(MetaString::from(check.message.as_str()))
    };

    let service_check = ServiceCheck::new(check.name, status)
        .with_hostname(Some(MetaString::from(execution_context.hostname().as_ref())))
        .with_message(message)
        .with_tags(tags.into_shared());

    Event::ServiceCheck(service_check)
}

/// Converts a `dd_rs_checks` event to a Saluki `Event`.
pub fn event_to_eventd(event: event::Event, execution_context: &ExecutionContext) -> Event {
    let mut tags = TagSet::default();
    event.tags.iter().for_each(|(k, v)| tags.insert_tag(format!("{k}:{v}")));

    let alert_type = if event.alert_type.is_empty() {
        None
    } else {
        match event.alert_type.as_str() {
            "error" => Some(AlertType::Error),
            "warning" => Some(AlertType::Warning),
            "info" => Some(AlertType::Info),
            "success" => Some(AlertType::Success),
            _ => None,
        }
    };

    let priority = if event.priority.is_empty() {
        None
    } else {
        match event.priority.as_str() {
            "low" => Some(Priority::Low),
            "normal" => Some(Priority::Normal),
            _ => None,
        }
    };

    let aggregation_key = if event.aggregation_key.is_empty() {
        None
    } else {
        Some(MetaString::from(event.aggregation_key.as_str()))
    };

    let source_type_name = if event.source_type_name.is_empty() {
        None
    } else {
        Some(MetaString::from(event.source_type_name.as_str()))
    };

    let eventd = EventD::new(event.title, event.text)
        .with_timestamp(event.timestamp)
        .with_hostname(Some(MetaString::from(execution_context.hostname().as_ref())))
        .with_aggregation_key(aggregation_key)
        .with_priority(priority)
        .with_source_type_name(source_type_name)
        .with_alert_type(alert_type)
        .with_tags(tags.into_shared());

    Event::EventD(eventd)
}

/// Converts a `dd_rs_checks` histogram to a Saluki `Event`.
///
/// Note: Histograms are converted to histogram metrics in Saluki.
pub fn histogram_to_event(histogram: histogram::Histogram, execution_context: &ExecutionContext) -> Event {
    let mut tags = TagSet::default();
    histogram
        .tags
        .iter()
        .for_each(|(k, v)| tags.insert_tag(format!("{k}:{v}")));
    let context = Context::from_parts(histogram.metric_name, tags.into_shared());

    let metadata = MetricMetadata::default()
        .with_hostname(Some(Arc::from(execution_context.hostname().as_ref())))
        .with_source_type(Some(Arc::from("check")));

    // Convert i64 value to f64 for histogram
    let values = MetricValues::histogram(histogram.value as f64);

    Event::Metric(Metric::from_parts(context, values, metadata))
}
