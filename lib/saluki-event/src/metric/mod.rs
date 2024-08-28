//! Metric types.

mod metadata;
use std::time::Duration;

use saluki_context::Context;

pub use self::metadata::*;

mod value;
pub use self::value::{MetricValues, ScalarPoints, SetPoints, SketchPoints};

/// A metric.
///
/// Metrics represent the measurement of a particular quantity at a particular point in time. Several different metric
/// types exist that provide different views into the underlying quantity: counters for representing the quantities that
/// are aggregated/totaled over time, gauges for tracking the latest value of a quantity, and histograms for tracking
/// the distribution of a quantity.
///
/// ## Structure
///
/// A metric is composed of three parts: the context, the value, and the metadata.
///
/// The context represents the "full" name of the metric, which includes not only the name (e.g. `http_requests_total`),
/// but the tags as well. Effectively, a context is meant to be a unique name for a metric.
///
/// The value is precisely what it sounds like: the value of the metric. The value holds both the metric type and the
/// measurement (or measurements) tied to that metric type. This ensures that the measurement(s) are always represented
/// correctly for the given metric type.
///
/// The metadata contains ancillary data related to the metric, such as the timestamp, sample rate, and origination
/// information like hostname and sender.
#[derive(Clone, Debug)]
pub struct Metric {
    context: Context,
    values: MetricValues,
    metadata: MetricMetadata,
}

impl Metric {
    /// Creates a counter metric from the given context and value(s).
    ///
    /// Default metadata will be used.
    pub fn counter<C, V>(context: C, values: V) -> Self
    where
        C: Into<Context>,
        V: Into<ScalarPoints>,
    {
        Self {
            context: context.into(),
            values: MetricValues::counter(values),
            metadata: MetricMetadata::default(),
        }
    }

    /// Creates a gauge metric from the given context and value(s).
    ///
    /// Default metadata will be used.
    pub fn gauge<C, V>(context: C, values: V) -> Self
    where
        C: Into<Context>,
        V: Into<ScalarPoints>,
    {
        Self {
            context: context.into(),
            values: MetricValues::gauge(values),
            metadata: MetricMetadata::default(),
        }
    }

    /// Creates a rate metric from the given context and value(s).
    ///
    /// Default metadata will be used.
    pub fn rate<C, V>(context: C, values: V, interval: Duration) -> Self
    where
        C: Into<Context>,
        V: Into<ScalarPoints>,
    {
        Self {
            context: context.into(),
            values: MetricValues::rate(values, interval),
            metadata: MetricMetadata::default(),
        }
    }

    /// Creates a set metric from the given context and value(s).
    ///
    /// Default metadata will be used.
    pub fn set<C, V>(context: C, values: V) -> Self
    where
        C: Into<Context>,
        V: Into<SetPoints>,
    {
        Self {
            context: context.into(),
            values: MetricValues::set(values),
            metadata: MetricMetadata::default(),
        }
    }

    /// Creates a distribution metric from the given context and value(s).
    ///
    /// Default metadata will be used.
    pub fn distribution<C, V>(context: C, values: V) -> Self
    where
        C: Into<Context>,
        V: Into<SketchPoints>,
    {
        Self {
            context: context.into(),
            values: MetricValues::distribution(values),
            metadata: MetricMetadata::default(),
        }
    }

    /// Gets a reference to the context.
    pub fn context(&self) -> &Context {
        &self.context
    }

    /// Gets a mutable reference to the context.
    pub fn context_mut(&mut self) -> &mut Context {
        &mut self.context
    }

    /// Gets a reference to the values.
    pub fn values(&self) -> &MetricValues {
        &self.values
    }

    /// Gets a mutable reference to the values.
    pub fn values_mut(&mut self) -> &mut MetricValues {
        &mut self.values
    }

    /// Gets a reference to the metadata.
    pub fn metadata(&self) -> &MetricMetadata {
        &self.metadata
    }

    /// Gets a mutable reference to the metadata.
    pub fn metadata_mut(&mut self) -> &mut MetricMetadata {
        &mut self.metadata
    }

    /// Consumes the metric and returns the individual parts.
    pub fn into_parts(self) -> (Context, MetricValues, MetricMetadata) {
        (self.context, self.values, self.metadata)
    }

    /// Creates a `Metric` from the given parts.
    pub fn from_parts(context: Context, values: MetricValues, metadata: MetricMetadata) -> Self {
        Self {
            context,
            values,
            metadata,
        }
    }
}
