const ORIGIN_PRODUCT_AGENT: u32 = 10;
const ORIGIN_SUBPRODUCT_DOGSTATSD: u32 = 10;
const ORIGIN_SUBPRODUCT_INTEGRATION: u32 = 11;
const ORIGIN_PRODUCT_DETAIL_NONE: u32 = 0;

/// Metric metadata.
///
/// Metadata includes all information that is not specifically related to the context or value of the metric itself,
/// such as sample rate and timestamp.
#[derive(Clone, Debug, Default)]
pub struct MetricMetadata {
    /// Sample rate of this metric
    ///
    /// A value between 0 and 1, inclusive.
    pub sample_rate: Option<f64>,

    /// Timestamp of the metric, in seconds since the Unix epoch.
    ///
    /// Generally based on the time the metric was received, but not always.
    pub timestamp: u64,

    /// Origin of the metric.
    ///
    /// Indicates the source of the metric, such as the product or service that emitted it, or the source component
    /// itself that emitted it.
    pub origin: Option<MetricOrigin>,
}

impl MetricMetadata {
    /// Creates a new `MetricMetadata` with the given timestamp.
    pub fn from_timestamp(timestamp: u64) -> Self {
        Self {
            sample_rate: None,
            timestamp,
            origin: None,
        }
    }

    /// Set the sample rate.
    pub fn with_sample_rate(mut self, sample_rate: impl Into<Option<f64>>) -> Self {
        self.sample_rate = sample_rate.into();
        self
    }

    /// Set the timestamp.
    pub fn with_timestamp(mut self, timestamp: u64) -> Self {
        self.timestamp = timestamp;
        self
    }

    /// Set the metric origin to the given source type.
    pub fn with_source_type(mut self, source_type: String) -> Self {
        self.origin = Some(MetricOrigin::SourceType(source_type));
        self
    }

    /// Set the metric origin to the given origin.
    pub fn with_origin(mut self, origin: MetricOrigin) -> Self {
        self.origin = Some(origin);
        self
    }
}

// TODO: This is not technically right.
//
// In practice, the Datadog Agent _does_ ship metrics with both source type name and origin metadata, although perhaps
// luckily, that is only the case for check metrics, which we don't deal with in ADP (yet).
//
// Eventually, we likely will have to consider exposing both of these fields.
#[derive(Clone, Debug)]
pub enum MetricOrigin {
    /// Originated from a generic source.
    ///
    /// This is used to set the origin of a metric as the source component type itself, such as `dogstatsd` or `otel`,
    /// when richer origin metadata is not available.
    SourceType(String),

    /// Originated from a specific product, category, and/or service.
    OriginMetadata {
        product: u32,

        // Previously known as "category".
        subproduct: u32,

        // Previously known as "service".
        product_detail: u32,
    },
}

impl MetricOrigin {
    /// Creates a `MetricsOrigin` for any metric ingested via DogStatsD.
    pub fn dogstatsd() -> Self {
        Self::OriginMetadata {
            product: ORIGIN_PRODUCT_AGENT,
            subproduct: ORIGIN_SUBPRODUCT_DOGSTATSD,
            product_detail: ORIGIN_PRODUCT_DETAIL_NONE,
        }
    }

    /// Creates a `MetricsOrigin` for any metric that originated via an JXM check integration.
    pub fn jmx_check(check_name: &str) -> Self {
        let product_detail = jmx_check_name_to_product_detail(check_name);

        Self::OriginMetadata {
            product: ORIGIN_PRODUCT_AGENT,
            subproduct: ORIGIN_SUBPRODUCT_INTEGRATION,
            product_detail,
        }
    }
}

fn jmx_check_name_to_product_detail(check_name: &str) -> u32 {
    // Taken from Datadog Agent mappings:
    // https://github.com/DataDog/datadog-agent/blob/fd3a119bda125462d578e0004f1370ee019ce2d5/pkg/serializer/internal/metrics/origin_mapping.go#L41
    match check_name {
        "jmx-custom-check" => 9,
        "activemq" => 12,
        "cassandra" => 28,
        "confluent_platform" => 40,
        "hazelcast" => 70,
        "hive" => 73,
        "hivemq" => 74,
        "hudi" => 76,
        "ignite" => 83,
        "jboss_wildfly" => 87,
        "kafka" => 90,
        "presto" => 130,
        "solr" => 147,
        "sonarqube" => 148,
        "tomcat" => 163,
        "weblogic" => 172,
        _ => 0,
    }
}
