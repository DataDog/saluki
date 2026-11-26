use std::{
    fmt, ptr,
    sync::{
        atomic::{AtomicBool, AtomicPtr, Ordering},
        Arc, LazyLock,
    },
};

use metrics::{counter, Counter};
use stringtheory::MetaString;

const ORIGIN_PRODUCT_AGENT: u32 = 10;
const ORIGIN_SUBPRODUCT_DOGSTATSD: u32 = 10;
const ORIGIN_SUBPRODUCT_INTEGRATION: u32 = 11;
const ORIGIN_PRODUCT_DETAIL_NONE: u32 = 0;

/// Metric metadata.
///
/// Metadata includes all information that's not specifically related to the context or value of the metric itself,
/// such as sample rate and timestamp.
#[must_use]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MetricMetadata {
    /// The metric origin.
    // TODO: only optional so we can default? seems like we always have one
    pub origin: Option<MetricOrigin>,

    unit: Unit,
    admission: DogStatsDAdmission,
}

// Keep a word for admission without enlarging Metric. Common DogStatsD unit states do not allocate.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
enum Unit {
    #[default]
    None,
    Millisecond,
    Custom(Arc<MetaString>),
}

impl MetricMetadata {
    /// Returns the metric origin.
    pub fn origin(&self) -> Option<&MetricOrigin> {
        self.origin.as_ref()
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

    /// Returns the unit of the metric values, if set.
    ///
    /// When configured to report timer units, the DogStatsD source sets `"millisecond"` for timing metrics (`ms`).
    /// Without that setting, those metrics have no unit.
    pub fn unit(&self) -> Option<&str> {
        match &self.unit {
            Unit::None => None,
            Unit::Millisecond => Some("millisecond"),
            Unit::Custom(unit) => Some(unit),
        }
    }

    /// Set the unit of the metric values.
    ///
    /// This variant is specifically for use in builder-style APIs.
    pub fn with_unit(mut self, unit: impl Into<MetaString>) -> Self {
        self.set_unit(unit);
        self
    }

    /// Sets the unit of the metric values, or clears it if the supplied string is empty.
    pub fn set_unit(&mut self, unit: impl Into<MetaString>) {
        let unit = unit.into();
        self.unit = match unit.as_ref() {
            "" => Unit::None,
            "millisecond" => Unit::Millisecond,
            _ => Unit::Custom(Arc::new(unit)),
        };
    }

    /// Fills a missing unit from another metric's metadata.
    pub fn fill_unit(&mut self, incoming: &Self) {
        if matches!(self.unit, Unit::None) {
            self.unit = incoming.unit.clone();
        }
    }

    /// Associates this metric with an original DogStatsD wire line, including its delimiter if present.
    pub fn track_dogstatsd_admission(&mut self, wire_bytes: usize) {
        self.admission = DogStatsDAdmission::new(wire_bytes);
    }

    /// Takes the admission decision for the original wire line, if there is one.
    pub fn take_dogstatsd_admission(&mut self) -> DogStatsDAdmission {
        std::mem::take(&mut self.admission)
    }
}

impl fmt::Display for MetricMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(origin) = &self.origin {
            write!(f, " origin={}", origin)?;
        }

        if let Some(unit) = self.unit() {
            write!(f, " unit={unit}")?;
        }

        Ok(())
    }
}

static ADMISSION_COUNTERS: LazyLock<AdmissionCounters> = LazyLock::new(|| AdmissionCounters {
    admitted: counter!("dsd_byte_budget_admitted_events_total"),
    refused: counter!("dsd_byte_budget_refused_events_total"),
    refused_bytes: counter!("dsd_byte_budget_refused_wire_bytes_total"),
});

struct AdmissionCounters {
    admitted: Counter,
    refused: Counter,
    refused_bytes: Counter,
}

/// A single DogStatsD line's byte-budget decision, independent of filters and other non-budget drops.
///
/// The production topology has at most one aggregating branch. If a line never reaches it (e.g. an
/// aggregation bypass, a filter), dropping the last copy records that the source byte budget accepted it.
///
/// The word is null for untracked metrics, a tagged wire length for unique ownership, or a strong `Arc` reference
/// shared between fan-out copies. The common, un-cloned input does not allocate a ticket.
#[derive(Debug, Default)]
pub struct DogStatsDAdmission(AtomicPtr<SharedAdmission>);

