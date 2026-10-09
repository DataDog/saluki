//! DogStatsD name mapping: renaming metrics and adding tags by mapping profile.
//!
//! [`MetricMapper`] compiles a list of [`MapperProfile`]s and maps metric contexts with them. A profile applies to the
//! metrics whose raw names start with its prefix, or to every metric when its prefix is `*`. The profiles that apply
//! are tried in order, each trying its mappings in order, and the first mapping whose pattern matches the whole raw
//! name renames the metric and adds the mapping's tags, with the pattern's captures expanded in both. A metric that no
//! mapping matches keeps its context. Only the name and tags change: the host and origin tags are kept, and so are the
//! values and metadata.
//!
//! Patterns are either wildcard patterns or regular expressions. A wildcard pattern may contain only ASCII letters,
//! digits, `-`, `_`, `.`, and `*`, never two `*` in a row; each `*` matches a run of characters other than `.` and is a
//! capture. A regular expression is anchored at both ends.
//!
//! The mapper caches its result per raw name, including the result that nothing matched, and resolves the contexts it
//! builds through its own context resolver, which interns their strings. Both caches are unobservable: a mapped
//! context is equal to the one a mapping without caches would build.

use std::{mem::size_of, num::NonZeroUsize, sync::LazyLock, time::Duration};

use regex::Regex;
use saluki_core::{
    accounting::UsageExpr,
    cache::{Cache, CacheBuilder},
    data_model::{
        event::metric::context::{Context, ContextResolver, ContextResolverBuilder},
        tags::{SharedTagSet, TagSet},
    },
    execution::{Kernel, Progress, Turn},
    topology::EventsBuffer,
};
use snafu::Snafu;
use stringtheory::MetaString;

use crate::params::{MapperProfile, MatchType};

/// How long a context the mapper resolved stays cached in its context resolver after it was last used.
const RESOLVER_IDLE_CONTEXT_EXPIRATION: Duration = Duration::from_secs(30);

/// The pattern every wildcard match pattern must match.
const ALLOWED_WILDCARD_MATCH_PATTERN_SOURCE: &str = r"^[a-zA-Z0-9\-_*.]+$";

static ALLOWED_WILDCARD_MATCH_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(ALLOWED_WILDCARD_MATCH_PATTERN_SOURCE).expect("Invalid regex in ALLOWED_WILDCARD_MATCH_PATTERN")
});

/// An error compiling mapper profiles.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum MapperError {
    /// A profile has an empty name.
    #[snafu(display("missing profile name"))]
    MissingProfileName,

    /// A profile has an empty prefix.
    #[snafu(display("missing prefix for profile: {}", profile))]
    MissingPrefix {
        /// The profile's name.
        profile: String,
    },

    /// A mapping has an empty name.
    #[snafu(display("profile: {}, mapping num {}: name is required", profile, mapping))]
    MissingName {
        /// The profile's name.
        profile: String,

        /// The mapping's position in its profile, counting from zero.
        mapping: usize,
    },

    /// A mapping has an empty match pattern.
    #[snafu(display("profile: {}, mapping num {}: match is required", profile, mapping))]
    MissingMatch {
        /// The profile's name.
        profile: String,

        /// The mapping's position in its profile, counting from zero.
        mapping: usize,
    },

    /// A wildcard match pattern contains a character other than ASCII letters, digits, `-`, `_`, `.`, and `*`.
    #[snafu(display(
        "invalid wildcard match pattern `{}`, it does not match allowed match regex `{}`",
        pattern,
        ALLOWED_WILDCARD_MATCH_PATTERN_SOURCE
    ))]
    InvalidWildcard {
        /// The pattern.
        pattern: String,
    },

    /// A wildcard match pattern contains two `*` in a row.
    #[snafu(display(
        "invalid wildcard match pattern `{}`, it should not contain consecutive `*`",
        pattern
    ))]
    ConsecutiveWildcards {
        /// The pattern.
        pattern: String,
    },

    /// A match pattern does not compile to a regular expression.
    #[snafu(display("Failed to compile regular expression `{}` for `{}` match type", pattern, match_type))]
    InvalidRegex {
        /// The anchored regular expression the pattern compiles to.
        pattern: String,

        /// The pattern's match type.
        match_type: &'static str,

        /// The underlying error.
        source: regex::Error,
    },
}

/// Checks that mapper profiles compile, without building a mapper.
///
/// Profiles are checked in order, and within a profile its name, then its prefix, then each mapping's name, match
/// pattern, and pattern compilation, so the error returned is the first problem in that order. Unlike
/// [`MetricMapper::new`], this needs no Tokio runtime.
///
/// # Errors
///
/// Returns the first problem found, as [`MetricMapper::new`] would.
pub fn validate_profiles(profiles: &[MapperProfile]) -> Result<(), MapperError> {
    compile_profiles(profiles).map(|_| ())
}

/// The memory one entry of a mapper's result cache takes: the raw name and the mapping result.
pub const RESULT_CACHE_ENTRY_BYTES: usize = size_of::<(MetaString, Option<CachedMapResult>)>();

/// Returns the memory a mapper's result cache can hold with `cache_size` entries, [`RESULT_CACHE_ENTRY_BYTES`] each.
///
/// A cache size of `0` disables the cache, which then holds nothing.
pub fn result_cache_usage(cache_size: usize) -> UsageExpr {
    UsageExpr::product(
        "array",
        UsageExpr::struct_size::<(MetaString, Option<CachedMapResult>)>("mapper result cache"),
        UsageExpr::constant("len", cache_size),
    )
}

