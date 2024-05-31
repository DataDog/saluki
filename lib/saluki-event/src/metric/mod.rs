use std::fmt;

mod metadata;
use saluki_context::Context;

pub use self::metadata::*;

mod value;
pub use self::value::MetricValue;

#[derive(Clone, Debug)]
pub struct Metric {
    pub context: Context,
    pub value: MetricValue,
    pub metadata: MetricMetadata,
}

impl Metric {
    pub fn into_parts(self) -> (Context, MetricValue, MetricMetadata) {
        (self.context, self.value, self.metadata)
    }

    pub fn from_parts(context: Context, value: MetricValue, metadata: MetricMetadata) -> Self {
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
