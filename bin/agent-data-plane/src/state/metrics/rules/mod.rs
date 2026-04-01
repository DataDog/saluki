use saluki_context::{tags::TagSet, Context};
use stringtheory::MetaString;

mod aggregation;
mod dogstatsd;
mod transaction;

/// Returns the list of remapper rules relevant to metrics we send to the Datadog Agent via Remote Agent Registry (RAR).
///
/// These metrics are used specifically to ensure continuity of metrics that are emitted via COAT, regardless of whether
/// or not ADP is enabled.
pub fn get_datadog_agent_remappings() -> Vec<RemapperRule> {
    let mut rules = Vec::new();
    rules.extend(self::dogstatsd::get_dogstatsd_remappings());
    rules.extend(self::aggregation::get_aggregation_remappings());
    rules.extend(self::transaction::get_transaction_remappings());
    rules
}

/// A metric remapping rule.
///
/// Rules define the basic matching behavior -- metric name, and optionally tags -- as well as how to remap the new copy
/// of the metric. This can include copying tags as-is from the source metric, copying specific tags over with a new
/// name, and adding an additional fixed set of tags to the new metric.
pub struct RemapperRule {
    existing_name: &'static str,
    existing_tags: &'static [&'static str],
    new_name: &'static str,
    remapped_tags: Vec<(&'static str, &'static str)>,
    additional_tags: Vec<MetaString>,
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
        }
    }

    /// Adds a set of tags to remap from the source metric by changing their name.
    ///
    /// Remapped tags must be given in the form of `(source_tag, destination_tag)`. If a tag by the name `source_tag` is
    /// found in the source metric, it is copied to the remapped metric with a name of `destination_tag`.
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
    /// source metric, it is copied to the remapped metric with the same name.
    ///
    /// This method is additive, so it can be called multiple times to add more original tags. Tag remapping is
    /// order-dependent, so if a tag is configured to be copied, or remapped, multiple times, then the first match will
    /// take precedence.
    fn with_original_tags<I>(mut self, original_tags: I) -> Self
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
    /// The remapped metric name (Agent-compatible).
    pub name: &'static str,

    /// The remapped tags in `key:value` format.
    pub tags: Vec<MetaString>,
}
