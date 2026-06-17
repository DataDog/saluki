//! Seed-then-drive translation from Datadog source config into the ADP-native model.
//!
//! This module owns the [`Translator`] -- the [`DatadogConfigConsumer`] implementation that writes
//! witnessed Datadog values directly into the component-native structs embedded in a
//! [`SalukiConfiguration`]. Translation is two writers into one model:
//!
//! 1. The Saluki-schema-only seed (`saluki_only.seed()`) produces the base `SalukiConfiguration`
//!    with defaults plus Saluki-only fields.
//! 2. The Datadog witness `drive` overlays its (disjoint) schema fields on top via the
//!    `consume_<key>` methods.
//!
//! # Module shape
//!
//! The witness impl itself is mechanically thin (see [`datadog`]): each `consume_<key>` forwards to
//! a free function in the owning subsystem module ([`datadog::forwarder`], [`datadog::dogstatsd`],
//! etc.), which holds the real mapping logic, conversions, and fixups for its domain.
//!
//! [`DatadogConfigConsumer`]: datadog_agent_config::DatadogConfigConsumer
//! [`SalukiConfiguration`]: agent_data_plane_config::SalukiConfiguration

pub mod datadog;

use std::collections::HashMap;

use agent_data_plane_config::{SalukiConfiguration, SalukiOnlyConfiguration};
use datadog_agent_config::{drive, DatadogConfiguration, TranslateError};

/// The witness consumer that accumulates a [`SalukiConfiguration`] from a Datadog source parse.
///
/// The accumulator *is* the native model: `consume_<key>` methods write witnessed values straight
/// into `native`. The only additional state is scratch for the handful of native fields that are
/// genuinely assembled from multiple Datadog keys (the forwarder endpoint, which combines the API
/// key, primary URL/site, and additional endpoints), plus an error accumulator.
pub struct Translator {
    /// The in-progress native model. Seeded from the Saluki-only base, then overlaid by the drive.
    native: SalukiConfiguration,

    /// Scratch: the primary API key (`api_key`). Assembled into the forwarder endpoint in
    /// [`Translator::finish`].
    api_key: Option<String>,

    /// Scratch: the custom intake URL (`dd_url`). Assembled into the forwarder endpoint in
    /// [`Translator::finish`].
    dd_url: Option<String>,

    /// Scratch: the Datadog site (`site`). Assembled into the forwarder endpoint in
    /// [`Translator::finish`].
    site: Option<String>,

    /// Scratch: additional dual-shipping endpoints (`additional_endpoints`). Assembled into the
    /// forwarder endpoint in [`Translator::finish`].
    additional_endpoints: HashMap<String, Vec<String>>,

    /// Scratch: `aggregator_stop_timeout` (seconds). Combined with `forwarder_stop_timeout` to
    /// derive `control.stop_timeout` in [`Translator::finish`] when the seed left it unset.
    aggregator_stop_timeout_secs: u64,

    /// Scratch: `forwarder_stop_timeout` (seconds). See `aggregator_stop_timeout_secs`.
    forwarder_stop_timeout_secs: u64,

    /// Accumulated semantic translation errors. The first is surfaced by `drive`.
    errors: Vec<TranslateError>,
}

impl Translator {
    /// Creates a translator over a seeded base.
    ///
    /// `base` is the lowest-precedence starting point (typically `saluki_only.seed()`). The Datadog
    /// drive overlays its schema fields on top, so the base only usefully carries disjoint
    /// Saluki-schema-only fields.
    pub fn new(base: SalukiConfiguration) -> Self {
        Self {
            native: base,
            api_key: None,
            dd_url: None,
            site: None,
            additional_endpoints: HashMap::new(),
            // The Agent's own defaults: 2 seconds each. The drive overwrites these from the
            // witnessed values (which themselves default to 2).
            aggregator_stop_timeout_secs: 2,
            forwarder_stop_timeout_secs: 2,
            errors: Vec::new(),
        }
    }

