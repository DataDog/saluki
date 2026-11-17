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
    ExcludeAtMatch {
        /// Name of the rule.
        name: String,
        /// Regular expression matched against the content of each log.
        pattern: String,
    },

    /// Drops logs that don't match `pattern`.
    IncludeAtMatch {
        /// Name of the rule.
        name: String,
        /// Regular expression matched against the content of each log.
        pattern: String,
    },

    /// Replaces each match of `pattern` with `replace_placeholder`.
    MaskSequences {
        /// Name of the rule.
        name: String,
        /// Regular expression matched against the content of each log.
        pattern: String,
        /// Replacement for each match, where `$1` and `${name}` refer to capture groups.
        replace_placeholder: String,
    },

    /// Drops truncated logs.
    ExcludeTruncated {
        /// Name of the rule.
        name: String,
    },

    /// Changes the source of structured logs whose attribute has a given value.
    RemapSource {
        /// Name of the rule.
        name: String,
        /// Attribute values to match, and the source each one maps to.
        matching: Vec<SourceMatch>,
    },

    /// Aggregates multi-line logs.
    MultiLine {
        /// Name of the rule.
        name: String,
        /// Regular expression matching the start of a log.
        pattern: String,
    },
}

/// An attribute value that a `remap_source` rule maps to a new source.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SourceMatch {
    /// Path of the attribute.
    pub attribute: String,
    /// Value the attribute must have.
    pub value: String,
    /// Source given to a log that matches.
    pub new_source: String,
}
