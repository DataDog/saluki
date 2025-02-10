use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::LazyLock;

use async_trait::async_trait;
use bytesize::ByteSize;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use regex::Regex;
use saluki_config::GenericConfiguration;
use saluki_context::{tags::TagSet, Context, ContextResolver, ContextResolverBuilder};
use saluki_core::{
    components::transforms::{SynchronousTransform, SynchronousTransformBuilder},
    topology::interconnect::FixedSizeEventBuffer,
};
use saluki_error::{generic_error, GenericError};
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr, PickFirst};

const MATCH_TYPE_WILDCARD: &str = "wildcard";
const MATCH_TYPE_REGEX: &str = "regex";

static ALLOWED_WILDCARD_MATCH_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9\-_*.]+$").expect("Invalid regex in ALLOWED_WILDCARD_MATCH_PATTERN"));

const fn default_context_string_interner_size() -> ByteSize {
    ByteSize::kib(64)
}
/// DogstatsD mapper transform.
#[serde_as]
#[derive(Deserialize)]
pub struct DogstatsDMapperConfiguration {
    /// Total size of the string interner used for contexts.
    ///
    /// This controls the amount of memory that can be used to intern metric names and tags. If the interner is full,
    /// metrics with contexts that have not already been resolved may or may not be dropped, depending on the value of
    /// `allow_context_heap_allocations`.
    #[serde(
        rename = "dogstatsd_mapper_string_interner_size",
        default = "default_context_string_interner_size"
    )]
    context_string_interner_bytes: ByteSize,

    /// Configuration related to metric mapping.
    #[serde_as(as = "PickFirst<(DisplayFromStr, _)>")]
    #[serde(default)]
    dogstatsd_mapper_profiles: MapperProfileConfigs,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct MappingProfileConfig {
    name: String,
    prefix: String,
    mappings: Vec<MetricMappingConfig>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct MapperProfileConfigs(pub Vec<MappingProfileConfig>);

impl FromStr for MapperProfileConfigs {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let profiles: Vec<MappingProfileConfig> = serde_json::from_str(s)?;
        Ok(MapperProfileConfigs(profiles))
    }
}

impl MapperProfileConfigs {
    fn build(&self, context_string_interner_bytes: ByteSize) -> Result<MetricMapper, GenericError> {
        let mut profiles = Vec::with_capacity(self.0.len());
        for (i, config_profile) in self.0.iter().enumerate() {
            if config_profile.name.is_empty() {
                return Err(generic_error!("missing profile name"));
            }
            if config_profile.prefix.is_empty() {
                return Err(generic_error!("missing prefix for profile: {}", config_profile.name));
            }

            let mut profile = MappingProfile {
                prefix: config_profile.prefix.clone(),
                mappings: Vec::with_capacity(config_profile.mappings.len()),
            };

            for mapping in &config_profile.mappings {
                let mut match_type = mapping.match_type.to_string();
                if mapping.match_type.is_empty() {
                    match_type = "wildcard".to_string();
                }
                if match_type != MATCH_TYPE_WILDCARD && match_type != MATCH_TYPE_REGEX {
                    return Err(generic_error!(
                        "profile: {}, mapping num {}: invalid match type, must be `wildcard` or `regex`",
                        config_profile.name,
                        i
                    ));
                }
                if mapping.name.is_empty() {
                    return Err(generic_error!(
                        "profile: {}, mapping num {}: name is required",
                        config_profile.name,
                        i
                    ));
                }
                if mapping.match_type.is_empty() {
                    return Err(generic_error!(
                        "profile: {}, mapping num {}: match is required",
                        config_profile.name,
                        i
                    ));
                }
                let regex = build_regex(&mapping.metric_match, &match_type)?;
                profile.mappings.push(MetricMapping {
                    name: mapping.name.clone(),
                    tags: mapping.tags.clone(),
                    regex,
                });
            }
            profiles.push(profile);
        }

        let context_string_interner_size = NonZeroUsize::new(context_string_interner_bytes.as_u64() as usize)
            .ok_or_else(|| generic_error!("context_string_interner_size must be greater than 0"))
            .unwrap();
        let context_resolver = ContextResolverBuilder::from_name("dogstatsd_mapper")
            .expect("resolver name is not empty")
            .with_interner_capacity_bytes(context_string_interner_size)
            .build();

        Ok(MetricMapper {
            context_resolver,
            profiles,
        })
    }
}

