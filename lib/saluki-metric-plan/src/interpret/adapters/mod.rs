//! Opaque host adapters over native state machines, which the interpreter uses by default.
//!
//! - [`AggregateAdapter`]: native aggregation, over [`AggregateCore`][saluki_metric_kernels::aggregate::AggregateCore],
//!   for every `aggregate` node without an injected adapter.

mod aggregate;
pub use self::aggregate::AggregateAdapter;

#[cfg(test)]
mod compatibility_tests;
