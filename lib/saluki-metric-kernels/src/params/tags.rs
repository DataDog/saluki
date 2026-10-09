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

/// An error describing why a set of [`TagValueRule`]s cannot be used.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidTagValueRules(String);

impl InvalidTagValueRules {
    /// Returns the description of the problem.
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InvalidTagValueRules {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for InvalidTagValueRules {}

/// Checks that a set of tag value rules can be applied unambiguously.
///
/// The host's configuration layer validates its own tag value allowlists by the same rules, with messages in terms of
/// its configuration keys. Until the tag projection kernel is extracted and one of the two delegates to the other, a
/// differential test in the host checks that both accept and reject the same rule lists.
///
/// # Errors
///
/// Returns an error describing the first problem found: a rule with an empty prefix or tag name, a tag name containing
/// `:`, two rules for the same prefix and tag, or two distinct prefixes where one starts with the other. Rules with the
/// same prefix may restrict different tags.
pub fn validate_tag_value_rules(rules: &[TagValueRule]) -> Result<(), InvalidTagValueRules> {
    for (index, rule) in rules.iter().enumerate() {
        let number = index + 1;
        if rule.metric_prefix.is_empty() {
            return Err(InvalidTagValueRules(format!(
                "tag value rule {} has an empty metric prefix; give each rule a non-empty metric name prefix",
                number
            )));
        }
        if rule.tag_name.is_empty() {
            return Err(InvalidTagValueRules(format!(
                "tag value rule {} for prefix '{}' has an empty tag name; give each rule a non-empty tag name",
                number, rule.metric_prefix
            )));
        }
        if rule.tag_name.contains(':') {
            return Err(InvalidTagValueRules(format!(
                "tag value rule {} names the tag '{}', which contains ':'; name only the tag key, without a colon or \
                 value",
                number, rule.tag_name
            )));
        }
    }

    let mut sorted = rules.iter().collect::<Vec<_>>();
    sorted.sort_unstable_by(|left, right| {
        left.metric_prefix
            .cmp(&right.metric_prefix)
            .then_with(|| left.tag_name.cmp(&right.tag_name))
    });

    // After sorting by prefix and then tag, duplicate prefix and tag pairs are adjacent, and any distinct prefix that
    // extends another follows the whole group of the shorter prefix, so checking adjacent pairs finds both problems.
    for pair in sorted.windows(2) {
        let (left, right) = (pair[0], pair[1]);
        if left.metric_prefix == right.metric_prefix {
            if left.tag_name == right.tag_name {
                return Err(InvalidTagValueRules(format!(
                    "metric prefix '{}' has more than one tag value rule for tag '{}'; give each prefix and tag pair \
                     one rule",
                    left.metric_prefix, left.tag_name
                )));
            }
        } else if right.metric_prefix.starts_with(&left.metric_prefix) {
            return Err(InvalidTagValueRules(format!(
                "the tag value rule prefixes '{}' and '{}' overlap; use distinct prefixes where neither starts with \
                 the other",
                left.metric_prefix, right.metric_prefix
            )));
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
        // (rules, expected message fragment)
        let cases: Vec<(Vec<TagValueRule>, &str)> = vec![
            (vec![rule("", "env")], "rule 1 has an empty metric prefix"),
            (
                vec![rule("a.", "env"), rule("b.", "")],
                "rule 2 for prefix 'b.' has an empty tag name",
            ),
            (
                vec![rule("a.", "env:prod")],
                "names the tag 'env:prod', which contains ':'",
            ),
            (
                vec![rule("a.", "env"), rule("b.", "env"), rule("a.", "env")],
                "metric prefix 'a.' has more than one tag value rule for tag 'env'",
            ),
            (
                vec![rule("checkout.", "env"), rule("checkout.api.", "region")],
                "prefixes 'checkout.' and 'checkout.api.' overlap",
            ),
            // Overlap is detected even when a different tag of the shorter prefix sorts in between.
            (
                vec![rule("a", "z"), rule("a", "b"), rule("ab", "c")],
                "prefixes 'a' and 'ab' overlap",
            ),
        ];
        for (rules, fragment) in cases {
            let error = validate_tag_value_rules(&rules).unwrap_err();
            assert!(error.message().contains(fragment), "{:?}: {}", rules, error);
        }
    }
}
