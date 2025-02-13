use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::LazyLock;

use async_trait::async_trait;
use bytesize::ByteSize;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use regex::Regex;
use saluki_config::GenericConfiguration;
use saluki_context::{Context, ContextResolver, ContextResolverBuilder};
use saluki_core::{
    components::transforms::{SynchronousTransform, SynchronousTransformBuilder},
    topology::interconnect::FixedSizeEventBuffer,
};
use saluki_error::{generic_error, ErrorContext, GenericError};
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
    /// Total size of the string interner used for contexts, in bytes.
    ///
    /// This controls the amount of memory that will be pre-allocated for the purpose
    /// of interning mapped metric names and tags, which can help to avoid unnecessary
    /// allocations and allocator fragmentation.
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
                let match_type = match mapping.match_type.as_str() {
                    // Default to wildcard when not set.
                    "" => MATCH_TYPE_WILDCARD,
                    MATCH_TYPE_WILDCARD => MATCH_TYPE_WILDCARD,
                    MATCH_TYPE_REGEX => MATCH_TYPE_REGEX,
                    unknown => {
                        return Err(generic_error!(
                            "profile: {}, mapping num {}: invalid match type `{}`, expected `wildcard` or `regex`",
                            config_profile.name,
                            i,
                            unknown,
                        ))
                    }
                };
                if mapping.name.is_empty() {
                    return Err(generic_error!(
                        "profile: {}, mapping num {}: name is required",
                        config_profile.name,
                        i
                    ));
                }
                if mapping.metric_match.is_empty() {
                    return Err(generic_error!(
                        "profile: {}, mapping num {}: match is required",
                        config_profile.name,
                        i
                    ));
                }
                let regex = build_regex(&mapping.metric_match, match_type)?;
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

    Regex::new(&final_pattern).with_error_context(|| {
        format!(
            "Failed to compile regular expression `{}` for `{}` match type",
            final_pattern, match_type
        )
    })
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct MetricMappingConfig {
    // The metric name to extract groups from with the Wildcard or Regex match logic.
    #[serde(rename = "match")]
    metric_match: String,

    // The type of match to apply to the `metric_match`. Either wildcard or regex.
    #[serde(default)]
    match_type: String,

    // The new metric name to send to Datadog with the tags defined in the same group.
    name: String,

    // Map with the tag key and tag values collected from the `match_type` to inline.
    #[serde(default)]
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
    fn try_map(&mut self, context: &Context) -> Option<Context> {
        let metric_name = context.name();
        let tags = context.tags();
        let origin_tags = context.origin_tags();

        for profile in &self.profiles {
            if !metric_name.starts_with(&profile.prefix) && profile.prefix != "*" {
                continue;
            }

            for mapping in &profile.mappings {
                if let Some(captures) = mapping.regex.captures(metric_name) {
                    let mut name = String::new();
                    captures.expand(&mapping.name, &mut name);
                    let mut new_tags: Vec<String> = tags.into_iter().map(|tag| tag.as_str().to_owned()).collect();
                    for (tag_key, tag_value_expr) in &mapping.tags {
                        let mut expanded_value = String::new();
                        captures.expand(tag_value_expr, &mut expanded_value);
                        new_tags.push(format!("{}:{}", tag_key, expanded_value));
                    }
                    return self
                        .context_resolver
                        .resolve_with_origin_tags(&name, new_tags, origin_tags.clone());
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
                if let Some(new_context) = self.metric_mapper.try_map(metric.context()) {
                    *metric.context_mut() = new_context;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use bytesize::ByteSize;
    use saluki_context::Context;
    use saluki_error::GenericError;
    use saluki_event::metric::Metric;
    use serde_json::{json, Value};

    use super::{MapperProfileConfigs, MetricMapper};

    fn counter_metric(name: &'static str, tags: &[&'static str]) -> Metric {
        let context = Context::from_static_parts(name, tags);
        Metric::counter(context, 1.0)
    }

    fn mapper(json_data: Value) -> Result<MetricMapper, GenericError> {
        let mpc: MapperProfileConfigs = serde_json::from_value(json_data)?;
        let context_string_interner_bytes = ByteSize::kib(64);
        mpc.build(context_string_interner_bytes)
    }

    fn assert_tags(context: &Context, expected_tags: &[&str]) {
        for tag in expected_tags {
            assert!(context.tags().has_tag(tag), "missing tag: {}", tag);
        }
        assert_eq!(context.tags().len(), expected_tags.len(), "unexpected number of tags");
    }

    #[test]
    fn test_mapper_wildcard_simple() {
        let json_data = json!([{
          "name": "test",
          "prefix": "test.",
          "mappings": [
            {
              "match": "test.job.duration.*.*",
              "name": "test.job.duration",
              "tags": {
                "job_type": "$1",
                "job_name": "$2"
              }
            },
            {
              "match": "test.job.size.*.*",
              "name": "test.job.size",
              "tags": {
                "foo": "$1",
                "bar": "$2"
              }
            }
          ]
        }]);

        let mut mapper = mapper(json_data).expect("should have parsed mapping config");
        let metric = counter_metric("test.job.duration.my_job_type.my_job_name", &[]);
        let context = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(context.name(), "test.job.duration");
        assert_tags(&context, &["job_type:my_job_type", "job_name:my_job_name"]);

        let metric = counter_metric("test.job.size.my_job_type.my_job_name", &[]);
        let context = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(context.name(), "test.job.size");
        assert_tags(&context, &["foo:my_job_type", "bar:my_job_name"]);

        let metric = counter_metric("test.job.size.not_match", &[]);
        assert!(mapper.try_map(metric.context()).is_none(), "should not have remapped");
    }

    #[test]
    fn test_partial_match() {
        let json_data = json!([{
          "name": "test",
          "prefix": "test.",
          "mappings": [
            {
              "match": "test.job.duration.*.*",
              "name": "test.job.duration",
              "tags": {
                "job_type": "$1"
              }
            },
            {
              "match": "test.task.duration.*.*",
              "name": "test.task.duration",
            }
          ]
        }]);
        let mut mapper = mapper(json_data).expect("should have parsed mapping config");
        let metric = counter_metric("test.job.duration.my_job_type.my_job_name", &[]);
        let context = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(context.name(), "test.job.duration");
        assert!(context.tags().has_tag("job_type:my_job_type"));

        let metric = counter_metric("test.task.duration.my_job_type.my_job_name", &[]);
        let context = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(context.name(), "test.task.duration");
    }

    #[test]
    fn test_use_regex_expansion_alternative_syntax() {
        let json_data = json!([{
            "name": "test",
            "prefix": "test.",
            "mappings": [
                {
                    "match": "test.job.duration.*.*",
                    "name": "test.job.duration",
                    "tags": {
                        "job_type": "${1}_x",
                        "job_name": "${2}_y"
                    }
                }
            ]
        }]);

        let mut mapper = mapper(json_data).expect("should have parsed mapping config");

        let metric = counter_metric("test.job.duration.my_job_type.my_job_name", &[]);
        let context = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(context.name(), "test.job.duration");
        assert_tags(&context, &["job_type:my_job_type_x", "job_name:my_job_name_y"]);
    }

    #[test]
    fn test_expand_name() {
        let json_data = json!([{
            "name": "test",
            "prefix": "test.",
            "mappings": [
                {
                    "match": "test.job.duration.*.*",
                    "name": "test.hello.$2.$1",
                    "tags": {
                        "job_type": "$1",
                        "job_name": "$2"
                    }
                }
            ]
        }]);

        let mut mapper = mapper(json_data).expect("should have parsed mapping config");

        let metric = counter_metric("test.job.duration.my_job_type.my_job_name", &[]);
        let context = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(context.name(), "test.hello.my_job_name.my_job_type");
        assert_tags(&context, &["job_type:my_job_type", "job_name:my_job_name"]);
    }

    #[test]
    fn test_match_before_underscore() {
        let json_data = json!([{
            "name": "test",
            "prefix": "test.",
            "mappings": [
                {
                    "match": "test.*_start",
                    "name": "test.start",
                    "tags": {
                        "job": "$1"
                    }
                }
            ]
        }]);

        let mut mapper = mapper(json_data).expect("should have parsed mapping config");

        let metric = counter_metric("test.my_job_start", &[]);
        let context = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(context.name(), "test.start");
        assert!(context.tags().has_tag("job:my_job"));
    }

    #[test]
    fn test_no_tags() {
        let json_data = json!([{
            "name": "test",
            "prefix": "test.",
            "mappings": [
                {
                    "match": "test.my-worker.start",
                    "name": "test.worker.start"
                },
                {
                    "match": "test.my-worker.stop.*",
                    "name": "test.worker.stop"
                }
            ]
        }]);

        let mut mapper = mapper(json_data).expect("should have parsed mapping config");

        let metric = counter_metric("test.my-worker.start", &[]);
        let context = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(context.name(), "test.worker.start");
        assert!(context.tags().is_empty(), "Expected no tags");

        let metric = counter_metric("test.my-worker.stop.worker-name", &[]);
        let context = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(context.name(), "test.worker.stop");
        assert!(context.tags().is_empty(), "Expected no tags");
    }

    #[test]
    fn test_all_allowed_characters() {
        let json_data = json!([{
            "name": "test",
            "prefix": "test.",
            "mappings": [
                {
                    "match": "test.abcdefghijklmnopqrstuvwxyz_ABCDEFGHIJKLMNOPQRSTUVWXYZ-01234567.*",
                    "name": "test.alphabet"
                }
            ]
        }]);

        let mut mapper = mapper(json_data).expect("should have parsed mapping config");

        let metric = counter_metric(
            "test.abcdefghijklmnopqrstuvwxyz_ABCDEFGHIJKLMNOPQRSTUVWXYZ-01234567.123",
            &[],
        );
        let context = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(context.name(), "test.alphabet");
        assert!(context.tags().is_empty(), "Expected no tags");
    }

    #[test]
    fn test_regex_match_type() {
        let json_data = json!([{
            "name": "test",
            "prefix": "test.",
            "mappings": [
                {
                    "match": "test\\.job\\.duration\\.(.*)",
                    "match_type": "regex",
                    "name": "test.job.duration",
                    "tags": {
                        "job_name": "$1"
                    }
                },
                {
                    "match": "test\\.task\\.duration\\.(.*)",
                    "match_type": "regex",
                    "name": "test.task.duration",
                    "tags": {
                        "task_name": "$1"
                    }
                }
            ]
        }]);

        let mut mapper = mapper(json_data).expect("should have parsed mapping config");
        let metric = counter_metric("test.job.duration.my.funky.job$name-abc/123", &[]);
        let context = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(context.name(), "test.job.duration");
        assert!(context.tags().has_tag("job_name:my.funky.job$name-abc/123"));

        let metric = counter_metric("test.task.duration.MY_task_name", &[]);
        let context = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(context.name(), "test.task.duration");
        assert!(context.tags().has_tag("task_name:MY_task_name"));
    }

    #[test]
    fn test_complex_regex_match_type() {
        let json_data = json!([{
            "name": "test",
            "prefix": "test.",
            "mappings": [
                {
                    "match": "test\\.job\\.([a-z][0-9]-\\w+)\\.(.*)",
                    "match_type": "regex",
                    "name": "test.job",
                    "tags": {
                        "job_type": "$1",
                        "job_name": "$2"
                    }
                }
            ]
        }]);

        let mut mapper = mapper(json_data).expect("should have parsed mapping config");

        let metric = counter_metric("test.job.a5-foo.bar", &[]);
        let context = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(context.name(), "test.job");
        assert_tags(&context, &["job_type:a5-foo", "job_name:bar"]);

        let metric = counter_metric("test.job.foo.bar-not-match", &[]);
        assert!(mapper.try_map(metric.context()).is_none(), "should not have remapped");
    }

    #[test]
    fn test_profile_and_prefix() {
        let json_data = json!([{
            "name": "test",
            "prefix": "foo.",
            "mappings": [
                {
                    "match": "foo.duration.*",
                    "name": "foo.duration",
                    "tags": {
                        "name": "$1"
                    }
                }
            ]
        },
        {
            "name": "test",
            "prefix": "bar.",
            "mappings": [
                {
                    "match": "bar.count.*",
                    "name": "bar.count",
                    "tags": {
                        "name": "$1"
                    }
                },
                {
                    "match": "foo.duration2.*",
                    "name": "foo.duration2",
                    "tags": {
                        "name": "$1"
                    }
                }
            ]
        }]);

        let mut mapper = mapper(json_data).expect("should have parsed mapping config");

        let metric = counter_metric("foo.duration.foo_name1", &[]);
        let context = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(context.name(), "foo.duration");
        assert!(context.tags().has_tag("name:foo_name1"));

        let metric = counter_metric("foo.duration2.foo_name1", &[]);
        assert!(
            mapper.try_map(metric.context()).is_none(),
            "should not have remapped due to wrong group"
        );

        let metric = counter_metric("bar.count.bar_name1", &[]);
        let context = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(context.name(), "bar.count");
        assert!(context.tags().has_tag("name:bar_name1"));

        let metric = counter_metric("z.not.mapped", &[]);
        assert!(mapper.try_map(metric.context()).is_none(), "should not have remapped");
    }

    #[test]
    fn test_wildcard_prefix() {
        let json_data = json!([{
            "name": "test",
            "prefix": "*",
            "mappings": [
                {
                    "match": "foo.duration.*",
                    "name": "foo.duration",
                    "tags": {
                        "name": "$1"
                    }
                }
            ]
        }]);

        let mut mapper = mapper(json_data).expect("should have parsed mapping config");

        let metric = counter_metric("foo.duration.foo_name1", &[]);
        let context = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(context.name(), "foo.duration");
        assert!(context.tags().has_tag("name:foo_name1"));
    }

    #[test]
    fn test_wildcard_prefix_order() {
        let json_data = json!([{
            "name": "test",
            "prefix": "*",
            "mappings": [
                {
                    "match": "foo.duration.*",
                    "name": "foo.duration",
                    "tags": {
                        "name1": "$1"
                    }
                }
            ]
        },
        {
            "name": "test",
            "prefix": "*",
            "mappings": [
                {
                    "match": "foo.duration.*",
                    "name": "foo.duration",
                    "tags": {
                        "name2": "$1"
                    }
                }
            ]
        }]);

        let mut mapper = mapper(json_data).expect("should have parsed mapping config");
        let metric = counter_metric("foo.duration.foo_name", &[]);
        let context = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(context.name(), "foo.duration");
        assert!(context.tags().has_tag("name1:foo_name"));
        assert!(
            !context.tags().has_tag("name2:foo_name"),
            "Only the first matching profile should apply"
        );
    }

    #[test]
    fn test_multiple_profiles_order() {
        let json_data = json!([{
            "name": "test",
            "prefix": "foo.",
            "mappings": [
                {
                    "match": "foo.*.duration.*",
                    "name": "foo.bar1.duration",
                    "tags": {
                        "bar": "$1",
                        "foo": "$2"
                    }
                }
            ]
        },
        {
            "name": "test",
            "prefix": "foo.bar.",
            "mappings": [
                {
                    "match": "foo.bar.duration.*",
                    "name": "foo.bar2.duration",
                    "tags": {
                        "foo_bar": "$1"
                    }
                }
            ]
        }]);

        let mut mapper = mapper(json_data).expect("should have parsed mapping config");

        let metric = counter_metric("foo.bar.duration.foo_name", &[]);
        let context = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(context.name(), "foo.bar1.duration");
        assert_tags(&context, &["bar:bar", "foo:foo_name"]);
        assert!(
            !context.tags().has_tag("foo_bar:foo_name"),
            "Only the first matching profile should apply"
        );
    }

    #[test]
    fn test_different_regex_expansion_syntax() {
        let json_data = json!([{
            "name": "test",
            "prefix": "test.",
            "mappings": [
                {
                    "match": "test.user.(\\w+).action.(\\w+)",
                    "match_type": "regex",
                    "name": "test.user.action",
                    "tags": {
                        "user": "$1",
                        "action": "$2"
                    }
                }
            ]
        }]);

        let mut mapper = mapper(json_data).expect("should have parsed mapping config");
        let metric = counter_metric("test.user.john_doe.action.login", &[]);
        let context = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(context.name(), "test.user.action");
        assert_tags(&context, &["user:john_doe", "action:login"]);
    }

    #[test]
    fn test_retain_existing_tags() {
        let json_data = json!([{
          "name": "test",
          "prefix": "test.",
          "mappings": [
            {
              "match": "test.job.duration.*.*",
              "name": "test.job.duration.$2",
              "tags": {
                "job_type": "$1",
                "job_name": "$2"
              }
            },
          ]
        }]);
        let mut mapper = mapper(json_data).expect("should have parsed mapping config");
        let metric = counter_metric("test.job.duration.abc.def", &["foo:bar", "baz"]);
        let context = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(context.name(), "test.job.duration.def");
        assert!(context.tags().has_tag("foo:bar"));
        assert!(context.tags().has_tag("baz"));
    }

    #[test]
    fn test_empty_name() {
        let json_data = json!([{
            "name": "test",
            "prefix": "test.",
            "mappings": [
                {
                    "match": "test.job.duration.*.*",
                    "name": "",
                    "tags": {
                        "job_type": "$1"
                    }
                }
            ]
        }]);
        assert!(mapper(json_data).is_err())
    }

    #[test]
    fn test_missing_name() {
        let json_data = json!([{
            "name": "test",
            "prefix": "test.",
            "mappings": [
                {
                    "match": "test.job.duration.*.*",
                    "tags": {
                        "job_type": "$1",
                        "job_name": "$2"
                    }
                }
            ]
        }]);
        assert!(mapper(json_data).is_err());
    }

    #[test]
    fn test_invalid_match_regex_brackets() {
        let json_data = json!([{
            "name": "test",
            "prefix": "test.",
            "mappings": [
                {
                    "match": "test.[]duration.*.*", // Invalid regex
                    "name": "test.job.duration"
                }
            ]
        }]);
        assert!(mapper(json_data).is_err());
    }

    #[test]
    fn test_invalid_match_regex_caret() {
        let json_data = json!([{
            "name": "test",
            "prefix": "test.",
            "mappings": [
                {
                    "match": "^test.invalid.duration.*.*", // Invalid regex
                    "name": "test.job.duration"
                }
            ]
        }]);
        assert!(mapper(json_data).is_err());
    }

    #[test]
    fn test_consecutive_wildcards() {
        let json_data = json!([{
            "name": "test",
            "prefix": "test.",
            "mappings": [
                {
                    "match": "test.invalid.duration.**", // Consecutive *
                    "name": "test.job.duration"
                }
            ]
        }]);
        assert!(mapper(json_data).is_err());
    }

    #[test]
    fn test_invalid_match_type() {
        let json_data = json!([{
            "name": "test",
            "prefix": "test.",
            "mappings": [
                {
                    "match": "test.invalid.duration",
                    "match_type": "invalid", // Invalid match_type
                    "name": "test.job.duration"
                }
            ]
        }]);
        assert!(mapper(json_data).is_err());
    }

    #[test]
    fn test_missing_profile_name() {
        let json_data = json!([{
            // "name" is missing here
            "prefix": "test.",
            "mappings": [
                {
                    "match": "test.invalid.duration",
                    "match_type": "invalid",
                    "name": "test.job.duration"
                }
            ]
        }]);
        assert!(mapper(json_data).is_err());
    }

    #[test]
    fn test_missing_profile_prefix() {
        let json_data = json!([{
            "name": "test",
            // "prefix" is missing here
            "mappings": [
                {
                    "match": "test.invalid.duration",
                    "match_type": "invalid",
                    "name": "test.job.duration"
                }
            ]
        }]);
        assert!(mapper(json_data).is_err());
    }
}
