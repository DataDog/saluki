use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A version-independent logical batch of metric series.
///
/// Construction and deserialization retain only named series with finite, nonempty points.
/// The series are immutable while owned by the batch, so buffering and encoding can trust them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LogicalMetricBatch {
    #[serde(deserialize_with = "deserialize_batch_series")]
    series: Vec<LogicalMetricSeries>,
}

impl LogicalMetricBatch {
    /// Creates a batch, dropping non-finite points and series with no name or remaining points.
    pub fn new(mut series: Vec<LogicalMetricSeries>) -> Self {
        series.retain_mut(|series| {
            if series.name.is_empty() {
                return false;
            }
            series.points.retain(|point| point.value.is_finite());
            !series.points.is_empty()
        });

        Self { series }
    }

    /// Returns the logical series without cloning them.
    pub fn into_series(self) -> Vec<LogicalMetricSeries> {
        self.series
    }

    /// Borrows the logical series in this batch.
    pub fn series(&self) -> &[LogicalMetricSeries] {
        &self.series
    }

    /// Returns whether this batch contains no series.
    pub fn is_empty(&self) -> bool {
        self.series.is_empty()
    }

    /// Returns the total number of points in the batch.
    pub fn point_count(&self) -> usize {
        self.series.iter().map(|series| series.points.len()).sum()
    }

    /// Moves another validated batch's series into this batch in input order.
    pub(super) fn append(&mut self, other: Self) {
        self.series.extend(other.series);
    }
}

fn deserialize_batch_series<'de, D>(deserializer: D) -> Result<Vec<LogicalMetricSeries>, D::Error>
where
    D: Deserializer<'de>,
{
    let series = Vec::<LogicalMetricSeries>::deserialize(deserializer)?;
    Ok(LogicalMetricBatch::new(series).into_series())
}

/// A complete logical metric series.
///
/// Mirrors the Agent's [`metrics.Serie`](https://github.com/DataDog/datadog-agent/blob/5a5e02261d33e02497cff1665fdf1647615aade6/pkg/metrics/series.go#L49-L62).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogicalMetricSeries {
    /// Metric name.
    name: String,
    /// Metric type.
    metric_type: MetricSeriesType,
    /// Base and per-series tags, retaining their prefix-sharing boundary.
    #[serde(deserialize_with = "deserialize_metric_tag_set")]
    tags: MetricTagSet,
    /// Ordered resource pairs.
    resources: Vec<MetricResource>,
    /// Rate interval in seconds, or zero when it does not apply.
    interval: u64,
    /// Unit of the metric values, or empty when unknown.
    #[serde(default)]
    unit: String,
    /// Series points.
    points: Vec<MetricPoint>,
    /// Optional source type name.
    source_type_name: Option<String>,
    /// Optional origin tuple.
    origin: Option<MetricOrigin>,
    /// Whether the intake should avoid indexing the series.
    no_index: bool,
}

impl LogicalMetricSeries {
    /// Creates a series with no tags, resources, unit, or origin metadata.
    pub fn new(
        name: impl Into<String>,
        metric_type: MetricSeriesType,
        points: Vec<MetricPoint>,
    ) -> Self {
        Self {
            name: name.into(),
            metric_type,
            tags: MetricTagSet::default(),
            resources: Vec::new(),
            interval: 0,
            unit: String::new(),
            points,
            source_type_name: None,
            origin: None,
            no_index: false,
        }
    }

    /// Returns the metric name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the metric type.
    pub const fn metric_type(&self) -> MetricSeriesType {
        self.metric_type
    }

    /// Returns the metric tags.
    pub const fn tags(&self) -> &MetricTagSet {
        &self.tags
    }

    /// Sets the metric tags.
    pub fn set_tags(&mut self, mut tags: MetricTagSet) {
        tags.normalize();
        self.tags = tags;
    }

    /// Sets the metric tags and returns the series.
    pub fn with_tags(mut self, tags: MetricTagSet) -> Self {
        self.set_tags(tags);
        self
    }

    /// Returns the ordered resource pairs.
    pub fn resources(&self) -> &[MetricResource] {
        &self.resources
    }

    /// Sets the ordered resource pairs.
    pub fn set_resources(&mut self, resources: Vec<MetricResource>) {
        self.resources = resources;
    }

