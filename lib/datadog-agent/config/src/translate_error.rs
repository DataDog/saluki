//! The error type recorded by the configuration translator and surfaced by the witness driver.

use snafu::Snafu;

/// An error produced while translating a witnessed Datadog configuration value into the ADP-native
/// model.
///
/// The translator records a `TranslateError` when a value cannot be mapped to its native
/// destination. The generated [`drive`][crate::drive] surfaces the first such error after consuming
/// every key. Both variants name the offending Datadog key.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum TranslateError {
    /// A witnessed value could not be deserialized into its model shape.
    #[snafu(display("failed to translate config key `{key}`: {source}"))]
    Serde {
        /// The dotted Datadog key the error concerns.
        key: String,
        /// The underlying deserialization error.
        source: serde_json::Error,
    },

    /// A witnessed value was well-formed but could not be converted to its model type (for example,
    /// an unrecognized enum string or a byte size that cannot be parsed).
    #[snafu(display("failed to translate config key `{key}`: {reason}"))]
    Translator {
        /// The dotted Datadog key the error concerns.
        key: String,
        /// A human-readable description of what went wrong.
        reason: String,
    },
}
