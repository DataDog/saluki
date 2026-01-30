//! Configuration types for dreamweaver.

mod output;
mod root;
mod service;
mod template;
mod workload;

pub use self::output::OutputConfig;
pub use self::root::Config;
pub use self::service::ServiceDefinition;
pub use self::template::{LogTemplate, MetricTemplate, MetricType, ServiceTemplate, SpanKind, TraceTemplate};
pub use self::workload::WorkloadConfig;
