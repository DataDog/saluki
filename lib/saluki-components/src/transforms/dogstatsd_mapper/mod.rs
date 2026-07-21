use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use bytesize::ByteSize;
use regex::Regex;
use saluki_common::cache::{Cache, CacheBuilder};
use saluki_config::GenericConfiguration;
use saluki_context::tags::SharedTagSet;
use saluki_context::tags::TagSet;
use saluki_context::{Context, ContextResolver, ContextResolverBuilder};
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{
        transforms::{SynchronousTransform, SynchronousTransformBuilder},
        ComponentContext,
    },
    topology::EventsBuffer,
};
use saluki_error::{generic_error, ErrorContext, GenericError};
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr, PickFirst};
use stringtheory::MetaString;

const MATCH_TYPE_WILDCARD: &str = "wildcard";
const MATCH_TYPE_REGEX: &str = "regex";

static ALLOWED_WILDCARD_MATCH_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9\-_*.]+$").expect("Invalid regex in ALLOWED_WILDCARD_MATCH_PATTERN"));

const fn default_context_string_interner_size() -> ByteSize {
    ByteSize::kib(64)
}

const fn default_dogstatsd_mapper_cache_size() -> usize {
    1000
}
/// DogStatsD mapper transform.
#[serde_as]
#[derive(Deserialize)]
#[cfg_attr(test, derive(Debug, PartialEq, serde::Serialize))]
pub struct DogStatsDMapperConfiguration {
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

    /// Maximum number of mapped results to cache.
    ///
    /// When enabled, mapped metrics will be cached by name to avoid repeat evaluation of the configured mapper rules.
    ///
    /// When set to `0`, the cache is disabled.
    ///
    /// Defaults to `1000`.
    #[serde(
        rename = "dogstatsd_mapper_cache_size",
        default = "default_dogstatsd_mapper_cache_size"
    )]
    cache_size: usize,

    /// Configuration related to metric mapping.
    #[serde_as(as = "PickFirst<(DisplayFromStr, _)>")]
    #[serde(default)]
    dogstatsd_mapper_profiles: MapperProfileConfigs,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
struct MappingProfileConfig {
    name: String,
    prefix: String,
    mappings: Vec<MetricMappingConfig>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
struct MapperProfileConfigs(pub Vec<MappingProfileConfig>);

impl FromStr for MapperProfileConfigs {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let profiles: Vec<MappingProfileConfig> = serde_json::from_str(s)?;
        Ok(MapperProfileConfigs(profiles))
    }
}

#[cfg(test)]
impl std::fmt::Display for MapperProfileConfigs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", serde_json::to_string(&self.0).unwrap_or_default())
    }
}

