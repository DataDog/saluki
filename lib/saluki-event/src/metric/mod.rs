mod context;
use std::fmt;

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

impl fmt::Display for Metric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}[{} {}]", self.context, self.value, self.metadata)
    }
}
