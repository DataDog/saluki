mod context;
pub use self::context::*;

mod metadata;
pub use self::metadata::*;

mod value;
pub use self::value::MetricValue;

#[derive(Clone, Debug)]
pub struct Metric {
    pub context: MetricContext,
    pub value: MetricValue,
    pub metadata: MetricMetadata,
}

impl Metric {
    pub fn into_parts(self) -> (MetricContext, MetricValue, MetricMetadata) {
        (self.context, self.value, self.metadata)
    }

    pub fn from_parts(context: MetricContext, value: MetricValue, metadata: MetricMetadata) -> Self {
        Self {
            context,
            value,
            metadata,
        }
    }
}
