//! Snapshots of the contexts aggregation holds.

use saluki_core::data_model::event::metric::{context::Context, MetricValues};
use stringtheory::MetaString;

/// The shape of metric values retained by the aggregate transform.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AggregateMetricType {
    /// Counter values.
    Counter,

    /// Rate values.
    Rate,

    /// Gauge values.
    Gauge,

    /// Set values.
    Set,

    /// Histogram values.
    Histogram,

    /// Distribution values.
    Distribution,
}

impl From<&MetricValues> for AggregateMetricType {
    fn from(values: &MetricValues) -> Self {
        match values {
            MetricValues::Counter(_) => Self::Counter,
            MetricValues::Rate(_, _) => Self::Rate,
            MetricValues::Gauge(_) => Self::Gauge,
            MetricValues::Set(_) => Self::Set,
            MetricValues::Histogram(_) => Self::Histogram,
            MetricValues::Distribution(_) => Self::Distribution,
        }
    }
}

/// A retained metric context and its small aggregation metadata.
///
/// Cloning an entry shares the underlying context name and tags rather than copying their contents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AggregateContextSnapshotEntry {
    context: Context,
    metric_type: AggregateMetricType,
    unit: MetaString,
}

impl AggregateContextSnapshotEntry {
    pub(super) fn new(context: Context, metric_type: AggregateMetricType, unit: MetaString) -> Self {
        Self {
            context,
            metric_type,
            unit,
        }
    }

    /// Returns the retained metric context.
    pub fn context(&self) -> &Context {
        &self.context
    }

    /// Returns the shape of the retained metric values.
    pub fn metric_type(&self) -> AggregateMetricType {
        self.metric_type
    }

    /// Returns the unit attached to the retained metric values, if one is set.
    pub fn unit(&self) -> Option<&str> {
        if self.unit.is_empty() {
            None
        } else {
            Some(&self.unit)
        }
    }

    /// Creates a snapshot entry for downstream test and benchmark fixtures.
    #[cfg(any(test, feature = "test-util"))]
    pub fn for_test(context: Context, metric_type: AggregateMetricType, unit: MetaString) -> Self {
        Self::new(context, metric_type, unit)
    }
}

/// A read-only control message that asks an [`AggregateKernel`][super::AggregateKernel] for the contexts it holds.
///
/// The kernel appends an entry for every context it holds to [`entries`][Self::entries], so one message sent to every
/// instance of aggregation in turn collects all of their contexts; a context two instances hold appears twice. The
/// message is read-only: a host may send it while another call of the kernel is yielded, such as in the middle of a
/// flush, and the kernel answers from its state as it stands without disturbing the yielded call. A context a flush in
/// progress has emptied, but not yet removed, is listed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ContextSnapshot {
    /// The entries collected so far.
    pub entries: Vec<AggregateContextSnapshotEntry>,
}
