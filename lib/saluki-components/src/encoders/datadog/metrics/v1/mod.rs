use http::HeaderValue;
use saluki_common::iter::ReusableDeduplicator;
use saluki_context::tags::{SharedTagSet, Tag};
use saluki_core::data_model::event::metric::{Metric, MetricOrigin, MetricValues};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};

pub(super) const SERIES_COMPRESSED_SIZE_LIMIT: usize = 2_000_000; // ~2 MiB
pub(super) const SERIES_UNCOMPRESSED_SIZE_LIMIT: usize = 4_000_000; // ~4 MiB

pub(super) static CONTENT_TYPE: HeaderValue = HeaderValue::from_static("application/json");

// JSON framing for the V1 series payload, which wraps the array of `Serie` objects in a top-level object.
pub(super) const SERIES_PAYLOAD_PREFIX: &[u8] = b"{\"series\":[";
pub(super) const SERIES_PAYLOAD_SUFFIX: &[u8] = b"]}";
pub(super) const SERIES_INPUT_SEPARATOR: &[u8] = b",";

pub(super) fn encode_series_metric(
    metric: &Metric, additional_tags: &SharedTagSet, buffer: &mut Vec<u8>,
    tags_deduplicator: &mut ReusableDeduplicator<Tag>,
) -> Result<(), serde_json::Error> {
    let mut obj = JsonMap::new();

    obj.insert("metric".into(), JsonValue::String(metric.context().name().to_string()));

    let (type_str, points_iter, maybe_interval) = match metric.values() {
        MetricValues::Counter(points) => ("count", points.into_iter(), None),
        MetricValues::Rate(points, interval) => ("rate", points.into_iter(), Some(*interval)),
        MetricValues::Gauge(points) => ("gauge", points.into_iter(), None),
        MetricValues::Set(points) => ("gauge", points.into_iter(), None),
        _ => unreachable!("encode_series_metric called with non-series metric"),
    };

    let mut points = Vec::new();
    for (timestamp, value) in points_iter {
        let value = maybe_interval
            .map(|interval| value / interval.as_secs_f64())
            .unwrap_or(value);
        let timestamp = timestamp.map(|ts| ts.get()).unwrap_or(0) as i64;
        let value_json = JsonNumber::from_f64(value)
            .map(JsonValue::Number)
            .unwrap_or_else(|| JsonValue::from(0));
        points.push(JsonValue::Array(vec![JsonValue::from(timestamp), value_json]));
    }
    obj.insert("points".into(), JsonValue::Array(points));

    let deduplicated = get_deduplicated_tags(metric, additional_tags, tags_deduplicator);
    let mut tags_out = Vec::new();
    let mut device: Option<String> = None;
    for tag in deduplicated {
        if tag.name() == "dd.internal.resource" {
            continue;
        }
        if device.is_none() && tag.name() == "device" {
            if let Some(v) = tag.value() {
                device = Some(v.to_string());
                continue;
            }
        }
        tags_out.push(JsonValue::String(tag.as_str().to_string()));
    }
    obj.insert("tags".into(), JsonValue::Array(tags_out));

    obj.insert(
        "host".into(),
        JsonValue::String(metric.metadata().hostname().unwrap_or_default().to_string()),
    );

    if let Some(device) = device.filter(|device| !device.is_empty()) {
        obj.insert("device".into(), JsonValue::String(device));
    }

    obj.insert("type".into(), JsonValue::String(type_str.into()));

    let interval_secs = maybe_interval.map(|interval| interval.as_secs() as i64).unwrap_or(0);
    obj.insert("interval".into(), JsonValue::from(interval_secs));

    if let Some(MetricOrigin::SourceType(source_type)) = metric.metadata().origin() {
        obj.insert(
            "source_type_name".into(),
            JsonValue::String(source_type.as_ref().to_string()),
        );
    }

    if let Some(unit) = metric.metadata().unit() {
        if !unit.is_empty() {
            obj.insert("unit".into(), JsonValue::String(unit.to_string()));
        }
    }

    serde_json::to_writer(buffer, &JsonValue::Object(obj))
}

