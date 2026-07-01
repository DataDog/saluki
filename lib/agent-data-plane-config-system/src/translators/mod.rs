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
    /// Translates the source into a [`SalukiConfiguration`].
    ///
    /// # Errors
    ///
    /// Returns the first [`TranslateError`] recorded while translating a source value (for example,
    /// an enum or byte-size string that cannot be parsed).
    fn translate(self) -> Result<SalukiConfiguration, TranslateError>;
}
