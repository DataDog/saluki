use std::{cmp::Ordering, fmt};

/// A metric tag value.
#[derive(Clone, Ord, PartialOrd)]
pub enum MetricTagValue {
    /// A single tag value.
    Single(String),

    /// Multiple tag values.
    Multiple(Vec<String>),
}

impl MetricTagValue {
    fn merge(&mut self, other: Self) {
        if let Self::Multiple(values) = self {
            match other {
                Self::Single(other_value) => values.push(other_value),
                Self::Multiple(other_values) => values.extend(other_values),
            }

            values.sort_unstable();
        } else {
            let old_value = std::mem::replace(self, Self::Multiple(Vec::new()));
            self.merge(old_value);
            self.merge(other);
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Single(_) => 1,
            Self::Multiple(values) => values.len(),
        }
    }

    pub fn values(&self) -> &[String] {
        match self {
            Self::Single(value) => std::slice::from_ref(value),
            Self::Multiple(values) => values.as_slice(),
        }
    }

    /// Returns the value of this tag as a string.
    ///
    /// Single values are returned as-is, while multiple values are joined by a comma.
    pub fn as_string(&self) -> String {
        match self {
            Self::Single(value) => value.clone(),
            Self::Multiple(values) => values.join(","),
        }
    }

    /// Consumes this tag value and renders it as a string.
    ///
    /// Single values are returned as-is, while multiple values are joined by comma.
    pub fn into_string(self) -> String {
        match self {
            Self::Single(value) => value,
            Self::Multiple(values) => values.join(","),
        }
    }
}

impl PartialEq for MetricTagValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Single(a), Self::Single(b)) => a == b,
            (Self::Multiple(a), Self::Multiple(b)) => a == b,
            (Self::Single(a), Self::Multiple(b)) | (Self::Multiple(b), Self::Single(a)) => b.len() == 1 && &b[0] == a,
        }
    }
}

impl Eq for MetricTagValue {}

impl std::hash::Hash for MetricTagValue {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            Self::Single(value) => value.hash(state),
            Self::Multiple(values) => {
                for value in values {
                    value.hash(state);
                }
            }
        }
    }
}

impl fmt::Debug for MetricTagValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MetricTagValue::Single(v) => write!(f, "{}", v),
            MetricTagValue::Multiple(vs) => write!(f, "[{}]", vs.join(", ")),
        }
    }
}

impl IntoIterator for MetricTagValue {
    type Item = String;
    type IntoIter = MetricTagValueIntoIter;

    fn into_iter(self) -> Self::IntoIter {
        match self {
            Self::Single(value) => MetricTagValueIntoIter::single(value),
            Self::Multiple(values) => MetricTagValueIntoIter::multiple(values),
        }
    }
}

impl From<String> for MetricTagValue {
    fn from(value: String) -> Self {
        Self::Single(value)
    }
}

impl<'a> From<&'a str> for MetricTagValue {
    fn from(value: &'a str) -> Self {
        Self::Single(value.to_string())
    }
}

impl From<Vec<String>> for MetricTagValue {
    fn from(mut values: Vec<String>) -> Self {
        values.sort_unstable();
        Self::Multiple(values)
    }
}

enum IterState {
    Empty,
    Single(Option<String>),
    Multiple(std::vec::IntoIter<String>),
}

pub struct MetricTagValueIntoIter {
    state: IterState,
}

impl MetricTagValueIntoIter {
    fn empty() -> Self {
        Self {
            state: IterState::Empty,
        }
    }

    fn single(value: String) -> Self {
        Self {
            state: IterState::Single(Some(value)),
        }
    }

    fn multiple(values: Vec<String>) -> Self {
        Self {
            state: IterState::Multiple(values.into_iter()),
        }
    }
}

impl Iterator for MetricTagValueIntoIter {
    type Item = String;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.state {
            IterState::Empty => None,
            IterState::Single(value) => value.take(),
            IterState::Multiple(iter) => iter.next(),
        }
    }
}

