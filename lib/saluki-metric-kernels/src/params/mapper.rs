/// How a mapping's pattern is matched against a metric name.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MatchType {
    /// The pattern is a wildcard pattern, in which `*` matches any run of characters other than `.`, and every other
    /// character matches itself.
    Wildcard,

    /// The pattern is a regular expression, anchored at both ends.
    Regex,
}

impl MatchType {
    /// Returns the match type's name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wildcard => "wildcard",
            Self::Regex => "regex",
        }
    }
}

/// One mapping of a [`MapperProfile`]: a pattern, the name a matching metric is renamed to, and the tags it gains.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetricMapping {
    /// The pattern matched against the raw metric name.
    pub metric_match: String,

    /// How the pattern is matched.
    pub match_type: MatchType,

    /// The new name, which may refer to the pattern's captures, such as `$1`.
    pub name: String,

    /// The tags added to a matching metric, as `(key, value)` pairs whose values may refer to the pattern's captures.
    ///
    /// The metric keeps its existing tags, even one with the same key.
    pub tags: Vec<(String, String)>,
}

/// A name mapping profile: the mappings that apply to metric names starting with a prefix.
///
/// Profiles are tried in order, and a profile applies to a metric whose raw name starts with its prefix, or to every
/// metric if its prefix is `*`. Within a profile, the first mapping whose pattern matches renames the metric. A metric
/// that no mapping matches is left unchanged. Mapping changes only the name and tags; the host, origin tags, values,
/// and metadata are kept.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MapperProfile {
    /// The profile's name, for diagnostics.
    pub name: String,

    /// The prefix of the raw names the profile applies to, or `*` for every name.
    pub prefix: String,

    /// The profile's mappings, in the order they are tried.
    pub mappings: Vec<MetricMapping>,
}