fn get_deduplicated_tags<'a>(
    metric: &'a Metric, additional_tags: &'a SharedTagSet, tags_deduplicator: &'a mut ReusableDeduplicator<Tag>,
) -> impl Iterator<Item = &'a Tag> {
    let chained_tags = metric
        .context()
        .tags()
        .into_iter()
        .chain(additional_tags)
        .chain(metric.context().origin_tags());

    tags_deduplicator.deduplicated(chained_tags)
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use saluki_common::iter::ReusableDeduplicator;
    use saluki_context::{tags::SharedTagSet, Context};
    use saluki_core::data_model::event::metric::{Metric, MetricMetadata, MetricOrigin, MetricValues};
    use serde_json::Value as JsonValue;
    use stringtheory::MetaString;

    use super::encode_series_metric;

    fn encode_one(metric: &Metric) -> JsonValue {
        let mut buf = Vec::new();
        let host_tags = SharedTagSet::default();
        let mut tags_deduplicator = ReusableDeduplicator::new();
        encode_series_metric(metric, &host_tags, &mut buf, &mut tags_deduplicator)
            .expect("encode_series_metric should succeed");
        serde_json::from_slice(&buf).expect("encoder produced invalid JSON")
    }

    #[test]
    fn basic_payload_shape() {
        // Each metric variant maps to the right `type` string, points are emitted as [ts, value] tuples,
        // and `interval`/`host` are always present (zero/empty when not set).
        let counter = Metric::counter("my.count", 5.0);
        let counter_json = encode_one(&counter);
        assert_eq!(counter_json["metric"], "my.count");
        assert_eq!(counter_json["type"], "count");
        assert_eq!(counter_json["interval"], 0);
        assert_eq!(counter_json["host"], "");
        assert_eq!(counter_json["tags"], JsonValue::Array(vec![]));
        let points = counter_json["points"].as_array().expect("points is array");
        assert_eq!(points.len(), 1);
        assert_eq!(points[0][0], 0);
        assert_eq!(points[0][1], 5.0);
        // Optional fields must be absent when not set.
        assert!(counter_json.get("unit").is_none());
        assert!(counter_json.get("source_type_name").is_none());
        assert!(counter_json.get("device").is_none());

        let rate = Metric::rate("my.rate", 30.0, Duration::from_secs(10));
        let rate_json = encode_one(&rate);
        assert_eq!(rate_json["type"], "rate");
        assert_eq!(rate_json["interval"], 10);
        // Rate value scaled by interval seconds: 30 / 10 = 3.
        let rate_points = rate_json["points"].as_array().expect("rate points is array");
        assert_eq!(rate_points[0][1], 3.0);

        let gauge = Metric::gauge("my.gauge", 42.0);
        let gauge_json = encode_one(&gauge);
        assert_eq!(gauge_json["type"], "gauge");

        // Sets are encoded as gauges with the set cardinality as the value, consistent with V2.
        let set = Metric::set("my.set", "alpha");
        let set_json = encode_one(&set);
        assert_eq!(set_json["type"], "gauge");
        let set_points = set_json["points"].as_array().expect("set points is array");
        assert_eq!(set_points[0][1], 1.0);
    }

    #[test]
    fn unit_and_hostname_emitted() {
        let context = Context::from_static_parts("my.timer.avg", &[]);
        let metadata = MetricMetadata::default()
            .with_unit(MetaString::from_static("millisecond"))
            .with_hostname(Some(Arc::from("host-1")));
        let gauge = Metric::from_parts(context, MetricValues::gauge([1.0_f64]), metadata);

        let json = encode_one(&gauge);
        assert_eq!(json["unit"], "millisecond");
        assert_eq!(json["host"], "host-1");
    }

    #[test]
    fn device_tag_extraction() {
        // A `device:<value>` tag is extracted into the `device` JSON field and dropped from `tags`.
        let context = Context::from_static_parts("my.metric", &["device:eth0", "env:prod"]);
        let counter = Metric::from_parts(context, MetricValues::counter([1.0_f64]), MetricMetadata::default());

        let json = encode_one(&counter);
        assert_eq!(json["device"], "eth0");
        let tags = json["tags"].as_array().expect("tags is array");
        let tag_strs: Vec<&str> = tags.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            !tag_strs.iter().any(|t| t.starts_with("device:")),
            "device tag must be removed: {:?}",
            tag_strs
        );
        assert!(tag_strs.contains(&"env:prod"));
    }

    #[test]
    fn source_type_name_from_source_type_origin() {
        let context = Context::from_static_parts("my.metric", &[]);
        let metadata = MetricMetadata::default().with_source_type(Some(Arc::from("integration_x")));
        let counter = Metric::from_parts(context, MetricValues::counter([1.0_f64]), metadata);

        let json = encode_one(&counter);
        assert_eq!(json["source_type_name"], "integration_x");
    }

    #[test]
    fn origin_metadata_dropped() {
        // OriginMetadata is V2-protobuf only; V1 must drop it.
        let context = Context::from_static_parts("my.metric", &[]);
        let metadata = MetricMetadata::default().with_origin(Some(MetricOrigin::dogstatsd()));
        let counter = Metric::from_parts(context, MetricValues::counter([1.0_f64]), metadata);

        let json = encode_one(&counter);
        assert!(json.get("source_type_name").is_none());
    }

    #[test]
    fn dd_internal_resource_dropped() {
        // `dd.internal.resource` is V2-protobuf-only; V1 must drop these tags silently.
        let context = Context::from_static_parts("my.metric", &["dd.internal.resource:host:foo", "env:prod"]);
        let counter = Metric::from_parts(context, MetricValues::counter([1.0_f64]), MetricMetadata::default());

        let json = encode_one(&counter);
        let tags = json["tags"].as_array().expect("tags is array");
        let tag_strs: Vec<&str> = tags.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            !tag_strs.iter().any(|t| t.starts_with("dd.internal.resource:")),
            "dd.internal.resource tag must be dropped: {:?}",
            tag_strs
        );
        assert!(tag_strs.contains(&"env:prod"));
    }
}
