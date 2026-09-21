//! Per-domain resolved config, grouped by ownership domain. Each field is owned by exactly one
//! subsystem; cross-cutting values live in `shared` instead.
// TODO: consider a different name instead of Domain

use serde::Serialize;

pub mod apm;
pub mod checks;
pub mod dogstatsd;
pub mod metrics_endpoint_routing;
pub mod multi_region_failover;
pub mod otlp;
pub mod traces;

/// Per-domain resolved configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct DomainConfiguration {
    pub apm: apm::Domain,
    pub dogstatsd: dogstatsd::Domain,
    pub otlp: otlp::Domain,
    pub traces: traces::Domain,
    pub checks: checks::Domain,
    pub metrics_endpoint_routing: metrics_endpoint_routing::Domain,
    pub multi_region_failover: multi_region_failover::Domain,
}
