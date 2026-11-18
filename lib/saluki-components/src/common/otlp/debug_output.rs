//! Debug logging of the OTLP data ADP receives.
//!
//! Operators turn this on to confirm that data is arriving and to see what it looks like. It's a view into what was
//! received and doesn't change what ADP processes or sends.

/// How much detail is logged for each received batch of OTLP data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OtlpDebugVerbosity {
    /// Log nothing.
    #[default]
    None,

    /// Log one summary line per batch, with the number of items received.
    Basic,

    /// Log the summary line, then about one line for each item received.
    Normal,

    /// Log the summary line, then every field of every item received.
    Detailed,
}
