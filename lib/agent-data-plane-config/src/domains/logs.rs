//! Logs domain. Carries the configuration of the logs pipeline that check and OTLP logs share.

use serde::Serialize;

/// Resolved logs configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Domain {
    /// Processing rules applied, in order, to every log: `logs_config.processing_rules`.
    ///
    /// Empty unless the logs agent is enabled, since the Agent only reads the rules then.
    pub processing_rules: Vec<ProcessingRule>,
}

/// A logs processing rule.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProcessingRule {
    /// Drops logs that match `pattern`.
    ExcludeAtMatch { name: String, pattern: String },

    /// Drops logs that don't match `pattern`.
    IncludeAtMatch { name: String, pattern: String },

    /// Replaces each match of `pattern` with `replace_placeholder`.
    MaskSequences {
        name: String,
        pattern: String,
        replace_placeholder: String,
    },

    /// Drops truncated logs.
    ExcludeTruncated { name: String },

    /// Changes the source of structured logs whose attribute has a given value.
    RemapSource { name: String, matching: Vec<SourceMatch> },

    /// Aggregates multi-line logs.
    MultiLine { name: String, pattern: String },
}

/// An attribute value that a `remap_source` rule maps to a new source.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SourceMatch {
    pub attribute: String,
    pub value: String,
    pub new_source: String,
}
