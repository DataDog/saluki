//! Source types for object arrays without `items` schemas.
//!
//! The generated configuration model exposes these arrays as `Vec<serde_json::Value>`. The
//! translator deserializes each value into a type in this module before mapping it to the ADP model.
//!
//! These types mirror the core Agent's `MappingProfileConfig`, `MetricMappingConfig`, and
//! `MetricTagListEntry` structs.

use std::collections::HashMap;

use serde::Deserialize;

/// One `dogstatsd_mapper_profiles` element.
#[derive(Clone, Debug, Deserialize)]
pub struct MapperProfile {
    /// Name of the profile.
    pub name: String,

    /// Metric-name prefix that selects this profile.
    pub prefix: String,

    /// Mapping rules the profile applies to a matching metric name.
    #[serde(default)]
    pub mappings: Vec<MetricMapping>,
}

/// One `mappings` element of a [`MapperProfile`].
#[derive(Clone, Debug, Deserialize)]
pub struct MetricMapping {
    /// Pattern the metric name is matched against.
    ///
    /// Renamed because the schema spells this field `match`, a Rust keyword.
    #[serde(rename = "match")]
    pub metric_match: String,

    /// How `metric_match` is interpreted, such as `wildcard` or `regex`.
    #[serde(default)]
    pub match_type: String,

    /// Name the matched metric is rewritten to.
    pub name: String,

    /// Tags added to the matched metric, with capture-group references allowed in the values.
    #[serde(default)]
    pub tags: HashMap<String, String>,
}

/// One `metric_tag_filterlist` element.
#[derive(Clone, Debug, Deserialize)]
pub struct MetricTagFilterlistEntry {
    /// Metric name the entry applies to.
    pub metric_name: String,

    /// Whether `tags` is an include or an exclude list.
    ///
    /// The schema types this as a free string, so the value is carried verbatim and classified by
    /// the consumer.
    #[serde(default)]
    pub action: String,

    /// Tags the action applies to.
    #[serde(default)]
    pub tags: Vec<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{MapperProfile, MetricTagFilterlistEntry};

    #[test]
    fn mapper_profile_defaults_its_optional_fields() {
        let profile: MapperProfile = serde_json::from_value(json!({ "name": "p", "prefix": "svc." }))
            .expect("a profile without mappings is valid");
        assert!(profile.mappings.is_empty());

        let profile: MapperProfile = serde_json::from_value(json!({
            "name": "p",
            "prefix": "svc.",
            "mappings": [{ "match": "svc.*.latency", "name": "svc.latency" }],
        }))
        .expect("a mapping without match_type or tags is valid");
        let mapping = &profile.mappings[0];
        assert_eq!(mapping.metric_match, "svc.*.latency");
        assert_eq!(mapping.name, "svc.latency");
        assert_eq!(mapping.match_type, "");
        assert!(mapping.tags.is_empty());
    }

    #[test]
    fn mapper_profile_requires_its_identifying_fields() {
        assert!(serde_json::from_value::<MapperProfile>(json!({ "prefix": "svc." })).is_err());
        assert!(serde_json::from_value::<MapperProfile>(json!({ "name": "p" })).is_err());
        assert!(serde_json::from_value::<MapperProfile>(json!({
            "name": "p",
            "prefix": "svc.",
            "mappings": [{ "name": "svc.latency" }],
        }))
        .is_err());
    }

    #[test]
    fn tag_filterlist_entry_defaults_action_and_tags() {
        let entry: MetricTagFilterlistEntry = serde_json::from_value(json!({ "metric_name": "svc.latency" }))
            .expect("an entry with only a metric name is valid");
        assert_eq!(entry.action, "");
        assert!(entry.tags.is_empty());
    }

    #[test]
    fn tag_filterlist_entry_requires_a_metric_name() {
        assert!(serde_json::from_value::<MetricTagFilterlistEntry>(json!({ "tags": ["host"] })).is_err());
    }
}
