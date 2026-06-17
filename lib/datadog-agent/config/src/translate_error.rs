//! The error type recorded by the configuration translator and surfaced by the witness driver.

use std::fmt;

/// An error produced while translating a witnessed Datadog configuration value into the ADP-native
/// model.
///
/// The downstream translator records a `TranslateError` when a value cannot be mapped to its native
/// destination (for example, a malformed nested structure or an out-of-range value). The generated
/// [`drive`][crate::drive] surfaces the first such error after consuming every key.
///
/// When the offending key is known, prefer [`TranslateError::for_key`] so the message can name it.
#[derive(Debug, Clone)]
pub struct TranslateError {
    /// The dotted Datadog key the error concerns, when one applies.
    key: Option<String>,
    /// A human-readable description of what went wrong.
    message: String,
}

impl TranslateError {
    /// Constructs a `TranslateError` not tied to a specific key.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            key: None,
            message: message.into(),
        }
    }

    /// Constructs a `TranslateError` attributed to a specific dotted key.
    pub fn for_key(key: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            key: Some(key.into()),
            message: message.into(),
        }
    }

    /// Returns the dotted key this error concerns, if any.
    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }

    /// Returns the human-readable error message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for TranslateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.key {
            Some(key) => write!(f, "failed to translate config key `{key}`: {}", self.message),
            None => write!(f, "config translation error: {}", self.message),
        }
    }
}

impl std::error::Error for TranslateError {}
