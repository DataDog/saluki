//! OTLP metric dimensions.

use otlp_protos::opentelemetry::proto::common::v1 as otlp_common;
use saluki_context::tags::{SharedTagSet, TagSet};

/// A helper struct for building the identity of a metric.
#[derive(Clone, Debug, Default)]
pub struct Dimensions {
    pub name: String,
    pub tags: SharedTagSet,
    pub host: Option<String>,
    #[allow(dead_code)]
    pub origin_id: Option<String>,
}

impl Dimensions {
    /// Creates a new `Dimensions` with a suffix added to the name.
    pub fn with_suffix(&self, suffix: &str) -> Self {
        Self {
            name: format!("{}.{}", self.name, suffix),
            tags: self.tags.clone(),
            host: self.host.clone(),
            origin_id: self.origin_id.clone(),
        }
    }

    /// Creates a new `Dimensions` with additional tags.
    #[allow(dead_code)]
    pub fn add_tags(&self, tags_to_add: &[String]) -> Self {
        let mut new_tags = TagSet::default();
        for tag in &self.tags {
            new_tags.insert_tag(tag.clone());
        }
        for tag in tags_to_add {
            new_tags.insert_tag(tag.clone());
        }

        Self {
            name: self.name.clone(),
            tags: new_tags.into_shared(),
            host: self.host.clone(),
            origin_id: self.origin_id.clone(),
        }
    }

    /// Creates a new `Dimensions` with tags from an OTLP attribute map.
    pub fn with_attribute_map(&self, attributes: &[otlp_common::KeyValue]) -> Self {
        if attributes.is_empty() {
            return self.clone();
        }

        let mut tag_set = TagSet::default();
        for kv in attributes {
            if let Some(value) = kv.value.as_ref().and_then(|v| v.value.as_ref()) {
                let v_str = match value {
                    otlp_common::any_value::Value::StringValue(s) => s.clone(),
                    otlp_common::any_value::Value::BoolValue(b) => b.to_string(),
                    otlp_common::any_value::Value::IntValue(i) => i.to_string(),
                    otlp_common::any_value::Value::DoubleValue(d) => d.to_string(),
                    // Other types (like bytes, array, kvlist) are not converted to tags in the Go implementation.
                    _ => continue,
                };
                tag_set.insert_tag(format!("{}:{}", kv.key, v_str));
            }
        }

        let mut tags = tag_set.into_shared();
        tags.extend_from_shared(&self.tags);

        Self {
            name: self.name.clone(),
            tags,
            host: self.host.clone(),
            origin_id: self.origin_id.clone(),
        }
    }

    /// Creates a canonical cache key for the dimension set.
    pub fn get_cache_key(&self) -> String {
        let mut dimensions: Vec<String> = self.tags.into_iter().map(|t| t.to_string()).collect();

        dimensions.push(format!("name:{}", self.name));

        if let Some(host) = &self.host {
            dimensions.push(format!("host:{}", host));
        }
        if let Some(origin_id) = &self.origin_id {
            dimensions.push(format!("originID:{}", origin_id));
        }

        // Sort the dimensions alphabetically to ensure a canonical key.
        dimensions.sort();

        // Join with a null character separator.
        dimensions.join("\0")
    }
}
