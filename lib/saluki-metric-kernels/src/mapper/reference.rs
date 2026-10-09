//! A frozen copy of the DogStatsD mapper as it was before its core moved into this crate.
//!
//! The functions below are the original `saluki-components` mapper, copied verbatim with only their imports and
//! visibility adjusted, and with the component builder, the memory bounds, and a cache lookup helper the tests do not
//! use left out, since they carry no mapping semantics. The
//! property tests compare [`MetricMapper`][crate::mapper::MetricMapper] with it, so that the kernel keeps the native
//! behavior while the legacy component calls the kernel. Do not change it.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use saluki_core::cache::{Cache, CacheBuilder};
use saluki_core::{
    components::{transforms::SynchronousTransform, BuildContext},
    data_model::{
        event::metric::context::{Context, ContextResolver, ContextResolverBuilder},
        tags::{SharedTagSet, TagSet},
    },
    topology::EventsBuffer,
};
use saluki_error::{generic_error, ErrorContext, GenericError};
use stringtheory::MetaString;

const MATCH_TYPE_WILDCARD: &str = "wildcard";
const MATCH_TYPE_REGEX: &str = "regex";

static ALLOWED_WILDCARD_MATCH_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9\-_*.]+$").expect("Invalid regex in ALLOWED_WILDCARD_MATCH_PATTERN"));

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

    pub(crate) fn build_mapper(&self, context: BuildContext) -> Result<MetricMapper, GenericError> {
        let mut profiles = Vec::with_capacity(self.profiles.len());
        for config_profile in &self.profiles {
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

            for (mapping_index, mapping) in config_profile.mappings.iter().enumerate() {
                let match_type = match mapping.match_type.as_str() {
                    // Default to wildcard when not set.
                    "" => MATCH_TYPE_WILDCARD,
                    MATCH_TYPE_WILDCARD => MATCH_TYPE_WILDCARD,
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
                profile.mappings.push(MetricMapping {
                    name: mapping.name.clone(),
                    tags: mapping.tags.clone(),
                    regex,
                });
            }
            profiles.push(profile);
        }

        let context_resolver =
            ContextResolverBuilder::from_name(format!("{}/dsd_mapper/primary", context.component_id()))
                .expect("resolver name is not empty")
                .with_interner_capacity_bytes(self.context_string_interner_bytes)
                .with_idle_context_expiration(Duration::from_secs(30))
                .build();

        let cache = match NonZeroUsize::new(self.cache_size) {
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

pub(crate) struct MetricMapper {
    profiles: Vec<MappingProfile>,
    context_resolver: ContextResolver,
    cache: Option<Cache<MetaString, Option<CachedMapResult>>>,
}

impl MetricMapper {
    pub(crate) fn try_map(&mut self, context: &Context) -> Option<Context> {
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

    pub(crate) fn cache_len(&self) -> Option<usize> {
        self.cache.as_ref().map(|c| c.len())
    }
}

pub struct DogStatsDMapper {
    pub(crate) metric_mapper: MetricMapper,
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
