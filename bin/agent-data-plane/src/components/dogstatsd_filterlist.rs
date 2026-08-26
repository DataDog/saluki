//! Shared DogStatsD metric filterlist matcher.

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
