//! Configuration translators: witnesses that consume a typed configuration source and produce a
//! [`SalukiConfiguration`].
//!
//! Each translator implements [`ConfigTranslator`]. [`DatadogTranslator`] is the translator for the
//! Datadog source; the generated `drive` feeds it one supported key at a time.

mod datadog_translator;

use agent_data_plane_config::SalukiConfiguration;
use datadog_agent_config::TranslateError;
pub(crate) use datadog_translator::DatadogTranslator;

/// Consumes a configuration source and produces a [`SalukiConfiguration`].
pub trait ConfigTranslator {
    /// Translates the source into a [`SalukiConfiguration`], returning any error recorded while
    /// converting an individual value.
    ///
    /// Every supported key is consumed regardless of individual conversion failures: a value that
    /// cannot be converted (for example, an enum or byte-size string that cannot be parsed) leaves
    /// its field at the model default and records an error. The returned configuration is therefore
    /// always complete, and the returned [`TranslateError`], when present, reports the first such
    /// failure.
    fn translate(self) -> (SalukiConfiguration, Option<TranslateError>);
}