/// A metric tag.
///
/// Metric tags are the second half of a "context", coupled together with a metric name, to uniquely identifies a
/// metric.
///
/// Three tag types are supported: bare tags, key/value tags, and multi-key/value tags.
///
/// - Bare tags are a simple string, such as `production`, where the key and the value are the same.
/// - Key/value tags have a key and a value, such as `service:web`, where the tag will be referred to by the key.
/// - Multi-key/value tags are a special case of key/value tags where a tag with the same key is specified multiple
///   times, where only the value differs. For instance, if two tags come in -- `tag-a:value1` and `tag-a:value2` --
///   they will be combined into a single tag that can conceptually be thought of as `tag-a:value1,value2`.
///
/// The difference between key/value tags and multi-key/value tags is handled in `MetricTagValue`.
#[derive(Clone, Eq, Hash, PartialEq)]
pub enum MetricTag {
    /// A bare tag, such as `production`.
    Bare(String),

    /// A key/value tag, such as `environment:production`.
    ///
    /// Supports the ability to hold multiple values for the same key.
    KeyValue { key: String, value: MetricTagValue },
}

#[allow(clippy::len_without_is_empty)]
impl MetricTag {
    /// Returns the number of tag values.
    ///
    /// For bare tags, this is always one. For key/value tags, this is the number of values that the tag holds.
    pub fn len(&self) -> usize {
        match self {
            Self::Bare(_) => 1,
            Self::KeyValue { value, .. } => value.len(),
        }
    }

    /// Returns the key of this tag.
    ///
    /// Bare tags are returned as-is, while key/value tags return the key.
    pub fn key(&self) -> &str {
        match self {
            Self::Bare(tag) => tag,
            Self::KeyValue { key, .. } => key,
        }
    }

    fn is_same_tag(&self, other: &Self) -> bool {
        match (self, other) {
            (MetricTag::Bare(a), MetricTag::Bare(b)) => a == b,
            (MetricTag::KeyValue { key: a, .. }, MetricTag::KeyValue { key: b, .. }) => a == b,
            _ => false,
        }
    }

    /// Consumes this tag and renders it as a string.
    ///
    /// Bare tags are returned as-is, while key/value tags are rendered as `key:value`. In the case of multi-key/value
    /// tags, the values are joined with commas.
    pub fn into_string(self) -> String {
        match self {
            Self::Bare(tag) => tag,
            Self::KeyValue { key, value } => format!("{}:{}", key, value.into_string()),
        }
    }

    /// Returns the value of this tag as a string.
    ///
    /// Bare tags have no values, and will return `None`. Key/value tags will return a single string with all values
    /// joined by a comma, if multiple values are present.
    pub fn as_value_string(&self) -> Option<String> {
        match self {
            Self::Bare(_) => None,
            Self::KeyValue { value, .. } => Some(value.as_string()),
        }
    }

    /// Consumes this tag and returns an iterator over the values.
    ///
    /// Bare tags have no values, and will return an empty iterator.
    pub fn into_values(self) -> MetricTagValueIntoIter {
        match self {
            Self::Bare(_) => MetricTagValueIntoIter::empty(),
            Self::KeyValue { value, .. } => value.into_iter(),
        }
    }
}

impl PartialOrd for MetricTag {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MetricTag {
    fn cmp(&self, other: &Self) -> Ordering {
        // We sort all tags by key first. If both tags are key/value tags, then they are additionally sorted by their
        // values. In the case where a bare tag and key/value tag both have equal keys, the bare tag is sorted before
        // the key/value tag.
        match (self, other) {
            (Self::Bare(a), Self::Bare(b)) => a.cmp(b),
            (Self::KeyValue { key: ak, value: av }, Self::KeyValue { key: bk, value: bv }) => {
                if ak == bk {
                    av.cmp(bv)
                } else {
                    ak.cmp(bk)
                }
            }
            (Self::Bare(a), Self::KeyValue { key: b, .. }) => {
                if a == b {
                    Ordering::Less
                } else {
                    a.cmp(b)
                }
            }
            (Self::KeyValue { key: a, .. }, Self::Bare(b)) => {
                if a == b {
                    Ordering::Greater
                } else {
                    a.cmp(b)
                }
            }
        }
    }
}

impl fmt::Debug for MetricTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bare(key) => write!(f, "{}", key),
            Self::KeyValue { key, value } => write!(f, "{} => {:?}", key, value),
        }
    }
}