fn compile_profiles(profiles: &[MapperProfile]) -> Result<Vec<MappingProfile>, MapperError> {
    let mut compiled = Vec::with_capacity(profiles.len());
    for config_profile in profiles {
        if config_profile.name.is_empty() {
            return Err(MapperError::MissingProfileName);
        }
        if config_profile.prefix.is_empty() {
            return Err(MapperError::MissingPrefix {
                profile: config_profile.name.clone(),
            });
        }

        let mut profile = MappingProfile {
            prefix: config_profile.prefix.clone(),
            mappings: Vec::with_capacity(config_profile.mappings.len()),
        };

        for (mapping_index, mapping) in config_profile.mappings.iter().enumerate() {
            if mapping.name.is_empty() {
                return Err(MapperError::MissingName {
                    profile: config_profile.name.clone(),
                    mapping: mapping_index,
                });
            }
            if mapping.metric_match.is_empty() {
                return Err(MapperError::MissingMatch {
                    profile: config_profile.name.clone(),
                    mapping: mapping_index,
                });
            }
            let regex = build_regex(&mapping.metric_match, mapping.match_type)?;
            profile.mappings.push(MetricMapping {
                name: mapping.name.clone(),
                tags: mapping.tags.clone(),
                regex,
            });
        }
        compiled.push(profile);
    }
    Ok(compiled)
}

fn build_regex(match_re: &str, match_type: MatchType) -> Result<Regex, MapperError> {
    let mut pattern = match_re.to_owned();
    if match_type == MatchType::Wildcard {
        // Check it against the allowed wildcard pattern
        if !ALLOWED_WILDCARD_MATCH_PATTERN.is_match(&pattern) {
            return Err(MapperError::InvalidWildcard { pattern });
        }
        if pattern.contains("**") {
            return Err(MapperError::ConsecutiveWildcards { pattern });
        }
        pattern = pattern.replace(".", "\\.");
        pattern = pattern.replace("*", "([^.]*)");
    }

    let final_pattern = format!("^{}$", pattern);

    Regex::new(&final_pattern).map_err(|source| MapperError::InvalidRegex {
        pattern: final_pattern,
        match_type: match_type.as_str(),
        source,
    })
}

struct MappingProfile {
    prefix: String,
    mappings: Vec<MetricMapping>,
}

struct MetricMapping {
    name: String,
    tags: Vec<(String, String)>,
    regex: Regex,
}

/// One cached mapping result: the mapped name and the tags the mapping added.
#[derive(Clone)]
struct CachedMapResult {
    name: MetaString,
    extra_tags: SharedTagSet,
}

/// The DogStatsD mapper: maps metric contexts by mapping profile.
///
/// See the [module documentation][self] for the mapping rules. The mapper is also a [`Kernel`] that maps every metric
/// of a batch in place.
pub struct MetricMapper {
    profiles: Vec<MappingProfile>,
    context_resolver: ContextResolver,
    cache: Option<Cache<MetaString, Option<CachedMapResult>>>,
}

impl MetricMapper {
    /// Builds a mapper from its profiles.
    ///
    /// The mapper resolves the contexts it builds through a context resolver whose string interner holds
    /// `interner_bytes`, and caches the mapping results of up to `cache_size` raw names; a cache size of `0` disables
    /// the result cache. Both report telemetry under names derived from `identity`, normally the ID of the component
    /// the mapper runs in: the resolver as `<identity>/dsd_mapper/primary` and the result cache as
    /// `<identity>/dsd_mapper/result_cache`.
    ///
    /// The resolver drives its telemetry from a background task, so the mapper must be built inside a Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if a profile has an empty name or prefix, a mapping has an empty name or match pattern, or a
    /// pattern does not compile; see [`validate_profiles`].
    pub fn new(
        identity: &str, profiles: &[MapperProfile], cache_size: usize, interner_bytes: NonZeroUsize,
    ) -> Result<Self, MapperError> {
        let profiles = compile_profiles(profiles)?;

        let context_resolver = ContextResolverBuilder::from_name(format!("{}/dsd_mapper/primary", identity))
            .expect("resolver name is not empty")
            .with_interner_capacity_bytes(interner_bytes)
            .with_idle_context_expiration(RESOLVER_IDLE_CONTEXT_EXPIRATION)
            .build();

        let cache = NonZeroUsize::new(cache_size).map(|capacity| {
            CacheBuilder::from_identifier(format!("{}/dsd_mapper/result_cache", identity))
                .expect("cache identifier is not empty")
                .with_capacity(capacity)
                .build()
        });

        Ok(Self {
            profiles,
            context_resolver,
            cache,
        })
    }

    /// Maps one metric context.
    ///
    /// Returns the mapped context, or `None` if no mapping matches the context's raw name, or if the mapped context
    /// cannot be resolved because the interner of the context resolver is full and cannot allocate.
    pub fn try_map(&mut self, context: &Context) -> Option<Context> {
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

        // We also cache "negative" results -- no match for this metric in the configured profiles -- to save ourselves
        // some work.
        if let Some(cache) = &self.cache {
            cache.insert(metric_name.clone(), None);
        }
        None
    }

    /// Maps every metric of a buffer in place, leaving other events, and metrics no mapping matches, unchanged.
    pub fn map_buffer(&mut self, buffer: &mut EventsBuffer) {
        for event in buffer {
            if let Some(metric) = event.try_as_metric_mut() {
                if let Some(new_context) = self.try_map(metric.context()) {
                    *metric.context_mut() = new_context;
                }
            }
        }
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

impl Kernel for MetricMapper {
    fn consume(&mut self, _turn: &mut Turn<'_>, input: &mut EventsBuffer) -> Progress {
        self.map_buffer(input);
        Progress::Done
    }
}

#[cfg(test)]
mod reference;

#[cfg(test)]
mod tests;
