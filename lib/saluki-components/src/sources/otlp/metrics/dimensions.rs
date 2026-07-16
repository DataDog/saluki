//! OTLP metric dimensions.

use otlp_protos::opentelemetry::proto::common::v1 as otlp_common;
use saluki_context::tags::{SharedTagSet, Tag, TagSet};
use stringtheory::MetaString;

use super::internal::utils::format_key_value_tag;

/// A helper struct for building the identity of a metric.
#[derive(Clone, Debug, Default)]
pub struct Dimensions {
    pub name: String,
    pub tags: SharedTagSet,
    pub host: Option<MetaString>,
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

    /// Creates a new `Dimensions` with additional tags, structurally sharing the existing tags.
    pub fn add_tags<I>(&self, tags_to_add: I) -> Self
    where
        I: IntoIterator<Item = String>,
    {
        let additions: SharedTagSet = tags_to_add
            .into_iter()
            .filter(|tag| !self.tags.has_tag(tag))
            .map(Tag::from)
            .collect();

        let mut tags = self.tags.clone();
        tags.extend_from_shared(&additions);

        Self {
            name: self.name.clone(),
            tags,
            host: self.host.clone(),
            origin_id: self.origin_id.clone(),
        }
    }

    /// Creates a new `Dimensions` with tags from an OTLP attribute map.
    ///
    /// When `shadowing_resource_attributes` is `Some`, any data-point attribute whose key also
    /// appears in the resource attributes is skipped. This matches the Agent, where resource
    /// attributes overwrite colliding data-point attributes when they are added as tags.
    pub fn with_attribute_map(
        &self, attributes: &[otlp_common::KeyValue], shadowing_resource_attributes: Option<&[otlp_common::KeyValue]>,
    ) -> Self {
        if attributes.is_empty() {
            return self.clone();
        }

        let mut tag_set = TagSet::default();
        for kv in attributes {
            if let Some(resource_attributes) = shadowing_resource_attributes {
                if resource_attributes.iter().any(|resource_kv| resource_kv.key == kv.key) {
                    continue;
                }
            }

            if let Some(value) = kv.value.as_ref().and_then(|v| v.value.as_ref()) {
                let v_str = match value {
                    otlp_common::any_value::Value::StringValue(s) => s.clone(),
                    otlp_common::any_value::Value::BoolValue(b) => b.to_string(),
                    otlp_common::any_value::Value::IntValue(i) => i.to_string(),
                    otlp_common::any_value::Value::DoubleValue(d) => d.to_string(),
                    // Bytes, arrays, and key-value lists are not converted to tags.
                    _ => continue,
                };
                // Empty values render as `n/a`, matching the Agent's `FormatKeyValueTag`.
                tag_set.insert_tag(format_key_value_tag(&kv.key, &v_str));
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

#[cfg(test)]
mod tests {
    use saluki_context::tags::{Tag, TagSet};

    use super::*;

    fn dims_with_tags(name: &str, tags: &[&str]) -> Dimensions {
        let tag_set: TagSet = tags.iter().map(|t| Tag::from(*t)).collect();
        Dimensions {
            name: name.to_string(),
            tags: tag_set.into_shared(),
            host: None,
            origin_id: None,
        }
    }

    #[test]
    fn add_tags_preserves_order_deduplication_and_base() {
        let base = dims_with_tags("http.request.duration", &["env:prod", "service:web", "lower_bound:1.0"]);
        let extended = base.add_tags(["lower_bound:1.0".to_string(), "upper_bound:2.0".to_string()]);

        let extended_tags: Vec<&str> = extended.tags.into_iter().map(|t| t.as_str()).collect();
        assert_eq!(
            extended_tags,
            vec!["env:prod", "service:web", "lower_bound:1.0", "upper_bound:2.0"]
        );

        let base_tags: Vec<&str> = base.tags.into_iter().map(|t| t.as_str()).collect();
        assert_eq!(base_tags, vec!["env:prod", "service:web", "lower_bound:1.0"]);
    }

    #[test]
    fn add_tags_preserves_canonical_cache_key() {
        let base = dims_with_tags("http.request.duration", &["service:web", "env:prod"]);
        let extended = base.add_tags(["lower_bound:1.0".to_string(), "upper_bound:2.0".to_string()]);

        let expected = [
            "env:prod",
            "lower_bound:1.0",
            "name:http.request.duration",
            "service:web",
            "upper_bound:2.0",
        ]
        .join("\0");
        assert_eq!(extended.get_cache_key(), expected);
    }
}