    /// Returns a mutable reference to the in-progress native model.
    ///
    /// Used by subsystem setters that need to write into multiple native fields at once (for
    /// example, obfuscation fan-out and the OTLP traces copies).
    pub(crate) fn native_mut(&mut self) -> &mut SalukiConfiguration {
        &mut self.native
    }

    /// Resolves the multi-key scratch into the native model and returns it.
    ///
    /// Currently this assembles the forwarder endpoint from the stashed API key, custom URL, site,
    /// and additional endpoints (see [`datadog::forwarder::assemble_endpoint`]).
    pub fn finish(mut self) -> SalukiConfiguration {
        datadog::forwarder::assemble_endpoint(
            &mut self.native.components.forwarder.datadog.endpoint,
            self.api_key.take(),
            self.dd_url.take(),
            self.site.take(),
            std::mem::take(&mut self.additional_endpoints),
        );

        // `control.stop_timeout` is owned by the Saluki-only seed (`data_plane.stop_timeout`). When
        // that key is unset, the seed leaves the field at `Duration::ZERO` and the effective value
        // is derived from the Datadog `aggregator_stop_timeout` + `forwarder_stop_timeout` keys,
        // mirroring the binary's original derivation.
        if self.native.control.stop_timeout.is_zero() {
            self.native.control.stop_timeout =
                std::time::Duration::from_secs(self.aggregator_stop_timeout_secs + self.forwarder_stop_timeout_secs);
        }

        self.native
    }

    /// Records a semantic translation error. The first recorded error is surfaced by `drive`.
    pub(crate) fn record_error(&mut self, e: TranslateError) {
        self.errors.push(e);
    }

    /// Stashes the API key for endpoint assembly in [`Translator::finish`].
    pub(crate) fn set_api_key_scratch(&mut self, value: String) {
        self.api_key = Some(value);
    }

    /// Stashes the custom intake URL for endpoint assembly in [`Translator::finish`].
    pub(crate) fn set_dd_url_scratch(&mut self, value: String) {
        self.dd_url = Some(value);
    }

    /// Stashes the Datadog site for endpoint assembly in [`Translator::finish`].
    pub(crate) fn set_site_scratch(&mut self, value: String) {
        self.site = Some(value);
    }

    /// Stashes the additional dual-shipping endpoints for endpoint assembly in
    /// [`Translator::finish`].
    pub(crate) fn set_additional_endpoints_scratch(&mut self, value: HashMap<String, Vec<String>>) {
        self.additional_endpoints = value;
    }

    /// Stashes `aggregator_stop_timeout` (seconds) for the `control.stop_timeout` derivation.
    pub(crate) fn set_aggregator_stop_timeout_secs(&mut self, value: u64) {
        self.aggregator_stop_timeout_secs = value;
    }

    /// Stashes `forwarder_stop_timeout` (seconds) for the `control.stop_timeout` derivation.
    pub(crate) fn set_forwarder_stop_timeout_secs(&mut self, value: u64) {
        self.forwarder_stop_timeout_secs = value;
    }
}

/// Translates a Datadog source parse plus the Saluki-only seed into the ADP-native model.
///
/// This is the single reusable translation path used by both startup and dynamic retranslation:
/// seed a [`Translator`] from `saluki_only`, drive the Datadog witness over it, and finish.
///
/// # Errors
///
/// Returns the first [`TranslateError`] recorded while consuming a witnessed value (for example, a
/// value that cannot be parsed into its native destination). Callers decide policy: startup bails;
/// a dynamic update is rejected and the last-good config retained.
pub fn translate(
    saluki_only: &SalukiOnlyConfiguration, datadog: &DatadogConfiguration,
) -> Result<SalukiConfiguration, TranslateError> {
    let mut t = Translator::new(saluki_only.seed());
    drive(datadog, &mut t)?;
    Ok(t.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_then_drive_round_trips() {
        let saluki_only = SalukiOnlyConfiguration::default();
        let datadog = DatadogConfiguration::default();
        let native = translate(&saluki_only, &datadog).expect("default translation succeeds");
        // The drive clobbers with Datadog defaults; site default flows into the endpoint.
        assert_eq!(native.components.forwarder.datadog.endpoint.site, "datadoghq.com");
    }
}
