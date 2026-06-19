//! Translation layer from Datadog config to ADP-native config.
//!
//! This module owns the [`Translator`] -- the [`DatadogConfigConsumer`] implementation that writes
//! witnessed Datadog values directly into the ADP-native structs embedded in a
//! [`SalukiConfiguration`].
//!
//! 1. The Saluki-schema-only seed (`saluki_only.seed()`) produces the base `SalukiConfiguration`
//!    with defaults plus Saluki-only fields that cannot arrive from the Datadog agent.
//! 2. The Datadog witness `drive` overlays its config fields via the `consume_<key>` methods.
//!
//! [`DatadogConfigConsumer`]: datadog_agent_config::DatadogConfigConsumer
//! [`SalukiConfiguration`]: agent_data_plane_config::SalukiConfiguration

mod datadog;

use agent_data_plane_config::{SalukiConfiguration, SalukiOnlyConfiguration};
use datadog_agent_config::{drive, DatadogConfiguration, TranslateError};

/// The witness consumer that accumulates a [`SalukiConfiguration`] from a Datadog source parse.
///
/// The accumulator composes the output type [`SalukiConfiguration`], along with accumulated
/// translation errors.
#[allow(dead_code)]
pub(crate) struct Translator {
    /// The in-progress native model. Seeded from the Saluki-only base, then overlaid by the drive.
    pub(crate) saluki: SalukiConfiguration,

    /// Accumulated semantic translation errors. The first is surfaced by `drive`.
    errors: Vec<TranslateError>,
}

impl Translator {
    /// Creates a translator over a seeded base.
    ///
    /// `base` is the lowest-precedence starting point (typically `saluki_only.seed()`). The Datadog
    /// drive overlays its schema fields on top, so the base only usefully carries disjoint
    /// Saluki-schema-only fields.
    #[allow(dead_code)]
    pub(crate) fn new(base: SalukiConfiguration) -> Self {
        Self {
            saluki: base,
            errors: Vec::new(),
        }
    }

    /// Returns the finished native model.
    #[allow(dead_code)]
    pub(crate) fn finish(self) -> SalukiConfiguration {
        self.saluki
    }

    /// Records a semantic translation error. The first recorded error is surfaced by `drive`.
    #[allow(dead_code)]
    pub(crate) fn record_error(&mut self, e: TranslateError) {
        self.errors.push(e);
    }
}

/// Translates a Datadog source parse plus the Saluki-only seed into the ADP-native model.
///
/// This is the single reusable translation path used by both startup and each dynamic update:
/// seed a [`Translator`] from `saluki_only`, drive the Datadog witness over it, and finish.
///
/// # Errors
///
/// Returns the first [`TranslateError`] recorded while consuming a witnessed value (for example, a
/// value that cannot be parsed into its native destination). Callers decide policy: startup bails;
/// a dynamic update is rejected and the last-good config retained.
#[allow(dead_code)]
pub(crate) fn translate(
    saluki_only: &SalukiOnlyConfiguration, datadog: &DatadogConfiguration,
) -> Result<SalukiConfiguration, TranslateError> {
    let mut t = Translator::new(saluki_only.seed());
    drive(datadog, &mut t)?;
    Ok(t.finish())
}

#[cfg(test)]
mod tests {
    use saluki_component_config::dogstatsd::SourceConfig;

    use super::*;

    // Datadog Agent defaults clobber Saluki native defaults by design. Translation is
    // clobber-not-merge: a Datadog-schema field's default overwrites the native default even when
    // the two differ.
    // TODO: we need a mechanism for default alignment #1802
    #[test]
    fn seed_then_drive_default_succeeds_and_clobbers() {
        let saluki_only = SalukiOnlyConfiguration::default();
        let datadog = DatadogConfiguration::default();
        let config = translate(&saluki_only, &datadog).expect("default translation succeeds");
        // The drive ran: the Datadog default port (8125) flows into the native source.
        assert_eq!(config.components.dogstatsd.source.port, 8125);
        // Clobber, not merge: the Datadog default for `dogstatsd_capture_depth` is 0, and it
        // overwrites the native default of 1024 (the component later raises sub-1024 values).
        assert_eq!(config.components.dogstatsd.source.capture_depth, 0);
    }

    #[test]
    fn seed_and_drive_write_disjoint_dogstatsd_fields() {
        // Saluki-only seeds `allow_context_heap_allocs` (a Saluki-schema-only field); the Datadog
        // drive overlays `dogstatsd_port` (a Datadog-schema field). Both survive into one struct.
        let mut saluki_only = SalukiOnlyConfiguration::default();
        saluki_only.dogstatsd.allow_context_heap_allocs = Some(false);

        let datadog = DatadogConfiguration {
            dogstatsd_port: 7000,
            ..Default::default()
        };

        let config = translate(&saluki_only, &datadog).expect("translation succeeds");
        assert!(
            !config.components.dogstatsd.source.allow_context_heap_allocations,
            "seeded Saluki-only field survives"
        );
        assert_eq!(
            config.components.dogstatsd.source.port, 7000,
            "Datadog drive overlays its field"
        );
    }

    #[test]
    fn drive_overlays_dogstatsd_source() {
        let saluki_only = SalukiOnlyConfiguration::default();
        let datadog = DatadogConfiguration {
            dogstatsd_buffer_size: 4096,
            dogstatsd_non_local_traffic: true,
            dogstatsd_tag_cardinality: "high".to_string(),
            ..Default::default()
        };

        let config = translate(&saluki_only, &datadog).expect("translation succeeds");
        let source: &SourceConfig = &config.components.dogstatsd.source;
        assert_eq!(source.buffer_size, 4096);
        assert!(source.non_local_traffic);
        assert_eq!(
            source.origin_enrichment.tag_cardinality,
            saluki_context::origin::OriginTagCardinality::High
        );
    }

    #[test]
    fn malformed_value_surfaces_as_error() {
        let saluki_only = SalukiOnlyConfiguration::default();
        let datadog = DatadogConfiguration {
            dogstatsd_tag_cardinality: "bogus".to_string(),
            ..Default::default()
        };
        assert!(translate(&saluki_only, &datadog).is_err());
    }
}
