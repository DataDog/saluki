//! Abstract diagnostic events.

/// Structured detail describing the nature of a [`DiagnosticEvent`].
///
/// This enum is intentionally minimal for now; additional variants will be added over time as more diagnostic
/// conditions are modeled. It is marked `#[non_exhaustive]` so that new variants can be added without breaking
/// downstream matches.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DiagnosticDetails {
    /// The configured API key was rejected as invalid.
    InvalidApiKey,
}

/// An abstract, point-in-time diagnostic event emitted by a subsystem.
///
/// An event pairs a human-readable message with a structured [`DiagnosticDetails`] value describing what occurred. The
/// emitting subsystem is conveyed by the dataspace identifier the event is sent under, so it is not duplicated on the
/// event itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticEvent {
    message: String,
    details: DiagnosticDetails,
}

impl DiagnosticEvent {
    /// Creates a new diagnostic event with the given message and structured detail.
    pub fn new(message: impl Into<String>, details: DiagnosticDetails) -> Self {
        Self {
            message: message.into(),
            details,
        }
    }

    /// Returns the human-readable message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the structured detail.
    pub fn details(&self) -> &DiagnosticDetails {
        &self.details
    }
}
