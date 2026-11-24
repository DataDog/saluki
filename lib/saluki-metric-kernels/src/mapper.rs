//! Pure DogStatsD metric-name mapper kernel.
//!
//! Compiles wildcard and regular-expression mapping profiles and expands capture groups into rewritten metric names
//! and extracted tags.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use saluki_core::data_model::{
    event::metric::Metric,
    tags::{SharedTagSet, TagSet},
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::{Deserialize, Serialize};
use stringtheory::MetaString;

const MATCH_TYPE_WILDCARD: &str = "wildcard";
const MATCH_TYPE_REGEX: &str = "regex";

static ALLOWED_WILDCARD_MATCH_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9\-_*.]+$").expect("allowed wildcard pattern regex should be valid"));

/// Configuration for a DogStatsD metric mapping profile.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct DogStatsDMapperProfile {
    /// Profile name used in configuration errors.
    pub name: String,

    /// Metric-name prefix this profile applies to (`"*"` matches every metric).
    pub prefix: String,

    /// Metric mappings evaluated in order for this profile.
    pub mappings: Vec<DogStatsDMetricMapping>,
}

/// Configuration for a single metric mapping in a [`DogStatsDMapperProfile`].
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct DogStatsDMetricMapping {
    /// Pattern matched against the incoming metric name.
    pub metric_match: String,

    /// Pattern type: `"wildcard"`, `"regex"`, or empty for the default wildcard behavior.
    pub match_type: String,

    /// Metric name template emitted after a match.
    pub name: String,

    /// Tag key-to-template pairs emitted after expanding capture groups.
    pub tags: HashMap<String, String>,
}

/// Outcome of matching a metric name against a compiled [`MapperKernel`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MappedMetricName {
    /// Expanded target metric name.
    pub name: MetaString,

    /// Extracted tags expanded from capture groups.
    pub extra_tags: SharedTagSet,
}

#[derive(Clone)]
struct CompiledProfile {
    prefix: String,
    mappings: Vec<CompiledMapping>,
}

#[derive(Clone)]
struct CompiledMapping {
    name: String,
    tags: HashMap<String, String>,
    regex: Regex,
}

/// Pure compiled DogStatsD metric mapper kernel.
#[derive(Clone, Default)]
pub struct MapperKernel {
    profiles: Vec<CompiledProfile>,
}

impl MapperKernel {
    /// Validates and compiles `profiles` into a [`MapperKernel`].
    ///
    /// # Errors
    ///
    /// Returns an error describing the first invalid profile or mapping.
    pub fn compile(profiles: &[DogStatsDMapperProfile]) -> Result<Self, GenericError> {
        let mut compiled = Vec::with_capacity(profiles.len());
        for (profile_index, config_profile) in profiles.iter().enumerate() {
            if config_profile.name.is_empty() {
                return Err(generic_error!("missing profile name {}", profile_index));
            }
            if config_profile.prefix.is_empty() {
                return Err(generic_error!("missing prefix for profile: {}", config_profile.name));
            }

            let mut mappings = Vec::with_capacity(config_profile.mappings.len());
            for (mapping_index, mapping) in config_profile.mappings.iter().enumerate() {
                let match_type = match mapping.match_type.as_str() {
                    "" | MATCH_TYPE_WILDCARD => MATCH_TYPE_WILDCARD,
                    MATCH_TYPE_REGEX => MATCH_TYPE_REGEX,
                    unknown => {
                        return Err(generic_error!(
                            "profile: {}, mapping num {}: invalid match type `{}`, expected `wildcard` or `regex`",
                            config_profile.name,
                            mapping_index,
                            unknown,
                        ))
                    }
                };
                if mapping.name.is_empty() {
                    return Err(generic_error!(
                        "profile: {}, mapping num {}: name is required",
                        config_profile.name,
                        mapping_index
                    ));
                }
                if mapping.metric_match.is_empty() {
                    return Err(generic_error!(
                        "profile: {}, mapping num {}: match is required",
                        config_profile.name,
                        mapping_index
                    ));
                }
                let regex = build_regex(&mapping.metric_match, match_type)?;
                mappings.push(CompiledMapping {
                    name: mapping.name.clone(),
                    tags: mapping.tags.clone(),
                    regex,
                });
            }

            compiled.push(CompiledProfile {
                prefix: config_profile.prefix.clone(),
                mappings,
            });
        }

        Ok(Self { profiles: compiled })
    }

