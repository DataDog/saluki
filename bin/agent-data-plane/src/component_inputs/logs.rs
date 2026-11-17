//! Inputs of the logs pipeline.

use agent_data_plane_config::domains::logs::{self, ProcessingRule};
use saluki_components::transforms::{LogsProcessingRule, LogsSourceMatch};

/// Builds the logs processing rules, in their configured order.
pub(crate) fn logs_processing_rules(logs: &logs::Domain) -> Vec<LogsProcessingRule> {
    logs.processing_rules
        .iter()
        .map(|rule| match rule.clone() {
            ProcessingRule::ExcludeAtMatch { name, pattern } => LogsProcessingRule::ExcludeAtMatch { name, pattern },
            ProcessingRule::IncludeAtMatch { name, pattern } => LogsProcessingRule::IncludeAtMatch { name, pattern },
            ProcessingRule::MaskSequences {
                name,
                pattern,
                replace_placeholder,
            } => LogsProcessingRule::MaskSequences {
                name,
                pattern,
                replace_placeholder,
            },
            ProcessingRule::ExcludeTruncated { name } => LogsProcessingRule::ExcludeTruncated { name },
            ProcessingRule::RemapSource { name, matching } => LogsProcessingRule::RemapSource {
                name,
                matching: matching
                    .into_iter()
                    .map(|entry| LogsSourceMatch {
                        attribute: entry.attribute,
                        value: entry.value,
                        new_source: entry.new_source,
                    })
                    .collect(),
            },
            ProcessingRule::MultiLine { name, pattern } => LogsProcessingRule::MultiLine { name, pattern },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use agent_data_plane_config::domains::logs::SourceMatch;

    use super::*;

    #[test]
    fn logs_processing_rules_copies_every_rule_in_order() {
        let domain = logs::Domain {
            processing_rules: vec![
                ProcessingRule::ExcludeAtMatch {
                    name: "exclude-name".into(),
                    pattern: "exclude-pattern".into(),
                },
                ProcessingRule::IncludeAtMatch {
                    name: "include-name".into(),
                    pattern: "include-pattern".into(),
                },
                ProcessingRule::MaskSequences {
                    name: "mask-name".into(),
                    pattern: "mask-pattern".into(),
                    replace_placeholder: "mask-placeholder".into(),
                },
                ProcessingRule::ExcludeTruncated {
                    name: "truncated-name".into(),
                },
                ProcessingRule::RemapSource {
                    name: "remap-name".into(),
                    matching: vec![SourceMatch {
                        attribute: "remap-attribute".into(),
                        value: "remap-value".into(),
                        new_source: "remap-source".into(),
                    }],
                },
                ProcessingRule::MultiLine {
                    name: "multi-line-name".into(),
                    pattern: "multi-line-pattern".into(),
                },
            ],
        };

        let rules = logs_processing_rules(&domain);

        assert_eq!(
            rules,
            vec![
                LogsProcessingRule::ExcludeAtMatch {
                    name: "exclude-name".into(),
                    pattern: "exclude-pattern".into(),
                },
                LogsProcessingRule::IncludeAtMatch {
                    name: "include-name".into(),
                    pattern: "include-pattern".into(),
                },
                LogsProcessingRule::MaskSequences {
                    name: "mask-name".into(),
                    pattern: "mask-pattern".into(),
                    replace_placeholder: "mask-placeholder".into(),
                },
                LogsProcessingRule::ExcludeTruncated {
                    name: "truncated-name".into(),
                },
                LogsProcessingRule::RemapSource {
                    name: "remap-name".into(),
                    matching: vec![LogsSourceMatch {
                        attribute: "remap-attribute".into(),
                        value: "remap-value".into(),
                        new_source: "remap-source".into(),
                    }],
                },
                LogsProcessingRule::MultiLine {
                    name: "multi-line-name".into(),
                    pattern: "multi-line-pattern".into(),
                },
            ]
        );
    }
}
