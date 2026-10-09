use std::collections::HashMap;
use std::num::NonZeroUsize;

use async_trait::async_trait;
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{
        transforms::{SynchronousTransform, SynchronousTransformBuilder},
        BuildContext,
    },
    topology::EventsBuffer,
};
use saluki_error::{generic_error, GenericError};
use saluki_metric_kernels::{
    mapper::{result_cache_usage, validate_profiles, MetricMapper},
    params::{MapperProfile, MatchType, MetricMapping},
};

const MATCH_TYPE_WILDCARD: &str = "wildcard";
const MATCH_TYPE_REGEX: &str = "regex";

/// DogStatsD mapper transform.
pub struct DogStatsDMapperConfiguration {
    /// Total size of the string interner used for contexts, in bytes.
    context_string_interner_bytes: NonZeroUsize,

    /// Maximum number of mapped results to cache.
    ///
    /// When set to `0`, the cache is disabled.
    cache_size: usize,

    /// Metric mapping profiles.
    profiles: Vec<DogStatsDMapperProfile>,
}

/// A DogStatsD metric mapping profile.
pub struct DogStatsDMapperProfile {
    /// Profile name used in configuration errors.
    pub name: String,

    /// Metric-name prefix this profile applies to.
    pub prefix: String,

    /// Metric mappings evaluated for this profile.
    pub mappings: Vec<DogStatsDMetricMapping>,
}

/// A metric-name mapping in a [`DogStatsDMapperProfile`].
pub struct DogStatsDMetricMapping {
    /// Pattern matched against the incoming metric name.
    pub metric_match: String,

    /// Pattern type: `wildcard`, `regex`, or empty for the default wildcard behavior.
    pub match_type: String,

    /// Metric name emitted after a match.
    pub name: String,

    /// Tags emitted after expanding capture groups.
    pub tags: HashMap<String, String>,
}

impl DogStatsDMapperConfiguration {
    /// Creates a DogStatsD mapper configuration.
    pub fn new(
        context_string_interner_bytes: NonZeroUsize, cache_size: usize, profiles: Vec<DogStatsDMapperProfile>,
    ) -> Self {
        Self {
            context_string_interner_bytes,
            cache_size,
            profiles,
        }
    }

    /// Returns the profiles the mapper kernel is built from, converted as building the mapper converts them.
    ///
    /// An empty match type means wildcard. Building the mapper checks the profiles further (see
    /// [`validate_profiles`]); this conversion only reports an unknown match type, and the problems that come before
    /// it.
    ///
    /// # Errors
    ///
    /// Returns an error if a match type is neither empty, `wildcard`, nor `regex`, or if a problem the kernel finds
    /// comes before it.
    pub fn profiles(&self) -> Result<Vec<MapperProfile>, GenericError> {
        mapper_profiles(&self.profiles)
    }

    fn build_mapper(&self, context: BuildContext) -> Result<MetricMapper, GenericError> {
        let profiles = self.profiles()?;
        Ok(MetricMapper::new(
            context.component_id(),
            &profiles,
            self.cache_size,
            self.context_string_interner_bytes,
        )?)
    }
}

/// Converts the configured profiles into the mapper kernel's parameters.
///
/// An empty match type means wildcard. The kernel checks everything else: profile by profile, its name and prefix, then
/// each mapping's name, match, and pattern, where a mapping's match type was checked first. So when a match type is
/// invalid, the profiles before it are checked first, and their first problem is reported instead, as it was before
/// the kernel was extracted.
///
/// # Errors
///
/// Returns an error if a match type is neither empty, `wildcard`, nor `regex`, or if a problem the kernel finds comes
/// before it.
fn mapper_profiles(profiles: &[DogStatsDMapperProfile]) -> Result<Vec<MapperProfile>, GenericError> {
    let mut converted = Vec::with_capacity(profiles.len());
    for profile in profiles {
        let mut mappings = Vec::with_capacity(profile.mappings.len());
        for (mapping_index, mapping) in profile.mappings.iter().enumerate() {
            let match_type = match mapping.match_type.as_str() {
                // Default to wildcard when not set.
                "" | MATCH_TYPE_WILDCARD => MatchType::Wildcard,
                MATCH_TYPE_REGEX => MatchType::Regex,
                unknown => {
                    converted.push(MapperProfile {
                        name: profile.name.clone(),
                        prefix: profile.prefix.clone(),
                        mappings,
                    });
                    validate_profiles(&converted)?;
                    return Err(generic_error!(
                        "profile: {}, mapping num {}: invalid match type `{}`, expected `wildcard` or `regex`",
                        profile.name,
                        mapping_index,
                        unknown,
                    ));
                }
            };
            mappings.push(MetricMapping {
                metric_match: mapping.metric_match.clone(),
                match_type,
                name: mapping.name.clone(),
                tags: mapping
                    .tags
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            });
        }
        converted.push(MapperProfile {
            name: profile.name.clone(),
            prefix: profile.prefix.clone(),
            mappings,
        });
    }
    Ok(converted)
}

