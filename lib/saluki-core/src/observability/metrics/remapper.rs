//! Remapper rules for translating source metrics into a different name and tag shape.
//!
//! A [`RemapperRule`] declares how to match a source metric (by name, optionally with a required
//! tag set) and how the matched metric should be rewritten: a new name, a set of tags copied
//! and/or renamed from the source, and an optional set of additional fixed tags. Rules also carry
//! optional help text that the renderer emits in the Prometheus `# HELP` header.

use saluki_context::{tags::TagSet, Context};
use stringtheory::MetaString;

/// A metric remapping rule.
///
/// Rules define the basic matching behavior (metric name, and optionally tags) as well as how to
/// remap the new copy of the metric. This can include copying tags as-is from the source metric,
/// copying specific tags over with a new name, and adding an additional fixed set of tags to the
/// new metric.
#[derive(Clone)]
pub struct RemapperRule {
    existing_name: &'static str,
    existing_tags: &'static [&'static str],
    new_name: &'static str,
    remapped_tags: Vec<(&'static str, &'static str)>,
    additional_tags: Vec<MetaString>,
    continue_matching: bool,
    help_text: Option<&'static str>,
}

impl RemapperRule {
    /// Creates a new `RemapperRule` that matches a source metric by name only.
    pub fn by_name(existing_name: &'static str, new_name: &'static str) -> Self {
        Self {
            existing_name,
            existing_tags: &[],
            new_name,
            remapped_tags: Vec::new(),
            additional_tags: Vec::new(),
            continue_matching: false,
            help_text: None,
        }
    }

    /// Creates a new `RemapperRule` that matches a source metric by name and tags.
    pub fn by_name_and_tags(
        existing_name: &'static str, existing_tags: &'static [&'static str], new_name: &'static str,
    ) -> Self {
        Self {
            existing_name,
            existing_tags,
            new_name,
            remapped_tags: Vec::new(),
            additional_tags: Vec::new(),
            continue_matching: false,
            help_text: None,
        }
    }

    /// Adds a set of tags to remap from the source metric by changing their name.
    ///
    /// Remapped tags must be given in the form of `(source_tag, destination_tag)`. If a tag by the name `source_tag` is
    /// found in the source metric, it's copied to the remapped metric with a name of `destination_tag`.
    ///
    /// This method is additive, so it can be called multiple times to add more remapped tags. Tag remapping is
    /// order-dependent, so if a tag is configured to be remapped, or copied, multiple times, then the first match will
    /// take precedence.
    pub fn with_remapped_tags<I>(mut self, remapped_tags: I) -> Self
    where
        I: IntoIterator<Item = (&'static str, &'static str)>,
    {
        self.remapped_tags.extend(remapped_tags);
        self
    }

    /// Adds a set of tags to remap from the source metric without changing their name.
    ///
    /// Remapped tags must be given in the form of `source_tag`. If a tag by the name `source_tag` is found in the
    /// source metric, it's copied to the remapped metric with the same name.
    ///
    /// This method is additive, so it can be called multiple times to add more original tags. Tag remapping is
    /// order-dependent, so if a tag is configured to be copied, or remapped, multiple times, then the first match will
    /// take precedence.
    pub fn with_original_tags<I>(mut self, original_tags: I) -> Self
    where
        I: IntoIterator<Item = &'static str>,
    {
        self.remapped_tags
            .extend(original_tags.into_iter().map(|tag| (tag, tag)));
        self
    }

    /// Adds a fixed set of tags to add to the remapped metric.
    ///
    /// Additional tags are given in the form of `tag`, which can be any valid tag value: bare or key/value.
    ///
    /// This method is additive, so it can be called multiple times to add more additional tags.
    pub fn with_additional_tags<I>(mut self, additional_tags: I) -> Self
    where
        I: IntoIterator<Item = &'static str>,
    {
        self.additional_tags
            .extend(additional_tags.into_iter().map(MetaString::from_static));
        self
    }

    /// Allows later rules to also match the same source metric.
    pub fn with_continued_matching(mut self) -> Self {
        self.continue_matching = true;
        self
    }

    /// Sets the Prometheus `# HELP` text that should be emitted alongside the remapped metric.
    ///
    /// Some downstream collectors (notably the Datadog Agent's OpenMetrics check) require a
    /// specific help text on overlapped metric names. Setting this here keeps the help text next
    /// to the rule that produces the name.
    pub fn with_help_text(mut self, help_text: &'static str) -> Self {
        self.help_text = Some(help_text);
        self
    }

    /// Returns `true` if matching should continue after this rule matches.
    pub const fn should_continue_matching(&self) -> bool {
        self.continue_matching
    }

    /// Returns the remapped metric name produced by this rule.
    pub const fn remapped_name(&self) -> &'static str {
        self.new_name
    }