struct SharedAdmission {
    wire_bytes: usize,
    settled: AtomicBool,
}

impl DogStatsDAdmission {
    /// Publishes zero-valued meters before admission starts.
    pub fn register_counters() {
        LazyLock::force(&ADMISSION_COUNTERS);
    }

    fn new(wire_bytes: usize) -> Self {
        let state = if wire_bytes <= usize::MAX >> 1 {
            ptr::without_provenance_mut((wire_bytes << 1) | 1)
        } else {
            Arc::into_raw(Self::shared(wire_bytes)).cast_mut()
        };
        Self(AtomicPtr::new(state))
    }

    fn shared(wire_bytes: usize) -> Arc<SharedAdmission> {
        Arc::new(SharedAdmission {
            wire_bytes,
            settled: AtomicBool::new(false),
        })
    }

    /// Records a successful admission for a non-metric line.
    pub fn record_admitted() {
        ADMISSION_COUNTERS.admitted.increment(1);
    }

    /// Records a byte-budget refusal before a metric was created.
    pub fn record_refused(wire_bytes: usize) {
        ADMISSION_COUNTERS.refused_bytes.increment(wire_bytes as u64);
        ADMISSION_COUNTERS.refused.increment(1);
    }

    /// Settles this line as refused even if it was cloned before aggregation.
    pub fn refuse(mut self) {
        self.finish(false);
    }

    /// Settles this line as admitted at the aggregate admission point.
    pub fn admit(mut self) {
        self.finish(true);
    }

    fn finish(&mut self, admitted: bool) {
        let state = std::mem::replace(self.0.get_mut(), ptr::null_mut());
        if state.is_null() {
            return;
        }
        if state.addr() & 1 != 0 {
            Self::record(state.addr() >> 1, admitted);
        } else {
            // SAFETY: Each non-null untagged pointer owns one strong reference from `Arc::into_raw`.
            let shared = unsafe { Arc::from_raw(state) };
            if !shared.settled.swap(true, Ordering::Relaxed) {
                Self::record(shared.wire_bytes, admitted);
            }
        }
    }

    fn record(wire_bytes: usize, admitted: bool) {
        if admitted {
            Self::record_admitted();
        } else {
            Self::record_refused(wire_bytes);
        }
    }
}

impl Clone for DogStatsDAdmission {
    fn clone(&self) -> Self {
        let mut state = self.0.load(Ordering::Acquire);
        if state.is_null() {
            return Self::default();
        }
        if state.addr() & 1 != 0 {
            let shared = Arc::into_raw(Self::shared(state.addr() >> 1)).cast_mut();
            state = match self
                .0
                .compare_exchange(state, shared, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => shared,
                Err(winner) => {
                    // SAFETY: No other owner observed the failed publication. Disarm before dropping it.
                    let unused = unsafe { Arc::from_raw(shared) };
                    unused.settled.store(true, Ordering::Relaxed);
                    winner
                }
            };
        }
        // SAFETY: The borrowed admission owns a strong reference. A shared state is never replaced by `clone`.
        unsafe { Arc::increment_strong_count(state) };
        Self(AtomicPtr::new(state))
    }
}

impl Drop for DogStatsDAdmission {
    fn drop(&mut self) {
        let state = *self.0.get_mut();
        if state.is_null() || state.addr() & 1 != 0 {
            self.finish(true);
        } else {
            // SAFETY: The pointer owns one strong reference. Only the last copy may settle by default.
            drop(unsafe { Arc::from_raw(state) });
        }
    }
}

impl Drop for SharedAdmission {
    fn drop(&mut self) {
        if !*self.settled.get_mut() {
            DogStatsDAdmission::record_admitted();
        }
    }
}

// Admission custody does not change a metric's semantic metadata.
impl PartialEq for DogStatsDAdmission {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for DogStatsDAdmission {}

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
    /// when richer origin metadata isn't available.
    SourceType(Arc<str>),

    /// Originated from a specific product/subproduct, with product-specific detail.
    OriginMetadata {
        /// Product.
        product: u32,

        /// Subproduct.
        subproduct: u32,

        /// Product detail.
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