#[async_trait]
impl SynchronousTransformBuilder for DogStatsDMapperConfiguration {
    async fn build(&self, context: BuildContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        let metric_mapper = self.build_mapper(context)?;
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
            .with_fixed_amount("string interner", self.context_string_interner_bytes.get());

        // Account for the per-name result cache when enabled.
        if self.cache_size > 0 {
            min.with_expr(result_cache_usage(self.cache_size));
        }
    }
}

/// The DogStatsD mapper: maps the metrics of each buffer with the mapper kernel.
pub struct DogStatsDMapper {
    metric_mapper: MetricMapper,
}

impl SynchronousTransform for DogStatsDMapper {
    fn transform_buffer(&mut self, event_buffer: &mut EventsBuffer) {
        self.metric_mapper.map_buffer(event_buffer);
    }
}

#[cfg(test)]
mod tests {

    use std::collections::HashMap;
    use std::num::NonZeroUsize;

    use saluki_core::{
        components::{transforms::SynchronousTransform, BuildContext},
        data_model::event::{
            metric::{
                context::{Context, ContextResolverBuilder},
                Metric,
            },
            Event,
        },
        topology::EventsBuffer,
    };
    use saluki_error::GenericError;
    use saluki_metric_kernels::mapper::MetricMapper;

    use super::{DogStatsDMapper, DogStatsDMapperConfiguration, DogStatsDMapperProfile, DogStatsDMetricMapping};

    macro_rules! metric_mapping {
        (@match_type) => {
            String::new()
        };
        (@match_type $match_type:expr) => {
            $match_type.to_string()
        };
        ({
            "match": $metric_match:expr,
            $("match_type": $match_type:expr,)?
            "name": $name:expr
            $(, "tags": { $($tag_name:literal: $tag_value:expr),* $(,)? })?
        }) => {
            DogStatsDMetricMapping {
                metric_match: $metric_match.to_string(),
                match_type: metric_mapping!(@match_type $($match_type)?),
                name: $name.to_string(),
                tags: HashMap::from([$($(($tag_name.to_string(), $tag_value.to_string())),*)?]),
            }
        };
    }

    macro_rules! mapper_profiles {
        ([$({
            "name": $name:expr,
            "prefix": $prefix:expr,
            "mappings": [$($mapping:tt),* $(,)?]
        }),* $(,)?]) => {
            vec![$(DogStatsDMapperProfile {
                name: $name.to_string(),
                prefix: $prefix.to_string(),
                mappings: vec![$(metric_mapping!($mapping)),*],
            }),*]
        };
    }

