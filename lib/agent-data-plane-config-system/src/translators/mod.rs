//! Configuration translators: witnesses that consume a typed configuration source and produce a
//! [`SalukiConfiguration`][agent_data_plane_config::SalukiConfiguration].
//!
//! [`DatadogTranslator`] is the translator for the Datadog source; the generated `drive` feeds it
//! one supported key at a time.

mod datadog_translator;

// TODO: remove the allow once ConfigurationSystem imports this re-export.
#[allow(unused_imports)]
pub(crate) use datadog_translator::DatadogTranslator;