    /// Sets the ordered resource pairs and returns the series.
    pub fn with_resources(mut self, resources: Vec<MetricResource>) -> Self {
        self.set_resources(resources);
        self
    }

    /// Returns the rate interval in seconds.
    pub const fn interval(&self) -> u64 {
        self.interval
    }

    /// Sets the rate interval in seconds.
    pub fn set_interval(&mut self, interval: u64) {
        self.interval = interval;
    }

    /// Sets the rate interval and returns the series.
    pub fn with_interval(mut self, interval: u64) -> Self {
        self.set_interval(interval);
        self
    }

    /// Returns the unit of the metric values when one is set.
    pub fn unit(&self) -> Option<&str> {
        if self.unit.is_empty() {
            None
        } else {
            Some(&self.unit)
        }
    }

    /// Sets the unit of the metric values.
    pub fn set_unit(&mut self, unit: impl Into<String>) {
        self.unit = unit.into();
    }

    /// Sets the unit and returns the series.
    pub fn with_unit(mut self, unit: impl Into<String>) -> Self {
        self.set_unit(unit);
        self
    }

    /// Returns the series points.
    pub fn points(&self) -> &[MetricPoint] {
        &self.points
    }

    /// Returns the source type name when one is set.
    pub fn source_type_name(&self) -> Option<&str> {
        self.source_type_name.as_deref()
    }

    /// Sets the source type name.
    pub fn set_source_type_name(&mut self, source_type_name: impl Into<Option<String>>) {
        self.source_type_name = source_type_name.into();
    }

    /// Sets the source type name and returns the series.
    pub fn with_source_type_name(mut self, source_type_name: impl Into<Option<String>>) -> Self {
        self.set_source_type_name(source_type_name);
        self
    }

    /// Returns the origin tuple when one is set.
    pub const fn origin(&self) -> Option<MetricOrigin> {
        self.origin
    }

    /// Sets the origin tuple.
    pub fn set_origin(&mut self, origin: impl Into<Option<MetricOrigin>>) {
        self.origin = origin.into();
    }

    /// Sets the origin tuple and returns the series.
    pub fn with_origin(mut self, origin: impl Into<Option<MetricOrigin>>) -> Self {
        self.set_origin(origin);
        self
    }

    /// Returns whether the intake should avoid indexing the series.
    pub const fn no_index(&self) -> bool {
        self.no_index
    }

    /// Sets whether the intake should avoid indexing the series.
    pub fn set_no_index(&mut self, no_index: bool) {
        self.no_index = no_index;
    }

    /// Sets whether the intake should avoid indexing the series and returns it.
    pub fn with_no_index(mut self, no_index: bool) -> Self {
        self.set_no_index(no_index);
        self
    }
}

/// Series metric type values from the metrics V3 protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum MetricSeriesType {
    /// A count submitted as a per-interval delta.
    Count = 1,
    /// A count normalized to a per-second rate.
    Rate = 2,
    /// A point-in-time value.
    Gauge = 3,
}

impl MetricSeriesType {
    pub(crate) const fn as_u64(self) -> u64 {
        self as u64
    }
}

/// A timestamped metric value.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricPoint {
    /// Unix timestamp in seconds.
    pub timestamp: i64,
    /// Metric value. Batch construction drops non-finite values; deserialization rejects them.
    #[serde(
        serialize_with = "serialize_finite_value",
        deserialize_with = "deserialize_finite_value"
    )]
    pub value: f64,
}

impl MetricPoint {
    /// Creates a metric point.
    pub const fn new(timestamp: i64, value: f64) -> Self {
        Self { timestamp, value }
    }
}

fn serialize_finite_value<S>(value: &f64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if value.is_finite() {
        serializer.serialize_f64(*value)
    } else {
        Err(serde::ser::Error::custom(
            "metric point value must be finite",
        ))
    }
}

fn deserialize_finite_value<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = f64::deserialize(deserializer)?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(
            "metric point value must be finite",
        ))
    }
}

/// Prefix-shareable tags for one series.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricTagSet {
    /// Tags shared by a group of series.
    pub prefix: Vec<String>,
    /// Tags specific to this series.
    pub values: Vec<String>,
}

impl MetricTagSet {
    /// Creates a standalone tagset without a shared prefix.
    pub fn standalone(values: Vec<String>) -> Self {
        let mut tags = Self {
            prefix: Vec::new(),
            values,
        };
        tags.normalize();
        tags
    }

