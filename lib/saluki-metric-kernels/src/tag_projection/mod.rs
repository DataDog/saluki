//! Tag projection, which removes tags and restricts tag values of counters and distributions by rules on their names.
//!
//! Tag projection exists to merge samples that differ only in a removed tag into one aggregated context, so it only
//! rewrites the kinds whose aggregation merges samples: counters and distributions. Every other kind passes unchanged,
//! even when a rule names it.
//!
//! Two kinds of rule apply, to the instrumented tags and to the origin tags alike:
//!
//! - **Tag rules** ([`TagFilterRule`]) name one exact raw metric name and either keep only the listed tag keys
//!   (`include`) or remove them (`exclude`). Rules for the same name and action combine; when rules for the same name
//!   disagree, `exclude` wins.
//! - **Value rules** ([`TagValueRule`]) name a raw metric name prefix and a tag key, and the values that tag may have.
//!   A tag with any other value is removed or has its value replaced. Prefixes must not overlap (see
//!   [`validate_tag_value_rules`]), so a metric matches at most one prefix.
//!
//! [`TagProjection`] is the kernel. It keeps the native memo of results per context, which is unobservable: a result
//! served from it equals the result computed again.

use std::{collections::hash_map::Entry, num::NonZeroUsize, time::Duration};

use saluki_common::collections::{FastHashMap, FastHashSet};
use saluki_core::{
    cache::{Cache, CacheBuilder},
    data_model::{
        event::metric::{
            context::{Context, TagSetMutViewState},
            Metric, MetricValues,
        },
        tags::Tag,
    },
    execution::{Kernel, Progress, Turn},
    topology::EventsBuffer,
};

use crate::params::{
    validate_tag_value_rules, InvalidTagValueRules, TagFilterAction, TagFilterRule, TagValueMismatch, TagValueRule,
};

mod telemetry;
pub use self::telemetry::TagProjectionTelemetry;

/// The identifier of the native context memo, under which its cache telemetry is reported.
pub const NATIVE_CONTEXT_CACHE_ID: &str = "tag_filterlist/context_cache";

/// The memory each entry of the context memo is accounted at, in bytes.
pub const CONTEXT_CACHE_ENTRY_BYTES: usize = 64;

const CONTEXT_CACHE_TTI: Duration = Duration::from_secs(30);
const CONTEXT_CACHE_EXPIRATION_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Eq, PartialEq)]
enum CompiledMismatchAction {
    Remove,
    Replace(Tag),
}

#[derive(Clone)]
struct CompiledTagValueAllowlist {
    allowed_values: FastHashSet<String>,
    on_miss: CompiledMismatchAction,
}

struct CompiledKeyFilter {
    is_exclude: bool,
    tag_names: FastHashSet<String>,
}

#[derive(Clone)]
struct CompiledValuePrefixFilter {
    metric_prefix: String,
    value_allowlists: FastHashMap<String, CompiledTagValueAllowlist>,
}

#[derive(Clone, Default)]
struct CompiledValuePrefixFilters(Vec<CompiledValuePrefixFilter>);

impl CompiledValuePrefixFilters {
    fn sort(&mut self) {
        self.0
            .sort_unstable_by(|left, right| left.metric_prefix.cmp(&right.metric_prefix));
    }

    fn get(&self, metric_name: &str) -> Option<&CompiledValuePrefixFilter> {
        find_matching_prefix(&self.0, metric_name, |filter| filter.metric_prefix.as_str())
    }

    fn rule_count(&self) -> usize {
        self.0.iter().map(|filter| filter.value_allowlists.len()).sum()
    }
}

fn find_matching_prefix<'a, T>(entries: &'a [T], value: &str, prefix: impl Fn(&T) -> &str) -> Option<&'a T> {
    match entries.binary_search_by(|candidate| prefix(candidate).cmp(value)) {
        Ok(index) => Some(&entries[index]),
        Err(index) if index > 0 && value.starts_with(prefix(&entries[index - 1])) => Some(&entries[index - 1]),
        _ => None,
    }
}

