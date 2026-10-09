use std::{error::Error, fmt};

/// Whether a [`TagFilterRule`] keeps or removes the tags it lists.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TagFilterAction {
    /// Keep only the listed tags. An empty list removes every tag.
    Include,

    /// Remove the listed tags. An empty list keeps every tag.
    Exclude,
}

impl TagFilterAction {
    /// Returns the action's name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Include => "include",
            Self::Exclude => "exclude",
        }
    }
}

/// A tag projection rule: which tags of the metrics with one exact raw name are kept.
///
/// Tags are matched by key; a bare tag's key is the whole tag. A rule applies to the instrumented tags and to the
/// origin tags alike. Rules for the same name and action combine; when rules for the same name disagree on the action,
/// `exclude` wins.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TagFilterRule {
    /// The exact raw metric name the rule applies to. A rule with an empty name is ignored.
    pub metric_name: String,

    /// Whether the listed tags are kept or removed.
    pub action: TagFilterAction,

    /// The tag keys the action applies to.
    pub tags: Vec<String>,
}

/// What happens to a tag whose value is not allowed by a [`TagValueRule`].
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum TagValueMismatch {
    /// The tag is removed.
    Remove,

    /// The tag's value is replaced with the given value.
    Replace(String),
}

/// A tag value rule: which values one tag of the metrics whose raw names start with a prefix may have.
///
/// Only tags with a value are affected, including tags with an empty value; bare tags are kept. A tag whose value is
/// in `values` is kept unchanged, and any other value is handled by `on_miss`. Matching is exact and case-sensitive,
/// including whitespace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TagValueRule {
    /// The raw metric name prefix the rule applies to. It must not be empty.
    pub metric_prefix: String,

    /// The tag key whose values are restricted. It must not be empty or contain `:`.
    pub tag_name: String,

    /// The allowed values. An empty list allows no value.
    pub values: Vec<String>,

    /// What happens to a tag whose value is not allowed.
    pub on_miss: TagValueMismatch,
}

/// Why a set of [`TagValueRule`]s cannot be applied unambiguously.
///
/// Rules are numbered from 1, in the order given. The variants carry what an error message needs, so that a host's
/// configuration layer can describe the problem in terms of its own configuration keys; [`Display`][fmt::Display]
/// describes it in terms of the rules.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InvalidTagValueRules {
    /// A rule's metric prefix is empty.
    EmptyMetricPrefix {
        /// The rule's number.
        rule: usize,
    },

    /// A rule's tag name is empty.
    EmptyTagName {
        /// The rule's number.
        rule: usize,

        /// The rule's metric prefix.
        metric_prefix: String,
    },

    /// A rule's tag name contains `:`, so it names a tag and value rather than a tag key.
    TagNameContainsColon {
        /// The rule's number.
        rule: usize,

        /// The rule's tag name.
        tag_name: String,
    },

    /// Two rules restrict the same tag for the same metric prefix.
    DuplicateRule {
        /// The metric prefix.
        metric_prefix: String,

        /// The tag name.
        tag_name: String,
    },

    /// Two distinct metric prefixes overlap: the longer one starts with the shorter one, so a metric could match both.
    OverlappingPrefixes {
        /// The shorter prefix.
        shorter: String,

        /// The longer prefix, which starts with the shorter one.
        longer: String,
    },
}

impl fmt::Display for InvalidTagValueRules {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyMetricPrefix { rule } => write!(
                f,
                "tag value rule {} has an empty metric prefix; give each rule a non-empty metric name prefix",
                rule
            ),
            Self::EmptyTagName { rule, metric_prefix } => write!(
                f,
                "tag value rule {} for prefix '{}' has an empty tag name; give each rule a non-empty tag name",
                rule, metric_prefix
            ),
            Self::TagNameContainsColon { rule, tag_name } => write!(
                f,
                "tag value rule {} names the tag '{}', which contains ':'; name only the tag key, without a colon or \
                 value",
                rule, tag_name
            ),
            Self::DuplicateRule {
                metric_prefix,
                tag_name,
            } => write!(
                f,
                "metric prefix '{}' has more than one tag value rule for tag '{}'; give each prefix and tag pair one \
                 rule",
                metric_prefix, tag_name
            ),
            Self::OverlappingPrefixes { shorter, longer } => write!(
                f,
                "the tag value rule prefixes '{}' and '{}' overlap; use distinct prefixes where neither starts with \
                 the other",
                shorter, longer
            ),
        }
    }
}

impl Error for InvalidTagValueRules {}

/// Checks that a set of tag value rules can be applied unambiguously.
///
/// This is [`validate_tag_value_rule_keys`] over each rule's metric prefix and tag name.
///
/// # Errors
///
/// Returns an error describing the first problem found; see [`validate_tag_value_rule_keys`].
pub fn validate_tag_value_rules(rules: &[TagValueRule]) -> Result<(), InvalidTagValueRules> {
    validate_tag_value_rule_keys(
        rules
            .iter()
            .map(|rule| (rule.metric_prefix.as_str(), rule.tag_name.as_str())),
    )
}

