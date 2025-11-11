//! Forwarder implementations.
mod datadog;
pub use self::datadog::DatadogConfiguration;

mod trace_agent;
pub use self::trace_agent::TraceAgentForwarderConfiguration;
