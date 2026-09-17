//! Selective metric routing to configured Datadog intakes.

use std::collections::HashMap;

use serde::Serialize;

/// Resolved configuration for selectively routing metrics to configured intakes.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Domain {
    /// Exact metric names permitted to reach each configured intake.
    ///
    /// Defaults to an empty map, which leaves ordinary endpoint routing unchanged. Keys must exactly match the
    /// configured primary endpoint or an entry in `additional_endpoints`. Each value is a case-sensitive exact-name
    /// allowlist covering both series and sketches. An empty list drops all metrics for that endpoint.
    /// Operators can use this to reduce metric volume at selected destinations. Changes require a restart.
    pub metric_allowlists: HashMap<String, Vec<String>>,
}