impl MapperProfileConfigs {
    fn build(
        &self, context: ComponentContext, context_string_interner_bytes: ByteSize, cache_size: usize,
    ) -> Result<MetricMapper, GenericError> {
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

        let context_resolver =
            ContextResolverBuilder::from_name(format!("{}/dsd_mapper/primary", context.component_id()))
                .expect("resolver name is not empty")
                .with_interner_capacity_bytes(context_string_interner_size)
                .with_idle_context_expiration(Duration::from_secs(30))
                .build();

        let cache = match NonZeroUsize::new(cache_size) {
            Some(capacity) => Some(
                CacheBuilder::from_identifier(format!("{}/dsd_mapper/result_cache", context.component_id()))?
                    .with_capacity(capacity)
                    .build(),
            ),
            None => None,
        };

        Ok(MetricMapper {
            context_resolver,
            profiles,
            cache,
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

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
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

#[derive(Clone)]
struct CachedMapResult {
    name: MetaString,
    extra_tags: SharedTagSet,
}

struct MetricMapper {
    profiles: Vec<MappingProfile>,
    context_resolver: ContextResolver,
    cache: Option<Cache<MetaString, Option<CachedMapResult>>>,
}

impl MetricMapper {
    fn try_map(&mut self, context: &Context) -> Option<Context> {
        // TODO: We should really be able to immutably borrow both the incoming tag set and the cached extra tags and
        // chain them together for our call into `resolve_with_origin_tags`, avoiding any allocations... but we need
        // some supporting work on the `TagSet` side to make it possible.

        let metric_name = context.name();
        let tags = context.tags();
        let origin_tags = context.origin_tags();
        // TODO: If host-bearing remaps show measurable allocation overhead, preserve the context's underlying host
        // representation through the resolver instead of rematerializing it from `&str`.
        let host = context.host();

        // See if we have a cached result for this metric name.
        if let Some(cache) = &self.cache {
            if let Some(cached) = cache.get(metric_name) {
                return match cached {
                    None => None,
                    Some(result) => {
                        let mut merged_tags = tags.clone();
                        merged_tags.merge_shared(&result.extra_tags);

                        self.context_resolver.resolve_with_optional_host_and_origin_tags(
                            result.name.clone(),
                            host,
                            merged_tags,
                            origin_tags.clone(),
                        )
                    }
                };
            }
        }

        // Slow path: iterate profiles and run regexes.
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

                    // Freeze the tags here so they can be shared / cached.
                    let extra_tags = extra_tags.into_shared();

                    let mut merged_tags = tags.clone();
                    merged_tags.merge_shared(&extra_tags);

                    let resolved = self.context_resolver.resolve_with_optional_host_and_origin_tags(
                        new_name.as_str(),
                        host,
                        merged_tags,
                        origin_tags.clone(),
                    )?;

                    if let Some(cache) = &self.cache {
                        cache.insert(
                            metric_name.clone(),
                            Some(CachedMapResult {
                                name: resolved.name().clone(),
                                extra_tags,
                            }),
                        );
                    }
                    return Some(resolved);
                }
            }
        }

        // We also cache "negative" results -- no match for this metric in the configured profiles -- to save ourselves some work.
        if let Some(cache) = &self.cache {
            cache.insert(metric_name.clone(), None);
        }
        None
    }

    #[cfg(test)]
    fn cache_len(&self) -> Option<usize> {
        self.cache.as_ref().map(|c| c.len())
    }

    #[cfg(test)]
    fn cache_contains(&self, metric_name: &str) -> bool {
        self.cache
            .as_ref()
            .is_some_and(|c| c.get(&MetaString::from(metric_name)).is_some())
    }
}

impl DogStatsDMapperConfiguration {
    /// Creates a new `DogstatsDMapperConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }
}

#[async_trait]
impl SynchronousTransformBuilder for DogStatsDMapperConfiguration {
    async fn build(&self, context: ComponentContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        let metric_mapper =
            self.dogstatsd_mapper_profiles
                .build(context, self.context_string_interner_bytes, self.cache_size)?;
        Ok(Box::new(DogStatsDMapper { metric_mapper }))
    }
}

impl MemoryBounds for DogStatsDMapperConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        let mut min = builder.minimum();
        min
            // Capture the size of the heap allocation when the component is built.
            .with_single_value::<DogStatsDMapper>("component struct")
            // We also allocate the backing storage for the string interner up front, which is used by our context
            // resolver.
            .with_fixed_amount("string interner", self.context_string_interner_bytes.as_u64() as usize);

        // Account for the per-name result cache when enabled.
        if self.cache_size > 0 {
            min.with_array::<(MetaString, Option<CachedMapResult>)>("mapper result cache", self.cache_size);
        }
    }
}

pub struct DogStatsDMapper {
    metric_mapper: MetricMapper,
}

impl SynchronousTransform for DogStatsDMapper {
    fn transform_buffer(&mut self, event_buffer: &mut EventsBuffer) {
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
    use saluki_context::{Context, ContextResolverBuilder};
    use saluki_core::{
        components::{transforms::SynchronousTransform, ComponentContext},
        data_model::event::{metric::Metric, Event},
        topology::EventsBuffer,
    };
    use saluki_error::GenericError;
    use serde_json::{json, Value};

    use super::{DogStatsDMapper, MapperProfileConfigs, MetricMapper};

    fn counter_metric(name: &'static str, tags: &[&'static str]) -> Metric {
        let context = Context::from_static_parts(name, tags);
        Metric::counter(context, 1.0)
    }

