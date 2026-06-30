//! The error types recorded by the configuration translator and surfaced by the witness driver.

use std::fmt;

/// An error produced while translating a witnessed Datadog configuration value into the
/// ADP-native model. Names the offending Datadog key, and optionally a human-readable note
/// and/or the underlying error object.
#[derive(Debug)]
pub struct TranslateError {
    key: String,
    context: Option<String>,
    error: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

impl TranslateError {
    /// Create a translation error wrapping an underlying error.
    ///
    /// Produces `error translating config key `{key}`: {error}`.
    pub fn new<S, E>(key: S, error: E) -> Self
    where
        S: Into<String>,
        E: Into<BoxError>,
    {
        Self {
            key: key.into(),
            context: None,
            error: Some(error.into()),
        }
    }

    /// Create a translation error wrapping an underlying error and adding a contextual message.
    ///
    /// Produces `error translating config key `{key}`, {context}: {error}`.
    pub fn new_with_context<S1, S2, E>(key: S1, context: S2, error: E) -> Self
    where
        S1: Into<String>,
        S2: Into<String>,
        E: Into<BoxError>,
    {
        Self {
            key: key.into(),
            context: Some(context.into()),
            error: Some(error.into()),
        }
    }

    /// Create a translation error when no underlying error object exists.
    ///
    /// Produces `error translating config key `{key}`, {message}`.
    pub fn new_with_message<S1, S2>(key: S1, message: S2) -> Self
    where
        S1: Into<String>,
        S2: Into<String>,
    {
        Self {
            key: key.into(),
            context: Some(message.into()),
            error: None,
        }
    }
}

impl fmt::Display for TranslateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "error translating config key `{}`", self.key)?;
        if let Some(context) = &self.context {
            write!(f, ", {context}")?;
        }
        if let Some(error) = &self.error {
            write!(f, ": {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for TranslateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.as_deref().map(|s| s as &(dyn std::error::Error + 'static))
    }
}

/// Every error [`drive`][crate::drive] recorded, non-empty by construction.
///
/// A collection of failures has no single `source`. Only [`drive`][crate::drive] constructs it,
/// and only when at least one key failed.
#[derive(Debug)]
pub struct TranslateErrors(Vec<TranslateError>);

impl TranslateErrors {
    /// Wraps the recorded errors. Callers must pass a non-empty vec.
    pub(crate) fn new(errors: Vec<TranslateError>) -> Self {
        debug_assert!(!errors.is_empty(), "TranslateErrors must be non-empty");
        Self(errors)
    }
}

impl fmt::Display for TranslateErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} config translation error(s):", self.0.len())?;
        for error in &self.0 {
            write!(f, "\n  - {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for TranslateErrors {}
