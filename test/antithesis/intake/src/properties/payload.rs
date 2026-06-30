//! Payload property checks, one module per README Category column. Each module
//! holds its properties' checks, assertions, and error messages together.

pub(crate) mod bytes;
pub(crate) mod constants;
pub(crate) mod envelope;
pub(crate) mod metric_payload;
pub(crate) mod point;
pub(crate) mod resource;
pub(crate) mod series;