/// Compiled value rules, which can be combined with different tag rules.
///
/// Value rules are compiled once and validated as they are; tag rules, which may change while the value rules stay,
/// are compiled with them by [`TagProjectionRules::new`].
#[derive(Clone, Default)]
pub struct ValueRules(CompiledValuePrefixFilters);

impl ValueRules {
    /// Compiles value rules.
    ///
    /// # Errors
    ///
    /// Returns an error when the rules cannot be applied unambiguously; see
    /// [`validate_tag_value_rules`].
    pub fn new(rules: &[TagValueRule]) -> Result<Self, InvalidTagValueRules> {
        validate_tag_value_rules(rules)?;

        let mut filters = CompiledValuePrefixFilters::default();
        for rule in rules {
            let on_miss = match &rule.on_miss {
                TagValueMismatch::Remove => CompiledMismatchAction::Remove,
                TagValueMismatch::Replace(replacement) => {
                    CompiledMismatchAction::Replace(Tag::from(format!("{}:{}", rule.tag_name, replacement)))
                }
            };

            let mut allowed_values = FastHashSet::default();
            allowed_values.reserve(rule.values.len());
            allowed_values.extend(rule.values.iter().cloned());
            let allowlist = CompiledTagValueAllowlist {
                allowed_values,
                on_miss,
            };

            if let Some(filter) = filters
                .0
                .iter_mut()
                .find(|filter| filter.metric_prefix == rule.metric_prefix)
            {
                filter.value_allowlists.insert(rule.tag_name.clone(), allowlist);
            } else {
                let mut value_allowlists = FastHashMap::default();
                value_allowlists.insert(rule.tag_name.clone(), allowlist);
                filters.0.push(CompiledValuePrefixFilter {
                    metric_prefix: rule.metric_prefix.clone(),
                    value_allowlists,
                });
            }
        }

        filters.sort();
        Ok(Self(filters))
    }
}

/// Compiled tag projection rules: exact-name tag rules and prefix value rules.
#[derive(Default)]
pub struct TagProjectionRules {
    key_filters: FastHashMap<String, CompiledKeyFilter>,
    value_prefix_filters: CompiledValuePrefixFilters,
}

impl TagProjectionRules {
    /// Compiles tag rules and combines them with compiled value rules.
    ///
    /// A tag rule with an empty metric name is ignored. Rules for the same name and action are combined into the union
    /// of their tags; when rules for the same name have different actions, `exclude` wins, with the tags of the
    /// `exclude` rules only.
    pub fn new(rules: &[TagFilterRule], value_rules: ValueRules) -> Self {
        let mut key_filters = FastHashMap::<String, CompiledKeyFilter>::default();

        for rule in rules {
            if rule.metric_name.is_empty() {
                continue;
            }

            let is_exclude = rule.action == TagFilterAction::Exclude;
            let mut tag_set = FastHashSet::default();
            tag_set.reserve(rule.tags.len());
            tag_set.extend(rule.tags.iter().cloned());

            match key_filters.entry(rule.metric_name.clone()) {
                Entry::Vacant(vacant) => {
                    vacant.insert(CompiledKeyFilter {
                        is_exclude,
                        tag_names: tag_set,
                    });
                }
                Entry::Occupied(mut occupied) => {
                    let existing = occupied.get_mut();
                    if existing.is_exclude == is_exclude {
                        existing.tag_names.extend(tag_set);
                    } else if is_exclude {
                        existing.is_exclude = true;
                        existing.tag_names = tag_set;
                    }
                }
            }
        }

        Self {
            key_filters,
            value_prefix_filters: value_rules.0,
        }
    }

    /// Returns the number of rules in effect: one per metric name with tag rules, plus one per metric prefix and tag
    /// with a value rule.
    pub fn rule_count(&self) -> usize {
        self.key_filters.len() + self.value_prefix_filters.rule_count()
    }
}

/// The outcome of applying tag projection rules to a metric.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilterMetricTagsOutcome {
    /// No rule exists for the metric name.
    RuleMiss,

    /// A rule exists, but applying it changed no tags.
    NoChange,

    /// A rule exists and removed one or more tags.
    Modified {
        /// The number of instrumented and origin tags removed, including those whose value was replaced.
        removed_tags: usize,
    },
}

