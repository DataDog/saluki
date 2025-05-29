use std::{fmt, sync::Arc};

const ORIGIN_PRODUCT_AGENT: u32 = 10;
const ORIGIN_SUBPRODUCT_DOGSTATSD: u32 = 10;
const ORIGIN_SUBPRODUCT_INTEGRATION: u32 = 11;
const ORIGIN_PRODUCT_DETAIL_NONE: u32 = 0;

/// Metric metadata.
///
/// Metadata includes all information that is not specifically related to the context or value of the metric itself,
/// such as sample rate and timestamp.
#[must_use]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MetricMetadata {
    /// The hostname where the metric originated from.
    pub hostname: Option<Arc<str>>,

    /// The metric origin.
    // TODO: only optional so we can default? seems like we always have one
    pub origin: Option<MetricOrigin>,
}

impl MetricMetadata {
    /// Returns the hostname.
    pub fn hostname(&self) -> Option<&str> {
        self.hostname.as_deref()
    }

    /// Returns the metric origin.
    pub fn origin(&self) -> Option<&MetricOrigin> {
        self.origin.as_ref()
    }

    /// Set the hostname where the metric originated from.
    ///
    /// This could be specified as part of a metric payload that was received from a client, or set internally to the
    /// hostname where this process is running.
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_hostname(mut self, hostname: impl Into<Option<Arc<str>>>) -> Self {
        self.hostname = hostname.into();
        self
    }

    /// Set the hostname where the metric originated from.
    ///
    /// This could be specified as part of a metric payload that was received from a client, or set internally to the
    /// hostname where this process is running.
    pub fn set_hostname(&mut self, hostname: impl Into<Option<Arc<str>>>) {
        self.hostname = hostname.into();
    }

    /// Set the metric origin to the given source type.
    ///
    /// Indicates the source of the metric, such as the product or service that emitted it, or the source component
    /// itself that emitted it.
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_source_type(mut self, source_type: impl Into<Option<Arc<str>>>) -> Self {
        self.origin = source_type.into().map(MetricOrigin::SourceType);
        self
    }

    /// Set the metric origin to the given source type.
    ///
    /// Indicates the source of the metric, such as the product or service that emitted it, or the source component
    /// itself that emitted it.
    pub fn set_source_type(&mut self, source_type: impl Into<Option<Arc<str>>>) {
        self.origin = source_type.into().map(MetricOrigin::SourceType);
    }

    /// Set the metric origin to the given origin.
    ///
    /// Indicates the source of the metric, such as the product or service that emitted it, or the source component
    /// itself that emitted it.
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_origin(mut self, origin: impl Into<Option<MetricOrigin>>) -> Self {
        self.origin = origin.into();
        self
    }

    /// Set the metric origin to the given origin.
    ///
    /// Indicates the source of the metric, such as the product or service that emitted it, or the source component
    /// itself that emitted it.
    pub fn set_origin(&mut self, origin: impl Into<Option<MetricOrigin>>) {
        self.origin = origin.into();
    }
}

impl fmt::Display for MetricMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(origin) = &self.origin {
            write!(f, " origin={}", origin)?;
        }

        Ok(())
    }
}

// TODO: This is not technically right.
//
// In practice, the Datadog Agent _does_ ship metrics with both source type name and origin metadata, although perhaps
// luckily, that is only the case for check metrics, which we don't deal with in ADP (yet).
//
// Eventually, we likely will have to consider exposing both of these fields.

/// Categorical origin of a metric.
///
/// This is used to describe, in high-level terms, where a metric originated from, such as the specific software package
/// or library that emitted. This is distinct from the `OriginEntity`, which describes the specific sender of the metric.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetricOrigin {
    /// Originated from a generic source.
    ///
    /// This is used to set the origin of a metric as the source component type itself, such as `dogstatsd` or `otel`,
    /// when richer origin metadata is not available.
    SourceType(Arc<str>),

    /// Originated from a specific product, category, and/or service.
    OriginMetadata {
        /// Product that emitted the metric.
        product: u32,

        /// Subproduct that emitted the metric.
        ///
        /// Previously known as "category".
        subproduct: u32,

        /// Product detail.
        ///
        /// Previously known as "service".
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

    /// Returns `true` if the origin of the metric is DogStatsD.
    pub fn is_dogstatsd(&self) -> bool {
        matches!(self, Self::OriginMetadata { subproduct, .. } if *subproduct == ORIGIN_SUBPRODUCT_DOGSTATSD)
    }
}

impl fmt::Display for MetricOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceType(source_type) => write!(f, "source_type={}", source_type),
            Self::OriginMetadata {
                product,
                subproduct,
                product_detail,
            } => write!(
                f,
                "product={} subproduct={} product_detail={}",
                product_id_to_str(*product),
                subproduct_id_to_str(*subproduct),
                product_detail_id_to_str(*product_detail),
            ),
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

fn product_id_to_str(product_id: u32) -> &'static str {
    match product_id {
        ORIGIN_PRODUCT_AGENT => "agent",
        _ => "unknown_product",
    }
}

fn subproduct_id_to_str(subproduct_id: u32) -> &'static str {
    match subproduct_id {
        ORIGIN_SUBPRODUCT_DOGSTATSD => "dogstatsd",
        ORIGIN_SUBPRODUCT_INTEGRATION => "integration",
        _ => "unknown_subproduct",
    }
}

fn product_detail_id_to_str(product_detail_id: u32) -> &'static str {
    match product_detail_id {
        // TODO: Map the JMX check integration product detail IDs to their respective names.
        ORIGIN_PRODUCT_DETAIL_NONE => "none",
        _ => "unknown_product_detail",
    }
}