    /// Returns `true` when no mapping profiles are active.
    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }

    /// Attempts to match `metric_name` against the compiled profiles and expand the first match.
    pub fn match_name(&self, metric_name: &str) -> Option<MappedMetricName> {
        if self.profiles.is_empty() {
            return None;
        }

        let mut new_name = String::new();
        let mut expanded_tag_value = String::new();

        for profile in &self.profiles {
            if !metric_name.starts_with(&profile.prefix) && profile.prefix != "*" {
                continue;
            }

            for mapping in &profile.mappings {
                if let Some(captures) = mapping.regex.captures(metric_name) {
                    new_name.clear();
                    captures.expand(&mapping.name, &mut new_name);

                    let mut extra_tags = TagSet::with_capacity(mapping.tags.len());
                    for (tag_key, tag_value_expr) in &mapping.tags {
                        expanded_tag_value.clear();
                        expanded_tag_value.push_str(tag_key);
                        expanded_tag_value.push(':');
                        captures.expand(tag_value_expr, &mut expanded_tag_value);

                        extra_tags.insert_tag(expanded_tag_value.as_str());
                    }

                    return Some(MappedMetricName {
                        name: MetaString::from(new_name),
                        extra_tags: extra_tags.into_shared(),
                    });
                }
            }
        }

        None
    }

    /// Rewrites `metric` in place if its name matches a compiled profile.
    ///
    /// Returns `true` if `metric` was remapped.
    pub fn apply(&self, metric: &mut Metric) -> bool {
        let Some(mapped) = self.match_name(metric.context().name().as_ref()) else {
            return false;
        };

        let mut new_context = metric.context().with_name(mapped.name);
        if !mapped.extra_tags.is_empty() {
            new_context.mutate_tags(|tags| tags.merge_shared(&mapped.extra_tags));
        }
        *metric.context_mut() = new_context;
        true
    }
}

fn build_regex(match_re: &str, match_type: &str) -> Result<Regex, GenericError> {
    let mut pattern = match_re.to_owned();
    if match_type == MATCH_TYPE_WILDCARD {
        if !ALLOWED_WILDCARD_MATCH_PATTERN.is_match(&pattern) {
            return Err(generic_error!(
                "invalid wildcard match pattern `{}`, it does not match allowed match regex `{}`",
                pattern,
                ALLOWED_WILDCARD_MATCH_PATTERN.as_str()
            ));
        }
        if pattern.contains("**") {
            return Err(generic_error!(
                "invalid wildcard match pattern `{}`, it should not contain consecutive `*`",
                pattern
            ));
        }
        pattern = pattern.replace('.', "\\.");
        pattern = pattern.replace('*', "([^.]*)");
    }

    let final_pattern = format!("^{}$", pattern);

    Regex::new(&final_pattern).with_error_context(|| {
        format!(
            "Failed to compile regular expression `{}` for `{}` match type",
            final_pattern, match_type
        )
    })
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::{event::metric::context::Context, tags::Tag};

    use super::*;

    #[test]
    fn wildcard_mapping_rewrites_name_and_extracts_tags() {
        let profiles = vec![DogStatsDMapperProfile {
            name: "test".to_string(),
            prefix: "test.".to_string(),
            mappings: vec![DogStatsDMetricMapping {
                metric_match: "test.job.*.duration".to_string(),
                match_type: "wildcard".to_string(),
                name: "test.job.duration".to_string(),
                tags: HashMap::from([("job".to_string(), "$1".to_string())]),
            }],
        }];

        let kernel = MapperKernel::compile(&profiles).expect("valid profile");
        let mut metric = Metric::gauge(
            Context::from_static_parts("test.job.ingest.duration", &["env:prod"]),
            1.0,
        );

        assert!(kernel.apply(&mut metric));
        assert_eq!(metric.context().name(), "test.job.duration");
        assert!(metric.context().tags().has_tag(Tag::from("job:ingest")));
        assert!(metric.context().tags().has_tag(Tag::from("env:prod")));
    }

    #[test]
    fn rejects_invalid_wildcard_pattern() {
        let profiles = vec![DogStatsDMapperProfile {
            name: "bad".to_string(),
            prefix: "test.".to_string(),
            mappings: vec![DogStatsDMetricMapping {
                metric_match: "test.**".to_string(),
                match_type: "wildcard".to_string(),
                name: "test.out".to_string(),
                tags: HashMap::new(),
            }],
        }];

        assert!(MapperKernel::compile(&profiles).is_err());
    }
}