#[inline]
fn should_keep_tag(tag: &Tag, is_exclude: bool, names: &FastHashSet<String>) -> bool {
    is_exclude != names.contains(tag.as_borrowed().name())
}

#[inline]
fn filter_tag(
    tag: &Tag, key_filter: Option<&CompiledKeyFilter>, value_filter: Option<&CompiledValuePrefixFilter>,
    replacements: &mut Vec<Tag>,
) -> bool {
    if let Some(key_filter) = key_filter {
        if !should_keep_tag(tag, key_filter.is_exclude, &key_filter.tag_names) {
            return false;
        }
    }

    let tag = tag.as_borrowed();

    let Some(value) = tag.value() else {
        return true;
    };
    let Some(allowlist) = value_filter.and_then(|filter| filter.value_allowlists.get(tag.name())) else {
        return true;
    };
    if allowlist.allowed_values.contains(value) {
        return true;
    }

    match &allowlist.on_miss {
        CompiledMismatchAction::Remove => false,
        CompiledMismatchAction::Replace(replacement) if replacement.as_str() == tag.as_ref() => true,
        CompiledMismatchAction::Replace(replacement) => {
            replacements.push(replacement.clone());
            false
        }
    }
}

/// Filters the tags of a metric according to the compiled rules.
///
/// Both instrumented tags and origin tags are filtered using the same key and value rules. Tag rules apply first, so
/// an `include` rule must include a value-restricted tag key for its value rule to apply. A metric and tag match at
/// most one value rule. Value rules leave bare tags unchanged; empty values are processed and can be kept by allowing
/// `""`. A replaced value is inserted into the tag set it came from, once.
///
/// This applies to a metric of any kind; [`TagProjection`] applies it only to the kinds tag projection rewrites. If the
/// metric name has neither an exact tag rule nor a matching value rule prefix, the metric is unchanged. If filtering
/// would change no tags, the metric's context is left untouched, without allocating.
#[inline]
pub fn filter_metric_tags(
    metric: &mut Metric, state: &mut TagSetMutViewState, rules: &TagProjectionRules,
) -> FilterMetricTagsOutcome {
    let metric_name = metric.context().name().clone();
    let key_filter = rules.key_filters.get(metric_name.as_ref());
    let value_filter = rules.value_prefix_filters.get(metric_name.as_ref());
    if key_filter.is_none() && value_filter.is_none() {
        return FilterMetricTagsOutcome::RuleMiss;
    }

    let mut tag_replacements = Vec::new();
    let mut origin_tag_replacements = Vec::new();
    let mut tag_set_view = metric.context_mut().tags_mut_view(state);
    tag_set_view.retain_tags(|tag| filter_tag(tag, key_filter, value_filter, &mut tag_replacements));
    tag_set_view.retain_origin_tags(|tag| filter_tag(tag, key_filter, value_filter, &mut origin_tag_replacements));
    let total_removed = tag_set_view.finish();

    if !tag_replacements.is_empty() || !origin_tag_replacements.is_empty() {
        metric.context_mut().with_tag_sets_mut(|tags, origin_tags| {
            for replacement in tag_replacements {
                tags.insert_tag(replacement);
            }
            for replacement in origin_tag_replacements {
                origin_tags.insert_tag(replacement);
            }
        });
    }

    if total_removed == 0 {
        FilterMetricTagsOutcome::NoChange
    } else {
        FilterMetricTagsOutcome::Modified {
            removed_tags: total_removed,
        }
    }
}

/// Returns `true` for the kinds tag projection rewrites: counters and distributions.
pub fn is_projected(values: &MetricValues) -> bool {
    values.is_sketch() || matches!(values, MetricValues::Counter(_))
}

/// A memo of tag projection results by context.
///
/// For each context it has projected, the memo holds the projected context and the number of tags removed, or that
/// nothing changed. Contexts no rule names are not kept. Entries expire after 30 seconds without use, and the
/// least recently used ones are evicted when the memo is full. Results depend on the rules, so a memo starts empty
/// with every new set of rules.
pub struct ContextMemo {
    cache: Cache<Context, Option<(Context, usize)>>,
}

