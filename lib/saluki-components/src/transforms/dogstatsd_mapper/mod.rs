use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::LazyLock;

use async_trait::async_trait;
use bytesize::ByteSize;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use regex::Regex;
use saluki_config::GenericConfiguration;
use saluki_context::{ContextResolver, ContextResolverBuilder};
use saluki_core::{
    components::transforms::{SynchronousTransform, SynchronousTransformBuilder},
    topology::interconnect::FixedSizeEventBuffer,
};
use saluki_error::{generic_error, GenericError};
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr, PickFirst};

pub const MATCH_TYPE_WILDCARD: &str = "wildcard";
pub const MATCH_TYPE_REGEX: &str = "regex";

static ALLOWED_WILDCARD_MATCH_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9\-_*.]+$").expect("Invalid regex in ALLOWED_WILDCARD_MATCH_PATTERN"));

/// DogstatsD mapper transform.
#[serde_as]
#[derive(Deserialize)]
#[allow(dead_code)]
pub struct DogstatsDMapperConfiguration {
    #[serde(skip)]
    context_string_interner_bytes: ByteSize,

    #[serde_as(as = "PickFirst<(DisplayFromStr, _)>")]
    #[serde(default)]
    dogstatsd_mapper_profiles: MapperProfileConfigs,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[allow(dead_code)]
struct MapperProfileConfigs(pub Vec<MappingProfileConfig>);

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[allow(dead_code)]
struct MappingProfileConfig {
    pub name: String,
    pub prefix: String,
    pub mappings: Vec<MetricMappingConfig>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[allow(dead_code)]
struct MetricMappingConfig {
    #[serde(rename = "match")]
    metric_match: String,
    #[serde(rename = "match_type")]
    match_type: String,
    name: String,
    tags: HashMap<String, String>,
}

#[allow(dead_code)]
struct MetricMapper {
    profiles: Vec<MappingProfile>,
    context_resolver: ContextResolver,
}

#[allow(dead_code)]
struct MappingProfile {
    name: String,
    prefix: String,
    mappings: Vec<MetricMapping>,
}

#[allow(dead_code)]
struct MetricMapping {
    name: String,
    tags: HashMap<String, String>,
    regex: Regex,
}

impl FromStr for MapperProfileConfigs {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let profiles: Vec<MappingProfileConfig> = serde_json::from_str(s)?;
        Ok(MapperProfileConfigs(profiles))
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
        // Disallow "**"
        if pattern.contains("**") {
            return Err(generic_error!(
                "invalid wildcard match pattern `{}`, it should not contain consecutive `*`",
                pattern
            ));
        }
        // Escape dots and replace '*' with '([^.]*)'
        pattern = pattern.replace(".", "\\.");
        pattern = pattern.replace("*", "([^.]*)");
    }

    // Build final pattern as ^pattern$
    let final_pattern = format!("^{}$", pattern);

    // Compile the regex, return a GenericError if it fails
    match Regex::new(&final_pattern) {
        Ok(re) => Ok(re),
        Err(e) => Err(generic_error!(
            "invalid match `{}`, cannot compile regex: {}",
            match_re,
            e
        )),
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
                name: config_profile.name.clone(),
                prefix: config_profile.prefix.clone(),
                mappings: Vec::with_capacity(config_profile.mappings.len()),
            };

            for mapping in &config_profile.mappings {
                let mut match_type = mapping.match_type.to_string();
                if mapping.match_type == "" {
                    match_type = "wildcard".to_string();
                }
                if match_type != MATCH_TYPE_WILDCARD && match_type != MATCH_TYPE_REGEX {
                    return Err(generic_error!(
                        "profile: {}, mapping num {}: invalid match type, must be `wildcard` or `regex`",
                        config_profile.name,
                        i
                    ));
                }
                if mapping.name == "" {
                    return Err(generic_error!(
                        "profile: {}, mapping num {}: name is required",
                        config_profile.name,
                        i
                    ));
                }
                if mapping.name == "" {
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
            .ok_or_else(|| generic_error!("context_string_interner_size must be greater than 0"))?;
        let context_resolver = ContextResolverBuilder::from_name("agent_telemetry_remapper")
            .expect("resolver name is not empty")
            .with_interner_capacity_bytes(context_string_interner_size)
            .build();

        Ok(MetricMapper {
            profiles,
            context_resolver,
        })
    }
}

impl DogstatsDMapperConfiguration {
    /// Creates a new `DogstatsDMapperConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let mut config: Self = config.as_typed()?;
        config.context_string_interner_bytes = ByteSize::kib(512);
        Ok(config)
    }
}

#[async_trait]
impl SynchronousTransformBuilder for DogstatsDMapperConfiguration {
    async fn build(&self) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        println!(
            "rz6300 map config: {}",
            serde_json::to_string_pretty(&self.dogstatsd_mapper_profiles).unwrap()
        );
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
            .with_single_value::<DogstatsDMapper>()
            // We also allocate the backing storage for the string interner up front, which is used by our context
            // resolver.
            .with_fixed_amount(self.context_string_interner_bytes.as_u64() as usize);
    }
}

#[allow(dead_code)]
pub struct DogstatsDMapper {
    metric_mapper: MetricMapper,
}

impl DogstatsDMapper {}

impl SynchronousTransform for DogstatsDMapper {
    fn transform_buffer(&self, event_buffer: &mut FixedSizeEventBuffer) {
        for event in event_buffer {
            if let Some(_metric) = event.try_as_metric_mut() {
                println!("rz6300 got metric {} from dogstatsd mapper", _metric.context().name());
            }
        }
    }
}