impl From<(String, String)> for MetricTag {
    fn from((key, value): (String, String)) -> Self {
        Self::KeyValue {
            key,
            value: value.into(),
        }
    }
}

impl From<String> for MetricTag {
    fn from(tag: String) -> Self {
        if let Some((key, value)) = tag.split_once(':') {
            Self::KeyValue {
                key: key.to_string(),
                value: value.into(),
            }
        } else {
            Self::Bare(tag)
        }
    }
}

impl<'a> From<&'a str> for MetricTag {
    fn from(tag: &'a str) -> Self {
        if let Some((key, value)) = tag.split_once(':') {
            Self::KeyValue {
                key: key.to_string(),
                value: value.into(),
            }
        } else {
            Self::Bare(tag.to_string())
        }
    }
}

impl<'a> From<(&'a str, &'a str)> for MetricTag {
    fn from((key, value): (&'a str, &'a str)) -> Self {
        Self::KeyValue {
            key: key.into(),
            value: value.into(),
        }
    }
}

impl<'a> From<(&'a str, String)> for MetricTag {
    fn from((key, value): (&'a str, String)) -> Self {
        Self::KeyValue {
            key: key.into(),
            value: value.into(),
        }
    }
}

/// A set of tags, or tagset.
#[derive(Clone, Debug, Default, Eq, PartialEq, PartialOrd, Ord)]
pub struct MetricTags(Vec<MetricTag>);

impl MetricTags {
    /// Returns `true` if the tagset is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Gets the number of tags.
    ///
    /// This accounts for multi-value tags, such that for each key/value tag, the number of values held is used. Bare
    /// tags always count as one.
    pub fn len(&self) -> usize {
        self.0.iter().map(|t| t.len()).sum()
    }

    fn find_tag(&self, other: &MetricTag) -> Option<usize> {
        // We specifically don't use equality here, because we want to ignore the values in any key/value tags: all that
        // matters is type and key, which `is_same_tag` provides.
        self.0.iter().position(|tag| tag.is_same_tag(other))
    }

    /// Returns `true` if the tagset contain the given tag.
    pub fn contains_tag(&self, tag_key: &str) -> bool {
        self.0.iter().any(|tag| tag.key() == tag_key)
    }

    /// Inserts the given tag into the tagset.
    ///
    /// If the given tag already exists in the tagset, the values are merged together. If the tag is a bare tag, it is a
    /// no-op instead.
    ///
    /// For the purpose of merging, tags are only considered equal if they have the same key and type, so a bare tag
    /// with a value of `foo` is not the same as a key/value tag with a key of `foo`.
    pub fn insert_tag(&mut self, tag: impl Into<MetricTag>) {
        let new_tag = tag.into();

        if let Some(idx) = self.find_tag(&new_tag) {
            let existing_tag = &mut self.0[idx];
            match (existing_tag, new_tag) {
                // Inserting a bare tag into an existing bare tag is a no-op.
                (MetricTag::Bare(_), MetricTag::Bare(_)) => {}
                // Inserting a key/value tag into an existing key/value tag merges their values.
                (
                    MetricTag::KeyValue {
                        value: existing_value, ..
                    },
                    MetricTag::KeyValue { value, .. },
                ) => {
                    existing_value.merge(value);
                }
                _ => unreachable!("existing tag type should match new tag type"),
            }
        } else {
            self.0.push(new_tag);
        }
    }

