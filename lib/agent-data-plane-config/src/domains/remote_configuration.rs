//! Defines Remote Configuration and product settings for the data plane.

use serde::Serialize;

/// Holds resolved Remote Configuration switches for product selection.
///
/// A product switch has no effect unless [`enabled`](Self::enabled) is `true`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Domain {
    /// Controls Remote Configuration (`remote_configuration.enabled`).
    ///
    /// Defaults to `true`. The data plane reads this value as given; unlike the Agent, it does not default it off on
    /// government sites or in FIPS mode.
    pub enabled: bool,

    /// Controls trace sampling updates (`remote_configuration.apm_sampling.enabled`).
    ///
    /// Defaults to `true`. Has no effect when [`enabled`](Self::enabled) is `false`.
    pub apm_sampling_enabled: bool,

    /// Controls semantic-registry updates (`remote_configuration.apm_semantics.enabled`).
    ///
    /// Defaults to `false`. Has no effect when [`enabled`](Self::enabled) is `false`.
    pub apm_semantics_enabled: bool,
}

impl Default for Domain {
    fn default() -> Self {
        Self {
            enabled: true,
            apm_sampling_enabled: true,
            apm_semantics_enabled: false,
        }
    }
}
