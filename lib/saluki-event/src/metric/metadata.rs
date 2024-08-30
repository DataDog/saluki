use std::{fmt, num::NonZeroU32, sync::Arc};

use ordered_float::OrderedFloat;
use serde::Deserialize;
use stringtheory::MetaString;

const ORIGIN_PRODUCT_AGENT: u32 = 10;
const ORIGIN_SUBPRODUCT_DOGSTATSD: u32 = 10;
const ORIGIN_SUBPRODUCT_INTEGRATION: u32 = 11;
const ORIGIN_PRODUCT_DETAIL_NONE: u32 = 0;

/// The cardinality of tags associated with the origin entity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(try_from = "String")]
pub enum OriginTagCardinality {
    /// Low cardinality.
    ///
    /// This generally covers tags which are static, or relatively slow to change, and generally results in a small
    /// number of unique values for the given tag key.
    Low,

    /// Orchestrator cardinality.
    ///
    /// This generally covers orchestrator-specific tags, such as Kubernetes pod UID, and lands somewhere between low
    /// and high cardinality.
    Orchestrator,

    /// High cardinality.
    ///
    /// This generally covers tags which frequently change and generally results in a large number of unique values for
    /// the given tag key.
    High,
}

impl<'a> TryFrom<&'a str> for OriginTagCardinality {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "low" => Ok(Self::Low),
            "high" => Ok(Self::High),
            "orch" | "orchestrator" => Ok(Self::Orchestrator),
            other => Err(format!("unknown tag cardinality type '{}'", other)),
        }
    }
}

impl TryFrom<String> for OriginTagCardinality {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_from(value.as_str())
    }
}

impl fmt::Display for OriginTagCardinality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Low => write!(f, "low"),
            Self::Orchestrator => write!(f, "orchestrator"),
            Self::High => write!(f, "high"),
        }
    }
}

/// The entity from which a metric originated from.
///
/// While "source type" and `MetricOrigin` describe the origin of a metric in high-level terms -- "this metric
/// originated from the DogStatsD source", "this metric came from an integration check", etc -- the origin entity
/// details the _specific_ sender of an individual metric in containerized environments.
///
/// The origin entity will generally be the process ID of the metric sender, or the container ID, both of which are then
/// generally mapped to the relevant information for the metric, such as the orchestrator-level tags for the
/// container/pod/deployment.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OriginEntity {
    /// Process ID of the sender.
    process_id: Option<NonZeroU32>,

    /// Container ID of the sender.
    ///
    /// This will generally be the typical long hexadecimal string that is used by container runtimes like `containerd`,
    /// but may sometimes also be a different form, such as the container's cgroups inode.
    container_id: Option<MetaString>,

    /// Pod UID of the sender.
    ///
    /// This is generally only used in Kubernetes environments to uniquely identify the pod. UIDs are equivalent to UUIDs.
    pod_uid: Option<MetaString>,

    /// Desired cardinality of any tags associated with the entity.
    ///
    /// This controls the cardinality of the tags added to this metric when enriching based on the available entity IDs.
    cardinality: Option<OriginTagCardinality>,
}

impl OriginEntity {
    /// Sets the process ID of the sender.
    ///
    /// Must be a non-zero value. If the value is zero, it is silently ignored.
    pub fn set_process_id(&mut self, process_id: u32) {
        self.process_id = NonZeroU32::new(process_id);
    }

    /// Sets the container ID of the sender.
    pub fn set_container_id<S>(&mut self, container_id: S)
    where
        S: Into<MetaString>,
    {
        self.container_id = Some(container_id.into());
    }

    /// Sets the pod UID of the sender.
    pub fn set_pod_uid<S>(&mut self, pod_uid: S)
    where
        S: Into<MetaString>,
    {
        self.pod_uid = Some(pod_uid.into());
    }

    /// Sets the desired cardinality of any tags associated with the entity.
    pub fn set_cardinality<S>(&mut self, cardinality: S)
    where
        S: Into<Option<OriginTagCardinality>>,
    {
        self.cardinality = cardinality.into();
    }

    /// Gets the process ID of the sender.
    pub fn process_id(&self) -> Option<u32> {
        self.process_id.map(NonZeroU32::get)
    }

    /// Gets the container ID of the sender.
    pub fn container_id(&self) -> Option<&str> {
        self.container_id.as_deref()
    }

    /// Gets the pod UID of the sender.
    pub fn pod_uid(&self) -> Option<&str> {
        self.pod_uid.as_deref()
    }

    /// Gets the desired cardinality of any tags associated with the entity.
    pub fn cardinality(&self) -> Option<OriginTagCardinality> {
        self.cardinality.as_ref().copied()
    }
}

/// Metric metadata.
///
/// Metadata includes all information that is not specifically related to the context or value of the metric itself,
/// such as sample rate and timestamp.
#[must_use]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetricMetadata {
    sample_rate: OrderedFloat<f64>,
    hostname: Option<Arc<str>>,
    origin_entity: OriginEntity,
    origin: Option<MetricOrigin>,
}

impl MetricMetadata {
    /// Gets the sample rate.
    ///
    /// This value is between 0 and 1, inclusive.
    pub fn sample_rate(&self) -> f64 {
        *self.sample_rate
    }

    /// Gets the hostname.
    pub fn hostname(&self) -> Option<&str> {
        self.hostname.as_deref()
    }

    /// Gets the origin entity.
    pub fn origin_entity(&self) -> &OriginEntity {
        &self.origin_entity
    }

    /// Gets the metric origin.
    pub fn origin(&self) -> Option<&MetricOrigin> {
        self.origin.as_ref()
    }

    /// Gets a mutable reference to the origin entity.
    pub fn origin_entity_mut(&mut self) -> &mut OriginEntity {
        &mut self.origin_entity
    }

    /// Set the sample rate.
    ///
    /// This value must be between 0 and 1, inclusive. If the value is outside of this range, it will be clamped to fit.
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_sample_rate(mut self, sample_rate: impl Into<Option<f64>>) -> Self {
        self.set_sample_rate(sample_rate);
        self
    }

    /// Set the sample rate.
    ///
    /// This value must be between 0 and 1, inclusive. If the value is outside of this range, it will be clamped to fit.
    pub fn set_sample_rate(&mut self, sample_rate: impl Into<Option<f64>>) {
        if let Some(sample_rate) = sample_rate.into().map(|sr| sr.clamp(0.0, 1.0)) {
            self.sample_rate = OrderedFloat(sample_rate);
        }
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

impl Default for MetricMetadata {
    fn default() -> Self {
        Self {
            sample_rate: OrderedFloat(1.0),
            hostname: None,
            origin_entity: OriginEntity::default(),
            origin: None,
        }
    }
}

impl fmt::Display for MetricMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sample_rate={}", self.sample_rate)?;

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
