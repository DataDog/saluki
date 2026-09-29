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
    /// allowlist covering both series and sketches. Metrics matching either this list or `metric_prefix_allowlists`
    /// are forwarded. If both lists for a selected endpoint are empty, all metrics to that endpoint are dropped.
    /// Operators can use this to reduce metric volume at selected destinations. Changes require a restart.
    pub metric_allowlists: HashMap<String, Vec<String>>,
    /// Permits metrics whose names start with a configured literal prefix to reach each intake.
    ///
    /// Defaults to an empty map, which adds no prefix policies and leaves `metric_allowlists` unchanged. Keys must
    /// exactly match the configured primary endpoint or an entry in `additional_endpoints`. Both series and sketches
    /// are forwarded when their normalized names match either an exact name in `metric_allowlists` or one of these
    /// prefixes. Matching is case-sensitive and treats every configured character literally.
    ///
    /// An empty string matches every valid metric name. If both lists for a selected endpoint are empty, all metrics
    /// to that endpoint are dropped. Endpoints absent from both maps retain ordinary routing.
    pub metric_prefix_allowlists: HashMap<String, Vec<String>>,
}
