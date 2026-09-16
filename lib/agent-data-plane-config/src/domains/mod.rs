//! Per-domain resolved config, grouped by ownership domain. Each field is owned by exactly one
//! subsystem; cross-cutting values live in `shared` instead.
// TODO: consider a different name instead of Domain

use serde::Serialize;

pub mod checks;
pub mod dogstatsd;
pub mod multi_region_failover;
pub mod otlp;
pub mod stateful_metrics;
pub mod traces;

/// Per-domain resolved configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct DomainConfiguration {
    pub stateful_metrics: stateful_metrics::Domain,
    pub dogstatsd: dogstatsd::Domain,
    pub otlp: otlp::Domain,
    pub traces: traces::Domain,
    pub checks: checks::Domain,
    pub multi_region_failover: multi_region_failover::Domain,
}