    fn mapper(json_data: Value) -> Result<MetricMapper, GenericError> {
        mapper_with_cache(json_data, 1000)
    }

    fn mapper_with_cache(json_data: Value, cache_size: usize) -> Result<MetricMapper, GenericError> {
        let context = ComponentContext::test_transform("test_mapper");
        let mpc: MapperProfileConfigs = serde_json::from_value(json_data)?;
        let context_string_interner_bytes = ByteSize::kib(64);
        mpc.build(context, context_string_interner_bytes, cache_size)
    }

    fn assert_tags(context: &Context, expected_tags: &[&str]) {
        for tag in expected_tags {
            assert!(context.tags().has_tag(tag), "missing tag: {}", tag);
        }
        assert_eq!(context.tags().len(), expected_tags.len(), "unexpected number of tags");
    }

    #[track_caller]
    fn assert_tags_for_case(context: &Context, expected_tags: &[&str], case: &str, input: &str) {
        for tag in expected_tags {
            assert!(
                context.tags().has_tag(tag),
                "[{case}] input {input:?}: missing tag {tag:?}"
            );
        }
        assert_eq!(
            context.tags().len(),
            expected_tags.len(),
            "[{case}] input {input:?}: unexpected number of tags"
        );
    }

    fn simple_mapping_profile() -> Value {
        json!([{
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
                }
            ]
        }])
    }

    #[tokio::test]
    async fn config_driven_mappings_produce_expected_output() {
        // Each case builds one mapper from `config`, then checks a series of inputs against it. A check is
        // `(input_name, input_tags, expected)`, where `expected` is `Some((mapped_name, mapped_tags))` when the metric
        // should be remapped, or `None` when it must pass through unmapped.
        struct MapperCase {
            description: &'static str,
            config: Value,
            #[allow(clippy::type_complexity)]
            checks: Vec<(
                &'static str,
                &'static [&'static str],
                Option<(&'static str, &'static [&'static str])>,
            )>,
        }

        let cases = vec![
            MapperCase {
                description: "wildcard mappings with capture-group tags",
                config: json!([{
                    "name": "test",
                    "prefix": "test.",
                    "mappings": [
                        { "match": "test.job.duration.*.*", "name": "test.job.duration", "tags": { "job_type": "$1", "job_name": "$2" } },
                        { "match": "test.job.size.*.*", "name": "test.job.size", "tags": { "foo": "$1", "bar": "$2" } }
                    ]
                }]),
                checks: vec![
                    (
                        "test.job.duration.my_job_type.my_job_name",
                        &[],
                        Some(("test.job.duration", &["job_type:my_job_type", "job_name:my_job_name"])),
                    ),
                    (
                        "test.job.size.my_job_type.my_job_name",
                        &[],
                        Some(("test.job.size", &["foo:my_job_type", "bar:my_job_name"])),
                    ),
                    ("test.job.size.not_match", &[], None),
                ],
            },
            MapperCase {
                description: "partial mapping, second mapping has no tags",
                config: json!([{
                    "name": "test",
                    "prefix": "test.",
                    "mappings": [
                        { "match": "test.job.duration.*.*", "name": "test.job.duration", "tags": { "job_type": "$1" } },
                        { "match": "test.task.duration.*.*", "name": "test.task.duration" }
                    ]
                }]),
                checks: vec![
                    (
                        "test.job.duration.my_job_type.my_job_name",
                        &[],
                        Some(("test.job.duration", &["job_type:my_job_type"])),
                    ),
                    (
                        "test.task.duration.my_job_type.my_job_name",
                        &[],
                        Some(("test.task.duration", &[])),
                    ),
                ],
            },
            MapperCase {
                description: "regex expansion with ${n} syntax",
                config: json!([{
                    "name": "test",
                    "prefix": "test.",
                    "mappings": [
                        { "match": "test.job.duration.*.*", "name": "test.job.duration", "tags": { "job_type": "${1}_x", "job_name": "${2}_y" } }
                    ]
                }]),
                checks: vec![(
                    "test.job.duration.my_job_type.my_job_name",
                    &[],
                    Some((
                        "test.job.duration",
                        &["job_type:my_job_type_x", "job_name:my_job_name_y"],
                    )),
                )],
            },
            MapperCase {
                description: "capture groups expanded into the metric name",
                config: json!([{
                    "name": "test",
                    "prefix": "test.",
                    "mappings": [
                        { "match": "test.job.duration.*.*", "name": "test.hello.$2.$1", "tags": { "job_type": "$1", "job_name": "$2" } }
                    ]
                }]),
                checks: vec![(
                    "test.job.duration.my_job_type.my_job_name",
                    &[],
                    Some((
                        "test.hello.my_job_name.my_job_type",
                        &["job_type:my_job_type", "job_name:my_job_name"],
                    )),
                )],
            },
            MapperCase {
                description: "wildcard matches a segment before an underscore",
                config: json!([{
                    "name": "test",
                    "prefix": "test.",
                    "mappings": [
                        { "match": "test.*_start", "name": "test.start", "tags": { "job": "$1" } }
                    ]
                }]),
                checks: vec![("test.my_job_start", &[], Some(("test.start", &["job:my_job"])))],
            },
            MapperCase {
                description: "mappings without any tags",
                config: json!([{
                    "name": "test",
                    "prefix": "test.",
                    "mappings": [
                        { "match": "test.my-worker.start", "name": "test.worker.start" },
                        { "match": "test.my-worker.stop.*", "name": "test.worker.stop" }
                    ]
                }]),
                checks: vec![
                    ("test.my-worker.start", &[], Some(("test.worker.start", &[]))),
                    ("test.my-worker.stop.worker-name", &[], Some(("test.worker.stop", &[]))),
                ],
            },
            MapperCase {
                description: "all allowed wildcard characters",
                config: json!([{
                    "name": "test",
                    "prefix": "test.",
                    "mappings": [
                        { "match": "test.abcdefghijklmnopqrstuvwxyz_ABCDEFGHIJKLMNOPQRSTUVWXYZ-01234567.*", "name": "test.alphabet" }
                    ]
                }]),
                checks: vec![(
                    "test.abcdefghijklmnopqrstuvwxyz_ABCDEFGHIJKLMNOPQRSTUVWXYZ-01234567.123",
                    &[],
                    Some(("test.alphabet", &[])),
                )],
            },
            MapperCase {
                description: "regex match type",
                config: json!([{
                    "name": "test",
                    "prefix": "test.",
                    "mappings": [
                        { "match": "test\\.job\\.duration\\.(.*)", "match_type": "regex", "name": "test.job.duration", "tags": { "job_name": "$1" } },
                        { "match": "test\\.task\\.duration\\.(.*)", "match_type": "regex", "name": "test.task.duration", "tags": { "task_name": "$1" } }
                    ]
                }]),
                checks: vec![
                    (
                        "test.job.duration.my.funky.job$name-abc/123",
                        &[],
                        Some(("test.job.duration", &["job_name:my.funky.job$name-abc/123"])),
                    ),
                    (
                        "test.task.duration.MY_task_name",
                        &[],
                        Some(("test.task.duration", &["task_name:MY_task_name"])),
                    ),
                ],
            },
            MapperCase {
                description: "complex regex match type",
                config: json!([{
                    "name": "test",
                    "prefix": "test.",
                    "mappings": [
                        { "match": "test\\.job\\.([a-z][0-9]-\\w+)\\.(.*)", "match_type": "regex", "name": "test.job", "tags": { "job_type": "$1", "job_name": "$2" } }
                    ]
                }]),
                checks: vec![
                    (
                        "test.job.a5-foo.bar",
                        &[],
                        Some(("test.job", &["job_type:a5-foo", "job_name:bar"])),
                    ),
                    ("test.job.foo.bar-not-match", &[], None),
                ],
            },
            MapperCase {
                description: "multiple profiles matched by prefix",
                config: json!([
                    {
                        "name": "test",
                        "prefix": "foo.",
                        "mappings": [ { "match": "foo.duration.*", "name": "foo.duration", "tags": { "name": "$1" } } ]
                    },
                    {
                        "name": "test",
                        "prefix": "bar.",
                        "mappings": [
                            { "match": "bar.count.*", "name": "bar.count", "tags": { "name": "$1" } },
                            { "match": "foo.duration2.*", "name": "foo.duration2", "tags": { "name": "$1" } }
                        ]
                    }
                ]),
                checks: vec![
                    (
                        "foo.duration.foo_name1",
                        &[],
                        Some(("foo.duration", &["name:foo_name1"])),
                    ),
                    // `foo.duration2` only exists under the `bar.` prefix, so it can't be reached by a `foo.` metric.
                    ("foo.duration2.foo_name1", &[], None),
                    ("bar.count.bar_name1", &[], Some(("bar.count", &["name:bar_name1"]))),
                    ("z.not.mapped", &[], None),
                ],
            },
            MapperCase {
                description: "wildcard prefix matches any metric",
                config: json!([{
                    "name": "test",
                    "prefix": "*",
                    "mappings": [ { "match": "foo.duration.*", "name": "foo.duration", "tags": { "name": "$1" } } ]
                }]),
                checks: vec![(
                    "foo.duration.foo_name1",
                    &[],
                    Some(("foo.duration", &["name:foo_name1"])),
                )],
            },
            MapperCase {
                description: "only the first matching wildcard-prefixed profile applies",
                config: json!([
                    {
                        "name": "test",
                        "prefix": "*",
                        "mappings": [ { "match": "foo.duration.*", "name": "foo.duration", "tags": { "name1": "$1" } } ]
                    },
                    {
                        "name": "test",
                        "prefix": "*",
                        "mappings": [ { "match": "foo.duration.*", "name": "foo.duration", "tags": { "name2": "$1" } } ]
                    }
                ]),
                // The single expected tag (and exact tag count) proves the second profile's `name2` tag was not applied.
                checks: vec![(
                    "foo.duration.foo_name",
                    &[],
                    Some(("foo.duration", &["name1:foo_name"])),
                )],
            },
            MapperCase {
                description: "only the first matching profile applies across differing prefixes",
                config: json!([
                    {
                        "name": "test",
                        "prefix": "foo.",
                        "mappings": [ { "match": "foo.*.duration.*", "name": "foo.bar1.duration", "tags": { "bar": "$1", "foo": "$2" } } ]
                    },
                    {
                        "name": "test",
                        "prefix": "foo.bar.",
                        "mappings": [ { "match": "foo.bar.duration.*", "name": "foo.bar2.duration", "tags": { "foo_bar": "$1" } } ]
                    }
                ]),
                // The exact tag count proves the second profile's `foo_bar` tag was not applied.
                checks: vec![(
                    "foo.bar.duration.foo_name",
                    &[],
                    Some(("foo.bar1.duration", &["bar:bar", "foo:foo_name"])),
                )],
            },
            MapperCase {
                description: "regex expansion with (\\w+) groups",
                config: json!([{
                    "name": "test",
                    "prefix": "test.",
                    "mappings": [
                        { "match": "test.user.(\\w+).action.(\\w+)", "match_type": "regex", "name": "test.user.action", "tags": { "user": "$1", "action": "$2" } }
                    ]
                }]),
                checks: vec![(
                    "test.user.john_doe.action.login",
                    &[],
                    Some(("test.user.action", &["user:john_doe", "action:login"])),
                )],
            },
            MapperCase {
                description: "existing metric tags are retained alongside mapped tags",
                config: json!([{
                    "name": "test",
                    "prefix": "test.",
                    "mappings": [
                        { "match": "test.job.duration.*.*", "name": "test.job.duration.$2", "tags": { "job_type": "$1", "job_name": "$2" } }
                    ]
                }]),
                checks: vec![(
                    "test.job.duration.abc.def",
                    &["foo:bar", "baz"],
                    Some((
                        "test.job.duration.def",
                        &["foo:bar", "baz", "job_type:abc", "job_name:def"],
                    )),
                )],
            },
        ];

        for case in cases {
            let mut mapper = mapper(case.config)
                .unwrap_or_else(|e| panic!("[{}] config should parse and build: {e}", case.description));

            for (input_name, input_tags, expected) in case.checks {
                let metric = counter_metric(input_name, input_tags);
                match (mapper.try_map(metric.context()), expected) {
                    (Some(context), Some((expected_name, expected_tags))) => {
                        assert_eq!(
                            context.name(),
                            expected_name,
                            "[{}] wrong mapped name for input {input_name:?}",
                            case.description
                        );
                        assert_tags_for_case(&context, expected_tags, case.description, input_name);
                    }
                    (None, None) => {}
                    (mapped, expected) => panic!(
                        "[{}] input {input_name:?}: expected remap={}, got remap={}",
                        case.description,
                        expected.is_some(),
                        mapped.is_some()
                    ),
                }
            }
        }
    }

    #[test]
    fn invalid_mapper_configurations_are_rejected() {
        // Each case is `(description, config, expected_error_substring)`. The empty-field cases exercise
        // `MapperProfileConfigs::build`'s custom validation, which is only reachable via *present-but-empty* fields:
        // missing fields are rejected earlier by serde (the required-field cases below), because the corresponding
        // config fields have no `#[serde(default)]`.
        let cases: Vec<(&str, Value, &str)> = vec![
            // Custom (present-but-empty) validation branches.
            (
                "profile with an empty name",
                json!([{ "name": "", "prefix": "test.", "mappings": [] }]),
                "missing profile name",
            ),
            (
                "profile with an empty prefix",
                json!([{ "name": "test", "prefix": "", "mappings": [] }]),
                "missing prefix for profile: test",
            ),
            (
                "mapping with an empty match",
                json!([{ "name": "test", "prefix": "test.", "mappings": [{ "match": "", "name": "test.mapped" }] }]),
                "match is required",
            ),
            (
                "mapping with an empty name",
                json!([{ "name": "test", "prefix": "test.", "mappings": [{ "match": "test.job.duration.*.*", "name": "", "tags": { "job_type": "$1" } }] }]),
                "name is required",
            ),
            // serde required-field rejection (missing fields short-circuit before the custom validation).
            (
                "mapping missing its name field",
                json!([{ "name": "test", "prefix": "test.", "mappings": [{ "match": "test.job.duration.*.*", "tags": { "job_type": "$1", "job_name": "$2" } }] }]),
                "missing field `name`",
            ),
            (
                "profile missing its name field",
                json!([{ "prefix": "test.", "mappings": [{ "match": "test.invalid.duration", "name": "test.job.duration" }] }]),
                "missing field `name`",
            ),
            (
                "profile missing its prefix field",
                json!([{ "name": "test", "mappings": [{ "match": "test.invalid.duration", "name": "test.job.duration" }] }]),
                "missing field `prefix`",
            ),
            // Match compilation / type validation.
            (
                "wildcard match with disallowed characters",
                json!([{ "name": "test", "prefix": "test.", "mappings": [{ "match": "test.[]duration.*.*", "name": "test.job.duration" }] }]),
                "does not match allowed match regex",
            ),
            (
                "wildcard match anchored with a caret",
                json!([{ "name": "test", "prefix": "test.", "mappings": [{ "match": "^test.invalid.duration.*.*", "name": "test.job.duration" }] }]),
                "does not match allowed match regex",
            ),
            (
                "wildcard match with consecutive wildcards",
                json!([{ "name": "test", "prefix": "test.", "mappings": [{ "match": "test.invalid.duration.**", "name": "test.job.duration" }] }]),
                "consecutive",
            ),
            (
                "unknown match type",
                json!([{ "name": "test", "prefix": "test.", "mappings": [{ "match": "test.invalid.duration", "match_type": "invalid", "name": "test.job.duration" }] }]),
                "invalid match type",
            ),
        ];

        for (description, config, expected_substring) in cases {
            let err = mapper(config)
                .err()
                .unwrap_or_else(|| panic!("[{description}] configuration should be rejected"));
            let message = err.to_string();
            assert!(
                message.contains(expected_substring),
                "[{description}] error {message:?} should contain {expected_substring:?}"
            );
        }
    }

    #[tokio::test]
    async fn transform_buffer_remaps_matching_metrics_and_passes_others_through() {
        // Drives the public `SynchronousTransform::transform_buffer` entry point (every other test exercises the
        // internal `try_map`). Matching metrics are remapped in place; non-matching metrics pass through untouched.
        let mut transform = DogStatsDMapper {
            metric_mapper: mapper(simple_mapping_profile()).expect("config should parse and build"),
        };

        let mut events = EventsBuffer::default();
        assert!(events
            .try_push(Event::Metric(counter_metric("test.job.duration.my_type.my_name", &[])))
            .is_none());
        assert!(events
            .try_push(Event::Metric(counter_metric("unrelated.metric", &["keep:me"])))
            .is_none());

        transform.transform_buffer(&mut events);

        let metrics: Vec<Metric> = events.into_iter().filter_map(Event::try_into_metric).collect();
        assert_eq!(metrics.len(), 2);

        // The matching metric is remapped in place (order is preserved).
        assert_eq!(metrics[0].context().name(), "test.job.duration");
        assert_tags(metrics[0].context(), &["job_type:my_type", "job_name:my_name"]);

        // The non-matching metric is left untouched.
        assert_eq!(metrics[1].context().name(), "unrelated.metric");
        assert_tags(metrics[1].context(), &["keep:me"]);
    }

    #[tokio::test]
    async fn mapper_preserves_host_context_dimension() {
        let json_data = json!([{
          "name": "test",
          "prefix": "test.",
          "mappings": [
            {
              "match": "test.job.duration.*",
              "name": "test.job.duration",
              "tags": {
                "job_name": "$1"
              }
            }
          ]
        }]);

        let mut resolver = ContextResolverBuilder::for_tests().build();
        let context_a = resolver
            .resolve_with_host("test.job.duration.worker", "host-a", &[] as &[&str], None)
            .expect("context should resolve");
        let context_b = resolver
            .resolve_with_host("test.job.duration.worker", "host-b", &[] as &[&str], None)
            .expect("context should resolve");

        let mut mapper = mapper(json_data).expect("should have parsed mapping config");
        let mapped_a = mapper.try_map(&context_a).expect("should have remapped");
        let mapped_b = mapper.try_map(&context_b).expect("should have remapped");

        assert_ne!(mapped_a, mapped_b);
        assert_eq!(mapped_a.host(), Some("host-a"));
        assert_eq!(mapped_b.host(), Some("host-b"));
        assert_eq!(mapped_a.name(), "test.job.duration");
        assert_tags(&mapped_a, &["job_name:worker"]);
        assert_tags(&mapped_b, &["job_name:worker"]);
    }

    #[tokio::test]
    async fn cache_hit_returns_same_result_as_miss() {
        let mut mapper = mapper_with_cache(simple_mapping_profile(), 1000).expect("should have parsed mapping config");
        assert_eq!(mapper.cache_len(), Some(0));

        let metric = counter_metric("test.job.duration.my_type.my_name", &[]);
        let first = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(mapper.cache_len(), Some(1));

        let metric = counter_metric("test.job.duration.my_type.my_name", &[]);
        let second = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(mapper.cache_len(), Some(1));

        assert_eq!(first.name(), second.name());
        assert_eq!(first.name(), "test.job.duration");
        assert_tags(&first, &["job_type:my_type", "job_name:my_name"]);
        assert_tags(&second, &["job_type:my_type", "job_name:my_name"]);
    }

    #[tokio::test]
    async fn negative_results_are_cached() {
        let mut mapper = mapper_with_cache(simple_mapping_profile(), 1000).expect("should have parsed mapping config");

        let metric = counter_metric("unrelated.metric.name", &[]);
        assert!(mapper.try_map(metric.context()).is_none());
        assert_eq!(mapper.cache_len(), Some(1));

        let metric = counter_metric("unrelated.metric.name", &[]);
        assert!(mapper.try_map(metric.context()).is_none());
        assert_eq!(mapper.cache_len(), Some(1));
    }

    #[tokio::test]
    async fn cache_disabled_when_size_is_zero() {
        let mut mapper = mapper_with_cache(simple_mapping_profile(), 0).expect("should have parsed mapping config");
        assert_eq!(mapper.cache_len(), None);

        let metric = counter_metric("test.job.duration.my_type.my_name", &[]);
        let context = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(context.name(), "test.job.duration");
        assert_tags(&context, &["job_type:my_type", "job_name:my_name"]);

        assert!(mapper
            .try_map(counter_metric("unrelated.metric", &[]).context())
            .is_none());
        assert_eq!(mapper.cache_len(), None);
    }

    #[tokio::test]
    async fn cache_evicts_older_entry_and_retains_newest_within_capacity() {
        let mut mapper = mapper_with_cache(simple_mapping_profile(), 2).expect("should have parsed mapping config");

        // Insert three distinct metric names into a capacity-2 result cache, in order a, b, then c.
        for suffix in ["a", "b", "c"] {
            let name = format!("test.job.duration.t.{}", suffix);
            let metric = counter_metric(Box::leak(name.into_boxed_str()), &[]);
            mapper.try_map(metric.context()).expect("should have remapped");
        }

        let a = mapper.cache_contains("test.job.duration.t.a");
        let b = mapper.cache_contains("test.job.duration.t.b");
        let c = mapper.cache_contains("test.job.duration.t.c");

        // The cache must respect its configured capacity...
        assert!(
            mapper.cache_len().unwrap() <= 2,
            "cache should not exceed configured capacity (got {})",
            mapper.cache_len().unwrap()
        );
        // ...eviction must actually have happened (three distinct names cannot all fit in a capacity-2 cache)...
        assert!(!(a && b && c), "at least one older entry must have been evicted");
        // ...the most-recently-inserted name ("c") must be the entry that survives eviction...
        assert!(c, "the most-recently-inserted metric name should survive eviction");
        // ...and with "c" retained at capacity 2, at most one of the two older names may remain.
        assert!(
            !(a && b),
            "only one older entry may coexist with the newest entry at capacity 2"
        );
    }

    #[tokio::test]
    async fn flood_of_identical_names_populates_single_cache_entry() {
        // Many profiles, only the last one matches the test metric. A flood of identical
        // names should be served from the cache after the first call.
        let mut profiles: Vec<Value> = (0..50)
            .map(|i| {
                json!({
                    "name": format!("noise-{}", i),
                    "prefix": format!("noise{}.", i),
                    "mappings": [{
                        "match": format!("noise{}.*", i),
                        "name": "noise.mapped"
                    }]
                })
            })
            .collect();
        profiles.push(json!({
            "name": "real",
            "prefix": "real.",
            "mappings": [{
                "match": "real.metric.*",
                "name": "real.mapped",
                "tags": { "x": "$1" }
            }]
        }));
        let json_data = Value::Array(profiles);

        let mut mapper = mapper_with_cache(json_data, 16).expect("should have parsed mapping config");

        for _ in 0..10_000 {
            let metric = counter_metric("real.metric.flood", &[]);
            let context = mapper.try_map(metric.context()).expect("should have remapped");
            assert_eq!(context.name(), "real.mapped");
        }

        assert_eq!(
            mapper.cache_len(),
            Some(1),
            "flood of identical names should populate exactly one cache entry"
        );
    }
}

#[cfg(test)]
mod config_smoke {
    use datadog_agent_config_testing::config_registry::structs;
    use datadog_agent_config_testing::run_config_smoke_tests;
    use serde_json::json;

    use super::DogStatsDMapperConfiguration;
    use crate::config::{DatadogRemapper, KEY_ALIASES};

    #[tokio::test]
    async fn smoke_test() {
        run_config_smoke_tests(
            structs::DOGSTATSD_MAPPER_CONFIGURATION,
            &[],
            json!({}),
            |cfg| {
                cfg.as_typed::<DogStatsDMapperConfiguration>()
                    .expect("DogStatsDMapperConfiguration should deserialize")
            },
            KEY_ALIASES,
            DatadogRemapper::new,
        )
        .await
    }
}