    fn counter_metric(name: &'static str, tags: &[&'static str]) -> Metric {
        let context = Context::from_static_parts(name, tags);
        Metric::counter(context, 1.0)
    }

    fn mapper(profiles: Vec<DogStatsDMapperProfile>) -> Result<MetricMapper, GenericError> {
        mapper_with_cache(profiles, 1000)
    }

    fn mapper_with_cache(
        profiles: Vec<DogStatsDMapperProfile>, cache_size: usize,
    ) -> Result<MetricMapper, GenericError> {
        let config =
            DogStatsDMapperConfiguration::new(NonZeroUsize::new(64 * 1024).expect("not zero"), cache_size, profiles);
        config.build_mapper(BuildContext::test_transform("test_mapper"))
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

    fn simple_mapping_profile() -> Vec<DogStatsDMapperProfile> {
        mapper_profiles!([{
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
            config: Vec<DogStatsDMapperProfile>,
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
                config: mapper_profiles!([{
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
                config: mapper_profiles!([{
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
                config: mapper_profiles!([{
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
                config: mapper_profiles!([{
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
                config: mapper_profiles!([{
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
                config: mapper_profiles!([{
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
                config: mapper_profiles!([{
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
                config: mapper_profiles!([{
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
                config: mapper_profiles!([{
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
                config: mapper_profiles!([
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
                config: mapper_profiles!([{
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
                config: mapper_profiles!([
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
                config: mapper_profiles!([
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
                config: mapper_profiles!([{
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
                config: mapper_profiles!([{
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
        // Each case is `(description, config, expected_error_substring)`. Empty required values reach the
        // component's validation; missing fields are rejected at the configuration boundary.
        let cases: Vec<(&str, Vec<DogStatsDMapperProfile>, &str)> = vec![
            // Custom (present-but-empty) validation branches.
            (
                "profile with an empty name",
                mapper_profiles!([{ "name": "", "prefix": "test.", "mappings": [] }]),
                "missing profile name",
            ),
            (
                "profile with an empty prefix",
                mapper_profiles!([{ "name": "test", "prefix": "", "mappings": [] }]),
                "missing prefix for profile: test",
            ),
            (
                "mapping with an empty match",
                mapper_profiles!([{ "name": "test", "prefix": "test.", "mappings": [{ "match": "", "name": "test.mapped" }] }]),
                "match is required",
            ),
            (
                "mapping with an empty name",
                mapper_profiles!([{ "name": "test", "prefix": "test.", "mappings": [{ "match": "test.job.duration.*.*", "name": "", "tags": { "job_type": "$1" } }] }]),
                "name is required",
            ),
            (
                "second mapping with an empty name",
                mapper_profiles!([{
                    "name": "test",
                    "prefix": "test.",
                    "mappings": [
                        { "match": "test.valid", "name": "mapped" },
                        { "match": "test.invalid", "name": "" }
                    ]
                }]),
                "mapping num 1: name is required",
            ),
            // Match compilation / type validation.
            (
                "wildcard match with disallowed characters",
                mapper_profiles!([{ "name": "test", "prefix": "test.", "mappings": [{ "match": "test.[]duration.*.*", "name": "test.job.duration" }] }]),
                "does not match allowed match regex",
            ),
            (
                "wildcard match anchored with a caret",
                mapper_profiles!([{ "name": "test", "prefix": "test.", "mappings": [{ "match": "^test.invalid.duration.*.*", "name": "test.job.duration" }] }]),
                "does not match allowed match regex",
            ),
            (
                "wildcard match with consecutive wildcards",
                mapper_profiles!([{ "name": "test", "prefix": "test.", "mappings": [{ "match": "test.invalid.duration.**", "name": "test.job.duration" }] }]),
                "consecutive",
            ),
            (
                "unknown match type",
                mapper_profiles!([{ "name": "test", "prefix": "test.", "mappings": [{ "match": "test.invalid.duration", "match_type": "invalid", "name": "test.job.duration" }] }]),
                "invalid match type",
            ),
            // A mapping's match type is checked before its name, and every earlier check before either.
            (
                "unknown match type on a mapping with an empty name",
                mapper_profiles!([{
                    "name": "test",
                    "prefix": "test.",
                    "mappings": [{ "match": "test.invalid", "match_type": "invalid", "name": "" }]
                }]),
                "profile: test, mapping num 0: invalid match type `invalid`",
            ),
            (
                "earlier mapping with an empty name before an unknown match type",
                mapper_profiles!([{
                    "name": "test",
                    "prefix": "test.",
                    "mappings": [
                        { "match": "test.valid", "name": "" },
                        { "match": "test.invalid", "match_type": "invalid", "name": "mapped" }
                    ]
                }]),
                "profile: test, mapping num 0: name is required",
            ),
            (
                "earlier profile with an empty prefix before an unknown match type",
                mapper_profiles!([
                    { "name": "first", "prefix": "", "mappings": [] },
                    {
                        "name": "second",
                        "prefix": "test.",
                        "mappings": [{ "match": "test.invalid", "match_type": "invalid", "name": "mapped" }]
                    }
                ]),
                "missing prefix for profile: first",
            ),
            (
                "unknown match type before a later invalid pattern",
                mapper_profiles!([{
                    "name": "test",
                    "prefix": "test.",
                    "mappings": [
                        { "match": "test.invalid", "match_type": "invalid", "name": "mapped" },
                        { "match": "test.**", "name": "mapped" }
                    ]
                }]),
                "profile: test, mapping num 0: invalid match type `invalid`",
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
        let profiles = mapper_profiles!([{
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

        let mut mapper = mapper(profiles).expect("should have built mapper");
        let mapped_a = mapper.try_map(&context_a).expect("should have remapped");
        let mapped_b = mapper.try_map(&context_b).expect("should have remapped");

        assert_ne!(mapped_a, mapped_b);
        assert_eq!(mapped_a.host(), Some("host-a"));
        assert_eq!(mapped_b.host(), Some("host-b"));
        assert_eq!(mapped_a.name(), "test.job.duration");
        assert_tags(&mapped_a, &["job_name:worker"]);
        assert_tags(&mapped_b, &["job_name:worker"]);
    }
}
