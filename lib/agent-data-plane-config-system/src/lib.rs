//! The Configuration system: translation and subscription of external configuration sources into an
//! ADP-typed model.
//!
//! This crate turns configuration sources into `SalukiConfiguration`:
//!
//! - the typed Datadog source (`DatadogConfiguration`), whose supported keys the generated `drive`
//!   feeds to `DatadogTranslator` (a `DatadogConfigWitness`) one key at a time, and
//! - the Saluki-schema-only source ([`SalukiOnly`]), whose values seed the fields the Datadog
//!   schema does not cover.
//!
//! [`translate`] runs both writers and returns the assembled model. This is the only ADP production
//! crate that bridges the source configuration to the model; it constructs no components and does
//! not depend on `saluki-components`.

mod saluki_only;
mod translators;

use agent_data_plane_config::SalukiConfiguration;
use datadog_agent_config::{DatadogConfiguration, TranslateError};
pub use saluki_only::SalukiOnly;
pub use translators::ConfigTranslator;
use translators::DatadogTranslator;

/// Translates the Datadog and Saluki-only sources into one [`SalukiConfiguration`].
///
/// The Datadog `drive` feeds every supported key in `datadog` to a `DatadogTranslator`, which
/// assembles the multi-key endpoint field. Finally, the Saluki-only values seed their disjoint
/// destinations.
///
/// # Errors
///
/// Returns the first [`TranslateError`] recorded while consuming a Datadog value (for example, an
/// enum or byte-size string that cannot be parsed). Seeding does not fail.
pub fn translate(
    datadog: &DatadogConfiguration, saluki_only: &SalukiOnly,
) -> Result<SalukiConfiguration, TranslateError> {
    let mut config = DatadogTranslator::new(datadog).translate()?;
    saluki_only.seed(&mut config);
    Ok(config)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use agent_data_plane_config::domains::dogstatsd::OriginTagCardinality;
    use datadog_agent_config::DatadogConfiguration;
    use serde_json::json;

    use super::{translate, SalukiOnly};

    #[test]
    fn translate_small_map_through_witness_and_seed() {
        // A small raw Datadog source map exercising a scalar conversion, an enum parse, a
        // seconds->Duration conversion, and the raw endpoint inputs.
        let datadog: DatadogConfiguration = serde_json::from_value(json!({
            "api_key": "abc",
            "dd_url": "https://custom.example.com",
            "dogstatsd_port": 9125,
            "dogstatsd_tag_cardinality": "high",
            "expected_tags_duration": 15.0,
            "telemetry": { "dogstatsd_origin": true },
        }))
        .expect("datadog source deserializes");

        // A small Saluki-only source setting one seeded field.
        let saluki_only: SalukiOnly = serde_json::from_value(json!({
            "dogstatsd": { "tcp_port": 8126 },
        }))
        .expect("saluki-only source deserializes");

        let config = translate(&datadog, &saluki_only).expect("translation succeeds");

        // Driven scalar conversion: i64 -> u16.
        assert_eq!(config.domains.dogstatsd.listeners.port, 9125);
        // Driven enum parse.
        assert_eq!(
            config.domains.dogstatsd.origin.tag_cardinality,
            OriginTagCardinality::High
        );
        // Driven seconds(f64) -> Duration.
        assert_eq!(config.shared.tags.expected_tags_duration, Duration::from_secs_f64(15.0));
        // Driven bool in a nested Datadog section.
        assert!(config.domains.dogstatsd.telemetry.origin_breakdown);
        // Raw endpoint inputs: carried through without resolution (see #1965).
        assert_eq!(config.shared.endpoints.api_key, "abc");
        assert_eq!(
            config.shared.endpoints.dd_url.as_deref(),
            Some("https://custom.example.com")
        );
        // Seeded Saluki-only field.
        assert_eq!(config.domains.dogstatsd.listeners.tcp_port, 8126);
    }
}
