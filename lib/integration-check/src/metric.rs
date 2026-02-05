use super::Tags;

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum Type {
    Gauge = 0,
    Rate,
    Count,
    MonotonicCount,
    Counter,
    Histrogram,
    Historate,
}

// rtloader/include/rtloader_types.h
#[derive(Debug, Clone)]
pub struct Metric {
    pub metric_type: Type,
    pub name: String,
    pub value: f64,
    pub tags: Tags,
}
