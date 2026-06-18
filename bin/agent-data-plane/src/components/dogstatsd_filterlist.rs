//! Shared DogStatsD metric filterlist helpers.

use stringtheory::MetaString;

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
    #[cfg(test)]
    pub(super) fn metric_filterlist_is_active(&self) -> bool {
        !self.metric_filterlist.is_empty()
    }

    /// Replaces the current `metric_filterlist`.
    #[cfg(test)]
    pub(super) fn set_metric_filterlist(&mut self, values: Vec<String>) {
        self.metric_filterlist = values;
    }

    /// Replaces the current `metric_filterlist_match_prefix`.
    #[cfg(test)]
    pub(super) fn set_metric_filterlist_match_prefix(&mut self, match_prefix: bool) {
        self.metric_filterlist_match_prefix = match_prefix;
    }

    /// Replaces the legacy `statsd_metric_blocklist`.
    #[cfg(test)]
    pub(super) fn set_metric_blocklist(&mut self, values: Vec<String>) {
        self.metric_blocklist = values;
    }

    /// Builds a metric-name blocklist from the active filter values.
    pub(super) fn to_matcher(&self) -> Blocklist {
        let (values, match_prefix) = self.effective_values();
        Blocklist::new(values.iter().map(String::as_str), match_prefix)
    }
}
