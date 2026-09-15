//! Per-domain resolved config, grouped by ownership domain. Each field is owned by exactly one
//! subsystem; cross-cutting values live in `shared` instead.
// TODO: consider a different name instead of Domain

use serde::Serialize;

pub mod checks;
pub mod dogstatsd;
pub mod metric_mirroring;
pub mod multi_region_failover;
pub mod otlp;
pub mod traces;

/// Per-domain resolved configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct DomainConfiguration {
    pub dogstatsd: dogstatsd::Domain,
    pub otlp: otlp::Domain,
    pub traces: traces::Domain,
    pub checks: checks::Domain,
    pub metric_mirroring: metric_mirroring::Domain,
    pub multi_region_failover: multi_region_failover::Domain,
}
