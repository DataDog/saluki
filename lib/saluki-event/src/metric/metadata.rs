use std::{fmt, sync::Arc};

use stringtheory::MetaString;

const ORIGIN_PRODUCT_AGENT: u32 = 10;
const ORIGIN_SUBPRODUCT_DOGSTATSD: u32 = 10;
const ORIGIN_SUBPRODUCT_INTEGRATION: u32 = 11;
const ORIGIN_PRODUCT_DETAIL_NONE: u32 = 0;

/// The entity from which a metric originated from.
///
/// While "source type" and `MetricOrigin` describe the origin of a metric in high-level terms -- "this metric
/// originated from the DogStatsD source", "this metric came from an integration check", etc -- the origin entity
/// details the _specific_ sender of an individual metric in containerized environments.
///
/// The origin entity will generally be the process ID of the metric sender, or the container ID, both of which are then
/// generally mapped to the relevant information for the metric, such as the orchestrator-level tags for the
/// container/pod/deployment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OriginEntity {
    /// Process ID of the sender.
    ProcessId(u32),

    /// Container ID of the sender.
    ///
    /// This will generally be the typical long hexadecimal string that is used by container runtimes like `containerd`,
    /// but may sometimes also be a different form, such as the container's cgroups inode.
    ContainerId(MetaString),
}

impl OriginEntity {
    /// Creates a new `OriginEntity` with the given container ID.
    pub fn container_id<S>(container_id: S) -> Self
    where
        S: Into<MetaString>,
    {
        Self::ContainerId(container_id.into())
    }
}

/// Metric metadata.
///
/// Metadata includes all information that is not specifically related to the context or value of the metric itself,
/// such as sample rate and timestamp.
#[must_use]
#[derive(Clone, Debug, Default)]
pub struct MetricMetadata {
    sample_rate: Option<f64>,
    timestamp: Option<u64>,
    hostname: Option<Arc<str>>,
    origin_entity: Option<OriginEntity>,
    origin: Option<MetricOrigin>,
}

impl MetricMetadata {
    /// Gets the sample rate.
    ///
    /// This value is between 0 and 1, inclusive.
    pub fn sample_rate(&self) -> Option<f64> {
        self.sample_rate
    }

    /// Gets the timestamp.
    pub fn timestamp(&self) -> Option<u64> {
        self.timestamp
    }

    /// Gets the hostname.
    pub fn hostname(&self) -> Option<&str> {
        self.hostname.as_deref()
    }

    /// Gets the origin entity.
    pub fn origin_entity(&self) -> Option<&OriginEntity> {
        self.origin_entity.as_ref()
    }

    /// Gets the metric origin.
    pub fn origin(&self) -> Option<&MetricOrigin> {
        self.origin.as_ref()
    }

    /// Set the sample rate.
    ///
    /// This value must be between 0 and 1, inclusive. If the value is outside of this range, it will be clamped to fit.
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_sample_rate(mut self, sample_rate: impl Into<Option<f64>>) -> Self {
        self.sample_rate = sample_rate.into().map(|sr| sr.clamp(0.0, 1.0));
        self
    }

    /// Set the sample rate.
    ///
    /// This value must be between 0 and 1, inclusive. If the value is outside of this range, it will be clamped to fit.
    pub fn set_sample_rate(&mut self, sample_rate: impl Into<Option<f64>>) {
        self.sample_rate = sample_rate.into().map(|sr| sr.clamp(0.0, 1.0));
    }

    /// Set the timestamp.
    ///
    /// Represented as a Unix timestamp, or the number of seconds since the Unix epoch. Generally based on the time the
    /// metric was received, but not always.
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_timestamp(mut self, timestamp: impl Into<Option<u64>>) -> Self {
        self.timestamp = timestamp.into();
        self
    }

    /// Set the timestamp.
    ///
    /// Represented as a Unix timestamp, or the number of seconds since the Unix epoch. Generally based on the time the
    /// metric was received, but not always.
    pub fn set_timestamp(&mut self, timestamp: impl Into<Option<u64>>) {
        self.timestamp = timestamp.into();
    }

    /// Set the hostname where the metric originated from.
    ///
    /// This could be specified as part of a metric payload that was received from a client, or set internally to the
    /// hostname where this process is running.
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_hostnam(mut self, hostname: impl Into<Option<Arc<str>>>) -> Self {
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

    /// Set the origin entity of the metric.
    ///
    /// Origin entity relates to the actual sender of the metric, such as the specific process/container, rather than
    /// just where the metric originated from categorically (i.e. the source type or `MetricOrigin`).
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_origin_entity(mut self, origin_entity: impl Into<Option<OriginEntity>>) -> Self {
        self.origin_entity = origin_entity.into();
        self
    }

    /// Set the origin entity of the metric.
    ///
    /// Origin entity relates to the actual sender of the metric, such as the specific process/container, rather than
    /// just where the metric originated from categorically (i.e. the source type or `MetricOrigin`).
    pub fn set_origin_entity(&mut self, origin_entity: impl Into<Option<OriginEntity>>) {
        self.origin_entity = origin_entity.into();
    }

    /// Set the metric origin to the given source type.
    ///
    /// Indicates the source of the metric, such as the product or service that emitted it, or the source component
    /// itself that emitted it.
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_source_type(mut self, source_type: impl Into<Option<String>>) -> Self {
        self.origin = source_type.into().map(MetricOrigin::SourceType);
        self
    }

    /// Set the metric origin to the given source type.
    ///
    /// Indicates the source of the metric, such as the product or service that emitted it, or the source component
    /// itself that emitted it.
    pub fn set_source_type(&mut self, source_type: impl Into<Option<String>>) {
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
        if let Some(timestamp) = self.timestamp {
            write!(f, "ts={}", timestamp)?;
        } else {
            write!(f, "ts=none")?;
        }

        if let Some(sample_rate) = self.sample_rate {
            write!(f, " sample_rate={}", sample_rate)?;
        }

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
#[derive(Clone, Debug)]
pub enum MetricOrigin {
    /// Originated from a generic source.
    ///
    /// This is used to set the origin of a metric as the source component type itself, such as `dogstatsd` or `otel`,
    /// when richer origin metadata is not available.
    SourceType(String),

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
