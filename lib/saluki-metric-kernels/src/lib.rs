//! Pure metric-processing kernels decoupled from Saluki's component and task runtime.
//!
//! Each module in this crate encapsulates deterministic, synchronous domain logic for a metric processing stage.
//! Async topology components and compiled pipeline engines delegate to these kernels so both paths share identical
//! semantics.

#![deny(warnings)]
#![deny(missing_docs)]

pub mod filterlist;
pub mod histogram;
pub mod metric_name;
pub mod routing_filter;