    /// Removes a tag from the tagset, returning it if it existed.
    ///
    /// If there is a bare tag and a key/value tag with the same key, the first one found is removed, based on their
    /// insertion order.
    pub fn remove_tag(&mut self, tag_key: &str) -> Option<MetricTag> {
        let mut i = 0;
        while i < self.0.len() {
            if self.0[i].key() == tag_key {
                return Some(self.0.remove(i));
            } else {
                i += 1;
            }
        }

        None
    }

    /// Retains only the tags specified by the given predicate.
    ///
    /// In other words, remove all tags `e` for which `f(&e)` returns false. This method operates in place, visiting
    /// each element exactly once in the original order, and preserves the order of the retained elements.
    pub fn retain<F>(&mut self, retain_fn: F)
    where
        F: FnMut(&MetricTag) -> bool,
    {
        self.0.retain(retain_fn);
    }

    /// Merges the `other` tagset into this one, only if a given tag is not already present.
    ///
    /// The tags in `self` have priority, meaning that if a tag in `other` already exists in `self`,
    /// it will not be merged. For merging that combines the tag values instead, consider the
    /// `Extend` implementation on `MetricTags`.
    pub fn merge_missing(&mut self, other: Self) {
        for tag in other {
            if self.find_tag(&tag).is_none() {
                self.0.push(tag);
            }
        }
    }