fn build_regex(match_re: &str, match_type: &str) -> Result<Regex, GenericError> {
    let mut pattern = match_re.to_owned();
    if match_type == MATCH_TYPE_WILDCARD {
        // Check it against the allowed wildcard pattern
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
        pattern = pattern.replace(".", "\\.");
        pattern = pattern.replace("*", "([^.]*)");
    }

    let final_pattern = format!("^{}$", pattern);

    match Regex::new(&final_pattern) {
        Ok(re) => Ok(re),
        Err(e) => Err(generic_error!(
            "invalid match `{}`, cannot compile regex: {}",
            match_re,
            e
        )),
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct MetricMappingConfig {
    #[serde(rename = "match")]
    metric_match: String,
    match_type: String,
    name: String,
    tags: HashMap<String, String>,
}

struct MappingProfile {
    prefix: String,
    mappings: Vec<MetricMapping>,
}

struct MetricMapping {
    name: String,
    tags: HashMap<String, String>,
    regex: Regex,
}

struct MetricMapper {
    profiles: Vec<MappingProfile>,
    context_resolver: ContextResolver,
}

impl MetricMapper {
    fn map(&mut self, metric_name: &str, existing_tags: TagSet) -> Option<Context> {
        for profile in &self.profiles {
            if !metric_name.starts_with(&profile.prefix) && profile.prefix != "*" {
                continue;
            }

            for mapping in &profile.mappings {
                if let Some(captures) = mapping.regex.captures(metric_name) {
                    let mut name = String::new();
                    captures.expand(&mapping.name, &mut name);
                    let mut tags = Vec::with_capacity(mapping.tags.len());
                    for (tag_key, tag_value_expr) in &mapping.tags {
                        let mut expanded_value = String::new();
                        captures.expand(tag_value_expr, &mut expanded_value);
                        tags.push(format!("{}:{}", tag_key, expanded_value));
                    }
                    for tag in existing_tags {
                        tags.push(tag.as_str().to_owned());
                    }
                    return self.context_resolver.resolve(&name, tags, None);
                }
            }
        }
        None
    }
}

impl DogstatsDMapperConfiguration {
    /// Creates a new `DogstatsDMapperConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }
}

#[async_trait]
impl SynchronousTransformBuilder for DogstatsDMapperConfiguration {
    async fn build(&self) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        let metric_mapper = self
            .dogstatsd_mapper_profiles
            .build(self.context_string_interner_bytes)?;
        Ok(Box::new(DogstatsDMapper { metric_mapper }))
    }
}

impl MemoryBounds for DogstatsDMapperConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built.
            .with_single_value::<DogstatsDMapper>("component struct")
            // We also allocate the backing storage for the string interner up front, which is used by our context
            // resolver.
            .with_fixed_amount("string interner", self.context_string_interner_bytes.as_u64() as usize);
    }
}

pub struct DogstatsDMapper {
    metric_mapper: MetricMapper,
}

impl SynchronousTransform for DogstatsDMapper {
    fn transform_buffer(&mut self, event_buffer: &mut FixedSizeEventBuffer) {
        for event in event_buffer {
            if let Some(metric) = event.try_as_metric_mut() {
                // TODO: Origin tags should be added before we the mapper's context resolver is used.
                if let Some(new_context) = self
                    .metric_mapper
                    .map(metric.context().name(), metric.context().tags().to_owned())
                {
                    *metric.context_mut() = new_context;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use bytesize::ByteSize;
    use saluki_context::tags::TagSet;
    use stringtheory::MetaString;

    use super::{MapperProfileConfigs, MetricMapper};

    fn mapper(json_data: &str) -> MetricMapper {
        let mpc: MapperProfileConfigs = serde_json::from_str(&json_data).unwrap();
        let context_string_interner_bytes = ByteSize::kib(64);
        mpc.build(context_string_interner_bytes).unwrap()
    }

    fn tags() -> TagSet {
        let mut existing_tags = TagSet::with_capacity(2);
        existing_tags.insert_tag(MetaString::from_static("foo:bar"));
        existing_tags.insert_tag(MetaString::from_static("baz"));
        existing_tags
    }

    #[test]
    fn test_mapper_wildcard() {
        let json_data = r#"[{"name":"my_custom_metric_profile","prefix":"custom_metric.","mappings":[{"match":"custom_metric.process.*.*","match_type":"wildcard","name":"custom_metric.process","tags":{"tag_key_1":"$1","tag_key_2":"$2"}}]}]"#;
        let mut mapper = mapper(&json_data);
        let input_metric_name = "custom_metric.process.value_1.value_2".to_string();
        let existing_tags = tags();
        let context = mapper.map(&input_metric_name, existing_tags.clone()).unwrap();
        let expected_metric_name = MetaString::from_static("custom_metric.process");
        assert_eq!(context.name(), &expected_metric_name);
        assert!(context.tags().has_tag("tag_key_1:value_1"));
        assert!(context.tags().has_tag("tag_key_2:value_2"));
        for tag in existing_tags {
            assert!(context.tags().has_tag(tag));
        }
    }

    #[test]
    fn test_mapper_regex() {
        let json_data = r#"[{"name":"my_custom_metric_profile","prefix":"custom_metric.","mappings":[{"match":"custom_metric\\.process\\.([\\w_]+)\\.(.+)","match_type":"regex","name":"custom_metric.process","tags":{"tag_key_1":"$1","tag_key_2":"$2"}}]}]"#;
        let mut mapper = mapper(&json_data);
        let input_metric_name = "custom_metric.process.value_1.value.with.dots._2".to_string();
        let existing_tags = tags();
        let context = mapper.map(&input_metric_name, existing_tags.clone()).unwrap();
        let expected_metric_name = MetaString::from_static("custom_metric.process");
        assert_eq!(context.name(), &expected_metric_name);
        assert!(context.tags().has_tag("tag_key_1:value_1"));
        assert!(context.tags().has_tag("tag_key_2:value.with.dots._2"));
        for tag in existing_tags {
            assert!(context.tags().has_tag(tag));
        }
    }

    #[test]
    fn test_mapper_expand_group() {
        let json_data = r#"[{"name":"my_custom_metric_profile","prefix":"custom_metric.","mappings":[{"match":"custom_metric.process.*.*","match_type":"wildcard","name":"custom_metric.process.prod.$1.live","tags":{"tag_key_2":"$2"}}]}]"#;
        let mut mapper = mapper(&json_data);
        let input_metric_name = "custom_metric.process.value_1.value_2".to_string();
        let expected_metric_name = MetaString::from_static("custom_metric.process.prod.value_1.live");
        let existing_tags = tags();
        let context = mapper.map(&input_metric_name, existing_tags.clone()).unwrap();
        assert_eq!(context.name(), &expected_metric_name);
        for tag in existing_tags {
            assert!(context.tags().has_tag(tag));
        }
    }
}