/// Checks that tag value rules, given by their metric prefix and tag name in order, can be applied unambiguously.
///
/// This is the one implementation of the rule: tag projection relies on it to find at most one rule per metric and
/// tag, and a host's configuration layer applies it to its own tag value allowlists, so that both accept and reject
/// the same rules.
///
/// # Errors
///
/// Returns an error describing the first problem found. Every rule is first checked on its own, in order: an empty
/// prefix, then an empty tag name, then a tag name containing `:`. Then, among the rules sorted by prefix and tag name,
/// the first two adjacent rules with the same prefix and tag, or with distinct prefixes where one starts with the
/// other, are reported. Rules with the same prefix may restrict different tags.
pub fn validate_tag_value_rule_keys<'a, I>(keys: I) -> Result<(), InvalidTagValueRules>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let keys = keys.into_iter().collect::<Vec<_>>();
    for (index, (metric_prefix, tag_name)) in keys.iter().enumerate() {
        let rule = index + 1;
        if metric_prefix.is_empty() {
            return Err(InvalidTagValueRules::EmptyMetricPrefix { rule });
        }
        if tag_name.is_empty() {
            return Err(InvalidTagValueRules::EmptyTagName {
                rule,
                metric_prefix: metric_prefix.to_string(),
            });
        }
        if tag_name.contains(':') {
            return Err(InvalidTagValueRules::TagNameContainsColon {
                rule,
                tag_name: tag_name.to_string(),
            });
        }
    }

    let mut sorted = keys;
    sorted.sort_unstable();

    // After sorting by prefix and then tag, duplicate prefix and tag pairs are adjacent, and any distinct prefix that
    // extends another follows the whole group of the shorter prefix, so checking adjacent pairs finds both problems.
    for pair in sorted.windows(2) {
        let ((left_prefix, left_tag), (right_prefix, right_tag)) = (pair[0], pair[1]);
        if left_prefix == right_prefix {
            if left_tag == right_tag {
                return Err(InvalidTagValueRules::DuplicateRule {
                    metric_prefix: left_prefix.to_string(),
                    tag_name: left_tag.to_string(),
                });
            }
        } else if right_prefix.starts_with(left_prefix) {
            return Err(InvalidTagValueRules::OverlappingPrefixes {
                shorter: left_prefix.to_string(),
                longer: right_prefix.to_string(),
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(prefix: &str, tag: &str) -> TagValueRule {
        TagValueRule {
            metric_prefix: prefix.to_string(),
            tag_name: tag.to_string(),
            values: vec!["prod".to_string()],
            on_miss: TagValueMismatch::Remove,
        }
    }

    #[test]
    fn valid_rules_may_share_a_prefix_for_different_tags() {
        let rules = [
            rule("checkout.", "env"),
            rule("checkout.", "region"),
            rule("catalog.", "env"),
        ];
        assert_eq!(validate_tag_value_rules(&rules), Ok(()));
        assert_eq!(validate_tag_value_rules(&[]), Ok(()));
    }

    #[test]
    fn invalid_rules_are_rejected_with_the_reason() {
        let owned = |value: &str| value.to_string();
        // (rules, expected error, expected message fragment)
        let cases: Vec<(Vec<TagValueRule>, InvalidTagValueRules, &str)> = vec![
            (
                vec![rule("", "env")],
                InvalidTagValueRules::EmptyMetricPrefix { rule: 1 },
                "rule 1 has an empty metric prefix",
            ),
            (
                vec![rule("a.", "env"), rule("b.", "")],
                InvalidTagValueRules::EmptyTagName {
                    rule: 2,
                    metric_prefix: owned("b."),
                },
                "rule 2 for prefix 'b.' has an empty tag name",
            ),
            (
                vec![rule("a.", "env:prod")],
                InvalidTagValueRules::TagNameContainsColon {
                    rule: 1,
                    tag_name: owned("env:prod"),
                },
                "names the tag 'env:prod', which contains ':'",
            ),
            // Every rule is checked on its own before rules are compared, so a later empty tag name is reported
            // rather than an earlier overlap.
            (
                vec![rule("a", "env"), rule("ab", "env"), rule("c", "")],
                InvalidTagValueRules::EmptyTagName {
                    rule: 3,
                    metric_prefix: owned("c"),
                },
                "rule 3 for prefix 'c' has an empty tag name",
            ),
            (
                vec![rule("a.", "env"), rule("b.", "env"), rule("a.", "env")],
                InvalidTagValueRules::DuplicateRule {
                    metric_prefix: owned("a."),
                    tag_name: owned("env"),
                },
                "metric prefix 'a.' has more than one tag value rule for tag 'env'",
            ),
            (
                vec![rule("checkout.api.", "region"), rule("checkout.", "env")],
                InvalidTagValueRules::OverlappingPrefixes {
                    shorter: owned("checkout."),
                    longer: owned("checkout.api."),
                },
                "prefixes 'checkout.' and 'checkout.api.' overlap",
            ),
            // Overlap is detected even when a different tag of the shorter prefix sorts in between.
            (
                vec![rule("a", "z"), rule("a", "b"), rule("ab", "c")],
                InvalidTagValueRules::OverlappingPrefixes {
                    shorter: owned("a"),
                    longer: owned("ab"),
                },
                "prefixes 'a' and 'ab' overlap",
            ),
        ];
        for (rules, expected, fragment) in cases {
            let error = validate_tag_value_rules(&rules).unwrap_err();
            assert_eq!(error, expected, "{:?}", rules);
            assert!(error.to_string().contains(fragment), "{:?}: {}", rules, error);
        }
    }

    #[test]
    fn rule_keys_are_validated_without_rules() {
        assert_eq!(
            validate_tag_value_rule_keys([("checkout.", "env"), ("catalog.", "env")]),
            Ok(())
        );
        assert_eq!(
            validate_tag_value_rule_keys([("checkout.", "env"), ("checkout.", "env")]),
            Err(InvalidTagValueRules::DuplicateRule {
                metric_prefix: "checkout.".to_string(),
                tag_name: "env".to_string(),
            })
        );
    }
}