    /// Consumes the tagset and returns it in sorted order.
    ///
    /// Bare tags and key/value tags are sorted based on their keys. If two key/value tags have the same key, they are
    /// additionally sorted according to their values. Whenever a bare tag and key/value tag have equal keys, the bare
    /// tag is sorted before the key/value tag.
    pub fn sorted(mut self) -> Self {
        self.0.sort_unstable();
        self
    }
}

impl From<Vec<String>> for MetricTags {
    fn from(vec: Vec<String>) -> Self {
        Self(vec.into_iter().map(MetricTag::from).collect())
    }
}

impl From<MetricTag> for MetricTags {
    fn from(tag: MetricTag) -> Self {
        Self(vec![tag])
    }
}

impl FromIterator<MetricTag> for MetricTags {
    fn from_iter<T: IntoIterator<Item = MetricTag>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl IntoIterator for MetricTags {
    type Item = MetricTag;
    type IntoIter = std::vec::IntoIter<MetricTag>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a MetricTags {
    type Item = &'a MetricTag;
    type IntoIter = std::slice::Iter<'a, MetricTag>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl Extend<MetricTag> for MetricTags {
    fn extend<T: IntoIterator<Item = MetricTag>>(&mut self, tags: T) {
        for tag in tags {
            self.insert_tag(tag);
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct MetricContext {
    pub name: String,
    pub tags: MetricTags,
}

impl MetricContext {
    /// Returns `true` if the context contains the given tag.
    ///
    /// Matches both bare tags and key/value tags.
    pub fn contains_tag(&self, tag_key: &str) -> bool {
        self.tags.contains_tag(tag_key)
    }

    pub fn insert_tag(&mut self, tag: impl Into<MetricTag>) {
        self.tags.insert_tag(tag);
    }

    pub fn remove_tag(&mut self, tag_key: &str) -> Option<MetricTag> {
        self.tags.remove_tag(tag_key)
    }
}

impl fmt::Display for MetricContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)?;
        if !self.tags.is_empty() {
            write!(f, "{{")?;

            let mut needs_separator = false;
            for tag in &self.tags {
                match tag {
                    MetricTag::Bare(tag) => {
                        if needs_separator {
                            write!(f, ", ")?;
                        } else {
                            needs_separator = true;
                        }

                        write!(f, "{}", tag)?;
                    }
                    MetricTag::KeyValue { key, value } => {
                        for tag_value in value.values() {
                            if needs_separator {
                                write!(f, ", ")?;
                            } else {
                                needs_separator = true;
                            }

                            write!(f, "{}:{}", key, tag_value)?;
                        }
                    }
                }
            }

            write!(f, "}}")?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use proptest::collection::vec as arb_vec;
    use proptest::prelude::*;

    use super::*;

    fn arb_metrictagvalue() -> impl Strategy<Value = MetricTagValue> {
        arb_vec(any::<String>(), 1..10).prop_map(|mut values| {
            if values.len() == 1 {
                values.remove(0).into()
            } else {
                values.into()
            }
        })
    }

    #[test]
    fn test_metrictagvalue_merge() {
        // Single into single.
        let mut single = MetricTagValue::Single("b".into());
        single.merge(MetricTagValue::Single("a".into()));
        assert_eq!(single, MetricTagValue::Multiple(vec!["a".into(), "b".into()]));

        // Single into multiple.
        let mut multiple = MetricTagValue::Multiple(vec!["c".into(), "e".into()]);
        multiple.merge(MetricTagValue::Single("d".into()));
        assert_eq!(
            multiple,
            MetricTagValue::Multiple(vec!["c".into(), "d".into(), "e".into()])
        );

        // Multiple into single.
        let mut single = MetricTagValue::Single("g".into());
        single.merge(MetricTagValue::Multiple(vec!["f".into(), "h".into()]));
        assert_eq!(
            single,
            MetricTagValue::Multiple(vec!["f".into(), "g".into(), "h".into()])
        );

        // Multiple into multiple.
        let mut multiple = MetricTagValue::Multiple(vec!["k".into(), "i".into()]);
        multiple.merge(MetricTagValue::Multiple(vec!["j".into(), "l".into()]));
        assert_eq!(
            multiple,
            MetricTagValue::Multiple(vec!["i".into(), "j".into(), "k".into(), "l".into()])
        );
    }

    proptest! {
        #[test]
        fn property_test_metrictagvalue_merge(input in arb_vec(arb_metrictagvalue(), 1..10)) {
            // We generate a number of `MetricTagValue` values, anywhere between one and ten of them, which themselves
            // can have anywhere between one and ten tag values, and then merge them all together.
            //
            // We make sure that for any combination of inputs, we always get the same result as if we added the all the
            // individual tag values to a vector and sorted it.
            let mut expected = input.iter()
                .cloned()
                .flat_map(|v| match v {
                    MetricTagValue::Single(value) => vec![value],
                    MetricTagValue::Multiple(values) => values,
                })
                .collect::<Vec<_>>();
            expected.sort_unstable();

            let actual = input.into_iter()
                .reduce(|mut acc, v| {
                    acc.merge(v);
                    acc
                })
                .unwrap();

            assert_eq!(expected.as_slice(), actual.values());
        }
    }

    #[test]
    fn test_metrictag_key() {
        let tag = MetricTag::Bare("foo".into());
        assert_eq!(tag.key(), "foo");

        let tag = MetricTag::KeyValue {
            key: "bar".into(),
            value: "baz".into(),
        };
        assert_eq!(tag.key(), "bar");
    }

    #[test]
    fn test_metrictag_is_same_tag() {
        let tag_a = MetricTag::Bare("foo".into());
        let tag_b = MetricTag::Bare("foo".into());
        assert!(tag_a.is_same_tag(&tag_b));

        let tag_a = MetricTag::KeyValue {
            key: "bar".into(),
            value: "baz".into(),
        };
        let tag_b = MetricTag::KeyValue {
            key: "bar".into(),
            value: "baz".into(),
        };
        assert!(tag_a.is_same_tag(&tag_b));

        let tag_a = MetricTag::Bare("foo".into());
        let tag_b = MetricTag::KeyValue {
            key: "foo".into(),
            value: "bar".into(),
        };
        assert!(!tag_a.is_same_tag(&tag_b));

        let tag_a = MetricTag::KeyValue {
            key: "bar".into(),
            value: "baz".into(),
        };
        let tag_b = MetricTag::Bare("bar".into());
        assert!(!tag_a.is_same_tag(&tag_b));

        let tag_a = MetricTag::Bare("foo".into());
        let tag_b = MetricTag::Bare("bar".into());
        assert!(!tag_a.is_same_tag(&tag_b));

        let tag_a = MetricTag::KeyValue {
            key: "bar".into(),
            value: "baz".into(),
        };
        let tag_b = MetricTag::KeyValue {
            key: "baz".into(),
            value: "qux".into(),
        };
        assert!(!tag_a.is_same_tag(&tag_b));
    }

    #[test]
    fn test_metrictag_into_string() {
        let tag = MetricTag::Bare("foo".into());
        assert_eq!(tag.into_string(), "foo");

        let tag = MetricTag::KeyValue {
            key: "bar".into(),
            value: "baz".into(),
        };
        assert_eq!(tag.into_string(), "bar:baz");
    }

    #[test]
    fn test_metrictags_contains_tag() {
        let tags = MetricTags(vec![
            MetricTag::Bare("foo".into()),
            MetricTag::KeyValue {
                key: "bar".into(),
                value: "baz".into(),
            },
        ]);

        assert!(tags.contains_tag("foo"));
        assert!(tags.contains_tag("bar"));
        assert!(!tags.contains_tag("baz"));
    }

    #[test]
    fn test_metrictags_insert_tag() {
        let mut tags = MetricTags::default();
        tags.insert_tag("foo");
        tags.insert_tag(("bar", "baz"));
        tags.insert_tag(("bar", "qux"));

        assert_eq!(
            tags,
            MetricTags(vec![
                MetricTag::Bare("foo".into()),
                MetricTag::KeyValue {
                    key: "bar".into(),
                    value: MetricTagValue::Multiple(vec!["baz".into(), "qux".into()]),
                },
            ])
        );
    }

    #[test]
    fn test_metrictags_insert_tag_different_type() {
        let mut tags = MetricTags::default();
        tags.insert_tag("foo");
        tags.insert_tag(("foo", "bar"));

        assert_eq!(
            tags,
            MetricTags(vec![
                MetricTag::Bare("foo".into()),
                MetricTag::KeyValue {
                    key: "foo".into(),
                    value: "bar".into(),
                },
            ])
        );
    }

    #[test]
    fn test_metrictags_insert_tag_different_type_existing() {
        let mut tags = MetricTags::default();
        tags.insert_tag("foo");
        tags.insert_tag(("foo", "bar"));
        tags.insert_tag(("foo", "baz"));

        assert_eq!(
            tags,
            MetricTags(vec![
                MetricTag::Bare("foo".into()),
                MetricTag::KeyValue {
                    key: "foo".into(),
                    value: MetricTagValue::Multiple(vec!["bar".into(), "baz".into()]),
                },
            ])
        );
    }

    #[test]
    fn test_metrictags_remove_tag() {
        let mut tags = MetricTags(vec![
            MetricTag::Bare("foo".into()),
            MetricTag::KeyValue {
                key: "bar".into(),
                value: "baz".into(),
            },
        ]);

        assert_eq!(tags.remove_tag("foo"), Some(MetricTag::Bare("foo".into())));
        assert_eq!(
            tags.remove_tag("bar"),
            Some(MetricTag::KeyValue {
                key: "bar".into(),
                value: "baz".into(),
            })
        );
        assert_eq!(tags.remove_tag("baz"), None);
    }

    #[test]
    fn test_metrictags_remove_tag_multiple_same_key() {
        let mut tags = MetricTags(vec![
            MetricTag::Bare("foo".into()),
            MetricTag::KeyValue {
                key: "foo".into(),
                value: "baz".into(),
            },
        ]);

        assert_eq!(tags.remove_tag("foo"), Some(MetricTag::Bare("foo".into())));
        assert_eq!(
            tags.remove_tag("foo"),
            Some(MetricTag::KeyValue {
                key: "foo".into(),
                value: "baz".into(),
            })
        );
        assert_eq!(tags.remove_tag("foo"), None);
    }
}