    fn normalize(&mut self) {
        self.prefix.sort_unstable();
        self.values.sort_unstable();
    }
}

fn deserialize_metric_tag_set<'de, D>(deserializer: D) -> Result<MetricTagSet, D::Error>
where
    D: Deserializer<'de>,
{
    let mut tags = MetricTagSet::deserialize(deserializer)?;
    tags.normalize();
    Ok(tags)
}

/// A resource type/name pair.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MetricResource {
    /// Resource type, such as `host` or `device`.
    pub kind: String,
    /// Resource name.
    pub name: String,
}

impl MetricResource {
    /// Creates a resource pair.
    pub fn new(kind: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            name: name.into(),
        }
    }
}

/// Origin metadata carried by the metrics V3 protocol.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MetricOrigin {
    /// Origin product code.
    pub product: i32,
    /// Origin category code.
    pub category: i32,
    /// Origin service code.
    pub service: i32,
}

impl MetricOrigin {
    /// Creates an origin tuple.
    pub const fn new(product: i32, category: i32, service: i32) -> Self {
        Self {
            product,
            category,
            service,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_unit_round_trips_through_json() {
        let mut series = LogicalMetricSeries::new(
            "request.duration",
            MetricSeriesType::Gauge,
            vec![MetricPoint::new(1, 25.0)],
        );
        assert_eq!(series.unit(), None);

        series.set_unit("millisecond");
        let batch = LogicalMetricBatch::new(vec![series]);
        let serialized = serde_json::to_vec(&batch).unwrap();
        let deserialized: LogicalMetricBatch = serde_json::from_slice(&serialized).unwrap();

        assert_eq!(deserialized, batch);
        assert_eq!(deserialized.series()[0].unit(), Some("millisecond"));
    }

    #[test]
    fn missing_serialized_unit_defaults_to_empty() {
        let serialized = br#"{
            "series": [{
                "name": "request.duration",
                "metric_type": "Gauge",
                "tags": { "prefix": [], "values": [] },
                "resources": [],
                "interval": 0,
                "points": [{ "timestamp": 1, "value": 25.0 }],
                "source_type_name": null,
                "origin": null,
                "no_index": false
            }]
        }"#;

        let batch: LogicalMetricBatch = serde_json::from_slice(serialized).unwrap();

        assert_eq!(batch.series()[0].unit(), None);
    }

    #[test]
    fn json_rejects_non_finite_metric_values() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let point = MetricPoint::new(1, value);

            let error = serde_json::to_vec(&point).unwrap_err();

            assert!(error.to_string().contains("must be finite"));
        }
    }

    #[test]
    fn batch_filters_non_finite_metric_values_and_empty_series() {
        let valid_series = LogicalMetricSeries::new(
            "request.duration",
            MetricSeriesType::Gauge,
            vec![
                MetricPoint::new(1, f64::NAN),
                MetricPoint::new(2, f64::INFINITY),
                MetricPoint::new(3, f64::NEG_INFINITY),
                MetricPoint::new(4, 25.0),
            ],
        );
        let empty_series = LogicalMetricSeries::new(
            "invalid.request.duration",
            MetricSeriesType::Gauge,
            vec![
                MetricPoint::new(1, f64::NAN),
                MetricPoint::new(2, f64::INFINITY),
            ],
        );

        let batch = LogicalMetricBatch::new(vec![valid_series, empty_series]);

        assert_eq!(batch.series().len(), 1);
        assert_eq!(batch.series()[0].points(), [MetricPoint::new(4, 25.0)]);
    }

    #[test]
    fn construction_and_deserialization_filter_the_same_unusable_series() {
        let series = vec![
            LogicalMetricSeries::new("", MetricSeriesType::Gauge, vec![MetricPoint::new(1, 1.0)]),
            LogicalMetricSeries::new(
                "first",
                MetricSeriesType::Rate,
                vec![MetricPoint::new(2, 2.0)],
            )
            .with_interval(10)
            .with_unit("second"),
            LogicalMetricSeries::new("no.points", MetricSeriesType::Count, Vec::new()),
            LogicalMetricSeries::new(
                "second",
                MetricSeriesType::Count,
                vec![MetricPoint::new(3, 3.0)],
            ),
        ];
        let constructed = LogicalMetricBatch::new(series.clone());
        let deserialized: LogicalMetricBatch =
            serde_json::from_value(serde_json::json!({ "series": series })).unwrap();

        assert_eq!(constructed, deserialized);
        assert_eq!(constructed.series().len(), 2);
        assert_eq!(constructed.series()[0].name(), "first");
        assert_eq!(constructed.series()[0].unit(), Some("second"));
        assert_eq!(constructed.series()[1].name(), "second");
        assert_eq!(constructed.point_count(), 2);
    }

    #[test]
    fn deserialization_of_only_unusable_series_produces_an_empty_batch() {
        for series in [
            Vec::new(),
            vec![LogicalMetricSeries::new(
                "",
                MetricSeriesType::Gauge,
                vec![MetricPoint::new(1, 1.0)],
            )],
            vec![LogicalMetricSeries::new(
                "no.points",
                MetricSeriesType::Gauge,
                Vec::new(),
            )],
        ] {
            let constructed = LogicalMetricBatch::new(series.clone());
            let deserialized: LogicalMetricBatch =
                serde_json::from_value(serde_json::json!({ "series": series })).unwrap();

            assert_eq!(constructed, LogicalMetricBatch::default());
            assert_eq!(deserialized, constructed);
        }
    }

    #[test]
    fn series_normalizes_tag_order() {
        let series = LogicalMetricSeries::new(
            "request.duration",
            MetricSeriesType::Gauge,
            vec![MetricPoint::new(1, 25.0)],
        )
        .with_tags(MetricTagSet {
            prefix: vec!["region:us".to_string(), "env:prod".to_string()],
            values: vec!["team:metrics".to_string(), "service:api".to_string()],
        });

        assert_eq!(
            series.tags().prefix,
            vec!["env:prod".to_string(), "region:us".to_string()]
        );
        assert_eq!(
            series.tags().values,
            vec!["service:api".to_string(), "team:metrics".to_string()]
        );
    }

    #[test]
    fn standalone_tagset_normalizes_order() {
        let tags =
            MetricTagSet::standalone(vec!["team:metrics".to_string(), "service:api".to_string()]);

        assert_eq!(
            tags.values,
            vec!["service:api".to_string(), "team:metrics".to_string()]
        );
    }

    #[test]
    fn deserialized_series_normalizes_tag_order() {
        let serialized = br#"{
            "name": "request.duration",
            "metric_type": "Gauge",
            "tags": {
                "prefix": ["region:us", "env:prod"],
                "values": ["team:metrics", "service:api"]
            },
            "resources": [],
            "interval": 0,
            "points": [{ "timestamp": 1, "value": 25.0 }],
            "source_type_name": null,
            "origin": null,
            "no_index": false
        }"#;

        let series: LogicalMetricSeries = serde_json::from_slice(serialized).unwrap();

        assert_eq!(
            series.tags().prefix,
            vec!["env:prod".to_string(), "region:us".to_string()]
        );
        assert_eq!(
            series.tags().values,
            vec!["service:api".to_string(), "team:metrics".to_string()]
        );
    }

    #[test]
    fn series_builders_expose_complete_configuration() {
        let tags = MetricTagSet {
            prefix: vec!["env:prod".to_string()],
            values: vec!["service:api".to_string()],
        };
        let resources = vec![MetricResource::new("host", "web-1")];
        let origin = MetricOrigin::new(1, 2, 3);
        let series = LogicalMetricSeries::new(
            "request.duration",
            MetricSeriesType::Rate,
            vec![MetricPoint::new(1, 25.0)],
        )
        .with_tags(tags.clone())
        .with_resources(resources.clone())
        .with_interval(10)
        .with_unit("millisecond")
        .with_source_type_name("nginx".to_string())
        .with_origin(origin)
        .with_no_index(true);

        assert_eq!(series.name(), "request.duration");
        assert_eq!(series.metric_type(), MetricSeriesType::Rate);
        assert_eq!(series.tags(), &tags);
        assert_eq!(series.resources(), resources);
        assert_eq!(series.interval(), 10);
        assert_eq!(series.unit(), Some("millisecond"));
        assert_eq!(series.points(), [MetricPoint::new(1, 25.0)]);
        assert_eq!(series.source_type_name(), Some("nginx"));
        assert_eq!(series.origin(), Some(origin));
        assert!(series.no_index());
    }
}
