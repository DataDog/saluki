//! Selective metric routing to configured secondary Datadog intakes.

use std::collections::HashMap;

use serde::Serialize;

/// Resolved configuration for selectively routing series to configured secondary intakes.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Domain {
    /// Exact series metric names permitted to reach each configured secondary intake.
    ///
    /// Defaults to an empty map, which leaves ordinary endpoint routing unchanged. Keys must exactly match entries in
    /// `additional_endpoints`; the primary endpoint cannot be selected. Each value is a case-sensitive exact-name
    /// allowlist. A selected endpoint with an empty list receives neither series nor sketches. Changes require a
    /// restart.
    pub metric_allowlists: HashMap<String, Vec<String>>,
}

/// Routing settings for a metrics-mirroring branch.
///
/// This type preserves the live MRF routing contract while the generic endpoint-aware feature owns its policy map in
/// [`Domain`].
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Routing {
    /// Whether the metrics-mirroring branch is active.
    ///
    /// Defaults to `false`.
    pub enabled: bool,

    /// Exact metric names permitted to reach the branch.
    ///
    /// Defaults to empty. Empty-list behavior belongs to the branch consuming these settings: a strict allowlist sends
    /// nothing, while the MRF compatibility branch sends everything. Names are case-sensitive and are not trimmed or
    /// treated as patterns.
    pub allowlist: Vec<String>,
}