    /// Returns the help text associated with this rule, if any.
    pub const fn help_text(&self) -> Option<&'static str> {
        self.help_text
    }

    /// Attempts to match the given context against this rule.
    ///
    /// If the rule matches, returns a [`RemappedMetric`] containing the new metric name and tags.
    pub fn try_match_no_context(&self, context: &Context) -> Option<RemappedMetric> {
        if context.name() != self.existing_name {
            return None;
        }

        let metric_tags = context.tags();
        for existing_tag in self.existing_tags {
            if !metric_tags.has_tag(existing_tag) {
                return None;
            }
        }

        let tags = self.build_remapped_tags(metric_tags);
        Some(RemappedMetric {
            name: self.new_name,
            tags,
        })
    }

    /// Builds the remapped tags for a matched metric.
    fn build_remapped_tags(&self, metric_tags: &TagSet) -> Vec<MetaString> {
        let mut new_tags = vec![];

        for (original_tag_name, new_tag_name) in &self.remapped_tags {
            if let Some(tag) = metric_tags.get_single_tag(original_tag_name) {
                if original_tag_name == new_tag_name {
                    // Just clone the tag since the name isn't changing.
                    new_tags.push(tag.clone().into_inner());
                } else {
                    // Build our new tag if this one has a value.
                    match tag.value() {
                        Some(value) => {
                            new_tags.push(MetaString::from(format!("{}:{}", new_tag_name, value)));
                        }
                        None => {
                            new_tags.push(MetaString::from(*new_tag_name));
                        }
                    }
                }
            }
        }

        for additional_tag in &self.additional_tags {
            new_tags.push(additional_tag.clone());
        }

        new_tags
    }
}

/// A metric that has been remapped by a [`RemapperRule`].
pub struct RemappedMetric {
    /// The remapped metric name.
    pub name: &'static str,

    /// The remapped tags in `key:value` (or bare) format.
    pub tags: Vec<MetaString>,
}

#[cfg(test)]
mod tests {
    use saluki_context::Context;

    use super::*;

    struct MatchCase {
        description: &'static str,
        rule: RemapperRule,
        context: Context,
        expected_name: Option<&'static str>,
    }

    #[test]
    fn matches_by_name_and_required_tags() {
        let cases = [
            MatchCase {
                description: "by_name matches on the metric name alone",
                rule: RemapperRule::by_name("src.metric", "dst.metric"),
                context: Context::from_static_parts("src.metric", &["env:prod"]),
                expected_name: Some("dst.metric"),
            },
            MatchCase {
                description: "by_name rejects a different metric name",
                rule: RemapperRule::by_name("src.metric", "dst.metric"),
                context: Context::from_static_parts("other.metric", &["env:prod"]),
                expected_name: None,
            },
            MatchCase {
                description: "by_name_and_tags matches when every required tag is present",
                rule: RemapperRule::by_name_and_tags("src.metric", &["env:prod", "role:api"], "dst.metric"),
                context: Context::from_static_parts("src.metric", &["env:prod", "role:api", "extra:1"]),
                expected_name: Some("dst.metric"),
            },
            MatchCase {
                description: "by_name_and_tags rejects when a required tag has a different value",
                rule: RemapperRule::by_name_and_tags("src.metric", &["env:prod"], "dst.metric"),
                context: Context::from_static_parts("src.metric", &["env:dev"]),
                expected_name: None,
            },
            MatchCase {
                description: "by_name_and_tags rejects when only one of several required tags is present",
                rule: RemapperRule::by_name_and_tags("src.metric", &["env:prod", "role:api"], "dst.metric"),
                context: Context::from_static_parts("src.metric", &["env:prod"]),
                expected_name: None,
            },
        ];

        for case in cases {
            let actual = case
                .rule
                .try_match_no_context(&case.context)
                .map(|remapped| remapped.name);
            assert_eq!(actual, case.expected_name, "case: {}", case.description);
        }
    }

    fn remapped_tags(rule: &RemapperRule, context: &Context) -> Vec<String> {
        rule.try_match_no_context(context)
            .expect("rule should match")
            .tags
            .iter()
            .map(|tag| tag.as_ref().to_string())
            .collect()
    }

    #[test]
    fn copies_original_tags_and_renames_remapped_tags() {
        // `with_original_tags` copies a tag unchanged; `with_remapped_tags` copies its value under a new
        // key. Output order follows the rule's configured order, not the source metric's tag order.
        let rule = RemapperRule::by_name("src.metric", "dst.metric")
            .with_original_tags(["region"])
            .with_remapped_tags([("host", "hostname")]);
        let context = Context::from_static_parts("src.metric", &["region:us-east-1", "host:web01"]);

        assert_eq!(remapped_tags(&rule, &context), ["region:us-east-1", "hostname:web01"]);
    }

    #[test]
    fn appends_additional_fixed_tags_after_copied_tags() {
        let rule = RemapperRule::by_name("src.metric", "dst.metric")
            .with_original_tags(["region"])
            .with_additional_tags(["source:internal"]);
        let context = Context::from_static_parts("src.metric", &["region:us-east-1"]);

        assert_eq!(remapped_tags(&rule, &context), ["region:us-east-1", "source:internal"]);
    }

    #[test]
    fn skips_remapped_tags_absent_from_the_source_metric() {
        // The `host` tag isn't present on the source metric, so it contributes no remapped tag.
        let rule = RemapperRule::by_name("src.metric", "dst.metric").with_remapped_tags([("host", "hostname")]);
        let context = Context::from_static_parts("src.metric", &["region:us-east-1"]);

        assert!(remapped_tags(&rule, &context).is_empty());
    }

    #[test]
    fn exposes_continue_matching_and_help_text_accessors() {
        let rule = RemapperRule::by_name("src.metric", "dst.metric");
        assert_eq!(rule.remapped_name(), "dst.metric");
        assert!(!rule.should_continue_matching());
        assert_eq!(rule.help_text(), None);

        let rule = rule.with_continued_matching().with_help_text("some help text");
        assert!(rule.should_continue_matching());
        assert_eq!(rule.help_text(), Some("some help text"));
    }
}
