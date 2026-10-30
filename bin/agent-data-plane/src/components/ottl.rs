//! Error handling shared by the OTTL processors.

/// How an OTTL processor handles an error evaluating a condition or statement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorMode {
    /// Log the error and continue.
    Ignore,

    /// Continue without logging the error.
    Silent,

    /// Stop processing the span; the filter processor drops it.
    Propagate,
}
