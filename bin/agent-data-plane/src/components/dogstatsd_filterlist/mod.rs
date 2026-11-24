//! Shared DogStatsD metric filterlist matcher delegated to [`saluki_metric_kernels::filterlist`].

pub use saluki_metric_kernels::filterlist::{compile_metric_filter, Blocklist, MetricNameFilter, MetricPrefixRule};
