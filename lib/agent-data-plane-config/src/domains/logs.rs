//! Logs domain configuration.

use serde::Serialize;

/// Resolved logs configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Domain {
    /// Stateful Foldspace transport configuration.
    pub stateful: StatefulEncoding,
}

/// Stateful Foldspace transport configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct StatefulEncoding {
    /// Whether logs use stateful gRPC encoding.
    ///
    /// Defaults to `false`. Enable this only when the configured logs intake supports Foldspace.
    pub enabled: bool,
}