impl ContextMemo {
    /// Creates a memo holding up to `capacity` contexts, reporting its telemetry as the cache `identifier`.
    ///
    /// A capacity of zero holds one context. The memo expires entries in a task it spawns, so it must be created
    /// inside a Tokio runtime.
    ///
    /// # Panics
    ///
    /// Panics if `identifier` is empty.
    pub fn new(identifier: &str, capacity: usize) -> Self {
        let capacity = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::MIN);
        let cache = CacheBuilder::from_identifier(identifier)
            .expect("identifier cannot be empty")
            .with_capacity(capacity)
            .with_time_to_idle(Some(CONTEXT_CACHE_TTI))
            .with_expiration_interval(CONTEXT_CACHE_EXPIRATION_INTERVAL)
            .build();
        Self { cache }
    }
}

/// The tag projection kernel: applies [tag projection rules][TagProjectionRules] to every counter and distribution,
/// in place.
///
/// Each projected metric records its outcome in the kernel's [telemetry][TagProjectionTelemetry], whether computed or
/// served from the [memo][ContextMemo]. Metrics of other kinds, and events that are not metrics, pass untouched and
/// record nothing.
pub struct TagProjection {
    rules: TagProjectionRules,
    memo: Option<ContextMemo>,
    view_state: TagSetMutViewState,
    telemetry: TagProjectionTelemetry,
}

impl TagProjection {
    /// Creates the kernel from its rules, an optional memo, and its telemetry, which records the number of rules.
    pub fn new(rules: TagProjectionRules, memo: Option<ContextMemo>, telemetry: TagProjectionTelemetry) -> Self {
        telemetry.set_size(rules.rule_count());
        Self {
            rules,
            memo,
            view_state: TagSetMutViewState::default(),
            telemetry,
        }
    }

    /// Replaces the rules in effect, and the memo, which must not hold results of the old rules, and records the
    /// update in telemetry.
    pub fn replace_rules(&mut self, rules: TagProjectionRules, memo: Option<ContextMemo>) {
        self.rules = rules;
        self.memo = memo;
        self.telemetry.set_size(self.rules.rule_count());
        self.telemetry.increment_updates();
    }

    /// Returns the number of rules in effect.
    pub fn rule_count(&self) -> usize {
        self.rules.rule_count()
    }

    /// Projects the tags of one metric, and returns the outcome, or `None` for a kind tag projection does not rewrite.
    pub fn apply(&mut self, metric: &mut Metric) -> Option<FilterMetricTagsOutcome> {
        if !is_projected(metric.values()) {
            return None;
        }

        let Some(memo) = &self.memo else {
            let outcome = filter_metric_tags(metric, &mut self.view_state, &self.rules);
            self.telemetry.record(outcome);
            return Some(outcome);
        };

        let original_context = metric.context().clone();
        let outcome = match memo.cache.get(&original_context) {
            Some(None) => FilterMetricTagsOutcome::NoChange,
            Some(Some((filtered_context, removed_tags))) => {
                *metric.context_mut() = filtered_context;
                FilterMetricTagsOutcome::Modified { removed_tags }
            }
            None => {
                let outcome = filter_metric_tags(metric, &mut self.view_state, &self.rules);
                match outcome {
                    FilterMetricTagsOutcome::RuleMiss => {}
                    FilterMetricTagsOutcome::NoChange => {
                        memo.cache.insert(original_context, None);
                    }
                    FilterMetricTagsOutcome::Modified { removed_tags } => {
                        memo.cache
                            .insert(original_context, Some((metric.context().clone(), removed_tags)));
                    }
                }
                outcome
            }
        };
        self.telemetry.record(outcome);
        Some(outcome)
    }

    /// Projects the tags of every metric in a buffer, in place.
    pub fn apply_buffer(&mut self, events: &mut EventsBuffer) {
        for event in events {
            if let Some(metric) = event.try_as_metric_mut() {
                self.apply(metric);
            }
        }
    }
}

impl Kernel for TagProjection {
    fn consume(&mut self, _turn: &mut Turn<'_>, input: &mut EventsBuffer) -> Progress {
        self.apply_buffer(input);
        Progress::Done
    }
}

#[cfg(test)]
mod reference;
#[cfg(test)]
mod tests;
