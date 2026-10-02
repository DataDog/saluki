//! Controls which trace settings ADP receives from the core Agent through Remote Configuration.

use serde::Serialize;

/// Switches for remote trace sampling settings and OTLP trace attribute mappings.
///
/// ADP subscribes only in connected mode with a local trace pipeline. Each product switch also requires
/// [`enabled`](Self::enabled) to be `true`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Domain {
    /// Controls Remote Configuration (`remote_configuration.enabled`).
    ///
    /// Defaults to `true`. The data plane reads this value as given; unlike the Agent, it does not default it off on
    /// government sites or in FIPS mode.
    pub enabled: bool,

    /// Controls remote trace sampling updates (`remote_configuration.apm_sampling.enabled`).
    ///
    /// Defaults to `true`. Has no effect when [`enabled`](Self::enabled) is `false`.
    pub apm_sampling_enabled: bool,

    /// Controls remote OTLP trace attribute mappings (`remote_configuration.apm_semantics.enabled`).
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
