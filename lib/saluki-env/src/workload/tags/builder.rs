// TODO: the Agent has a concept of "standard" tags -- at the same level as low/high card and orechestrator tags -- but
// the method on the equivalent tags builder type (TagList) for adding a "standard" tag adds it to both the dedicated
// standard tags container _and_ the low card tags container... and as far as i can tell, nothing ever really reads the
// "standard" tags directly, just low/high card and orchestrator.
//
// so we've ommitted "standard" tags for now because it kind of seems like a vestigial feature, but we should try and
// confirm this somehow.

#[derive(Default)]
pub struct TagsBuilder {
    pub low_cardinality: Vec<(String, String)>,
    pub high_cardinality: Vec<(String, String)>,
}

impl TagsBuilder {
    /// Adds a low cardinality tag.
    ///
    /// If either the key or value is empty, the tag is not added.
    pub fn add_low_cardinality<K, V>(&mut self, key: K, value: V)
    where
        K: Into<String>,
        V: Into<String>,
    {
        let key = key.into();
        let value = value.into();

        if !key.is_empty() && !value.is_empty() {
            self.low_cardinality.push((key, value));
        }
    }

    /// Adds a high cardinality tag.
    ///
    /// If either the key or value is empty, the tag is not added.
    pub fn add_high_cardinality<K, V>(&mut self, key: K, value: V)
    where
        K: Into<String>,
        V: Into<String>,
    {
        let key = key.into();
        let value = value.into();

        if !key.is_empty() && !value.is_empty() {
            self.high_cardinality.push((key, value));
        }
    }

    /// Adds a tag, automatically determining the cardinality.
    ///
    /// Keys that start with '+' are considered high cardinality tags, and are added with the '+' removed. Otherwise,
    /// the tag is considered low cardinality.
    ///
    /// If either the key or value is empty, the tag is not added.
    pub fn add_automatic<K, V>(&mut self, key: K, value: V)
    where
        K: Into<String> + AsRef<str>,
        V: Into<String>,
    {
        if let Some((_, key_no_prefix)) = key.as_ref().split_once('+') {
            self.add_high_cardinality(key_no_prefix, value.into());
        } else {
            self.add_low_cardinality(key.into(), value.into());
        }
    }
}
