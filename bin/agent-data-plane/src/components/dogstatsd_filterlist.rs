//! Shared DogStatsD metric filterlist helpers.

use stringtheory::MetaString;

/// Configuration key for the current Agent metric filterlist.
pub(super) const METRIC_FILTERLIST_CONFIG_KEY: &str = "metric_filterlist";
/// Configuration key for prefix matching on the current Agent metric filterlist.
pub(super) const METRIC_FILTERLIST_MATCH_PREFIX_CONFIG_KEY: &str = "metric_filterlist_match_prefix";
/// Configuration key for per-metric tag filter rules.
pub(super) const METRIC_TAG_FILTERLIST_CONFIG_KEY: &str = "metric_tag_filterlist";
/// Configuration key for the legacy Agent DogStatsD metric blocklist.
pub(super) const STATSD_METRIC_BLOCKLIST_CONFIG_KEY: &str = "statsd_metric_blocklist";
/// Configuration key for prefix matching on the legacy Agent DogStatsD metric blocklist.
pub(super) const STATSD_METRIC_BLOCKLIST_MATCH_PREFIX_CONFIG_KEY: &str = "statsd_metric_blocklist_match_prefix";

/// Compiled blocklist for metric names that should be filtered.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct Blocklist {
    data: Vec<MetaString>,
    match_prefix: bool,
}

impl Blocklist {
    /// Creates a matcher from filter values and the match mode.
    pub(super) fn new<T, I>(values: I, match_prefix: bool) -> Self
    where
        T: AsRef<str>,
        I: IntoIterator<Item = T>,
    {
        let mut data = values
            .into_iter()
            .map(|value| MetaString::from(value.as_ref()))
            .collect::<Vec<_>>();
        data.sort_by(|a, b| a.as_ref().cmp(b.as_ref()));

        if match_prefix && !data.is_empty() {
            let mut i = 0;
            for j in 1..data.len() {
                if !data[j].as_ref().starts_with(data[i].as_ref()) {
                    i += 1;
                    data[i] = data[j].clone();
                }
            }
            data.truncate(i + 1);
        }

        Self { data, match_prefix }
    }

    /// Returns whether the blocklist has no configured values.
    pub(super) fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Returns whether `name` matches a configured metric name.
    pub(super) fn contains(&self, name: &str) -> bool {
        if self.data.is_empty() {
            return false;
        }

        let i = self.data.binary_search_by(|candidate| candidate.as_ref().cmp(name));

        if self.match_prefix {
            let index = i.unwrap_or_else(|idx| idx);
            if index > 0 && name.starts_with(self.data[index - 1].as_ref()) {
                return true;
            }
        }

        i.is_ok()
    }
}

/// Runtime state for Agent metric filterlist configuration.
///
/// The current `metric_filterlist` takes precedence when configured. Otherwise, the legacy
/// `statsd_metric_blocklist` values are active.
#[derive(Clone, Debug, Default)]
pub(super) struct EffectiveFilterlist {
    metric_filterlist: Vec<String>,
    metric_filterlist_match_prefix: bool,
    metric_blocklist: Vec<String>,
    metric_blocklist_match_prefix: bool,
}

impl EffectiveFilterlist {
    /// Creates effective filterlist state from current and legacy values.
    pub(super) fn new(
        metric_filterlist: Vec<String>, metric_filterlist_match_prefix: bool, metric_blocklist: Vec<String>,
        metric_blocklist_match_prefix: bool,
    ) -> Self {
        Self {
            metric_filterlist,
            metric_filterlist_match_prefix,
            metric_blocklist,
            metric_blocklist_match_prefix,
        }
    }

    /// Returns the active values and match mode.
    pub(super) fn effective_values(&self) -> (&[String], bool) {
        if !self.metric_filterlist.is_empty() {
            (&self.metric_filterlist, self.metric_filterlist_match_prefix)
        } else {
            (&self.metric_blocklist, self.metric_blocklist_match_prefix)
        }
    }

    /// Returns the number of active filter entries.
    pub(super) fn effective_len(&self) -> usize {
        self.effective_values().0.len()
    }

    /// Returns whether the current `metric_filterlist` is active.
    pub(super) fn metric_filterlist_is_active(&self) -> bool {
        !self.metric_filterlist.is_empty()
    }

    /// Replaces the current `metric_filterlist`.
    pub(super) fn set_metric_filterlist(&mut self, values: Vec<String>) {
        self.metric_filterlist = values;
    }

    /// Replaces the current `metric_filterlist_match_prefix`.
    pub(super) fn set_metric_filterlist_match_prefix(&mut self, match_prefix: bool) {
        self.metric_filterlist_match_prefix = match_prefix;
    }

    /// Replaces the legacy `statsd_metric_blocklist`.
    pub(super) fn set_metric_blocklist(&mut self, values: Vec<String>) {
        self.metric_blocklist = values;
    }

    /// Replaces the legacy `statsd_metric_blocklist_match_prefix`.
    pub(super) fn set_metric_blocklist_match_prefix(&mut self, match_prefix: bool) {
        self.metric_blocklist_match_prefix = match_prefix;
    }

    /// Builds a metric-name blocklist from the active filter values.
    pub(super) fn to_matcher(&self) -> Blocklist {
        let (values, match_prefix) = self.effective_values();
        Blocklist::new(values.iter().map(String::as_str), match_prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::Blocklist;

    #[test]
    fn blocklist_empty_when_default() {
        assert!(Blocklist::default().is_empty());
    }

    #[test]
    fn blocklist_not_empty_with_exact_matches() {
        assert!(!Blocklist::new(["foo"], false).is_empty());
    }

    #[test]
    fn blocklist_not_empty_with_prefix_matches() {
        assert!(!Blocklist::new(["foo", "foo.bar"], true).is_empty());
    }
}
