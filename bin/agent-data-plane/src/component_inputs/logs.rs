//! Inputs of the logs pipeline.

use agent_data_plane_config::domains::logs::ProcessingRule;
use saluki_components::transforms::LogsProcessingRule;

/// Builds the validated logs processing rules that apply to check and OTLP logs, in their configured order.
///
/// The other rule types are left out, since they don't apply to these logs in the Agent either. `exclude_truncated`
/// applies to truncated logs and `remap_source` to structured logs, and neither kind reaches this pipeline.
/// `multi_line` applies to the aggregation of log lines, which happens before the processing rules.
pub(crate) fn logs_processing_rules(rules: &[ProcessingRule]) -> Vec<LogsProcessingRule> {
    rules
        .iter()
        .filter_map(|rule| match rule.clone() {
            ProcessingRule::ExcludeAtMatch { name, pattern } => {
                Some(LogsProcessingRule::ExcludeAtMatch { name, pattern })
            }
            ProcessingRule::IncludeAtMatch { name, pattern } => {
                Some(LogsProcessingRule::IncludeAtMatch { name, pattern })
            }
            ProcessingRule::MaskSequences {
                name,
                pattern,
                replace_placeholder,
            } => Some(LogsProcessingRule::MaskSequences {
                name,
                pattern,
                replace_placeholder,
            }),
            ProcessingRule::ExcludeTruncated { .. }
            | ProcessingRule::RemapSource { .. }
            | ProcessingRule::MultiLine { .. } => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use agent_data_plane_config::domains::logs::SourceMatch;

    use super::*;

    #[test]
    fn logs_processing_rules_copies_the_rules_that_apply_in_order() {
        let validated = vec![
            ProcessingRule::ExcludeAtMatch {
                name: "exclude-name".into(),
                pattern: "exclude-pattern".into(),
            },
            ProcessingRule::ExcludeTruncated {
                name: "truncated-name".into(),
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
        ];

        let rules = logs_processing_rules(&validated);

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
            ]
        );
    }
}
