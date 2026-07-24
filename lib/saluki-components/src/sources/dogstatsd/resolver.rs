use std::{num::NonZeroUsize, sync::Arc, time::Duration};

use saluki_context::{
    origin::RawOrigin, tags::SharedTagSet, ContextResolver, ContextResolverBuilder, TagsResolver, TagsResolverBuilder,
};
use saluki_core::components::ComponentContext;
use saluki_error::{generic_error, GenericError};
use stringtheory::interning::GenericMapInterner;

use super::{DogStatsDConfiguration, DogStatsDOriginTagResolver, ProcessOrigin};

/// Context resolvers for the DogStatsD source.
#[derive(Clone)]
pub struct ContextResolvers {
    primary: ContextResolver,
    no_agg: ContextResolver,
    tags: TagsResolver,
    origin_tags: Option<DogStatsDOriginTagResolver>,
}

impl ContextResolvers {
    /// Creates a new `ContextResolvers` instance from the given configuration.
    ///
    /// # Errors
    ///
    /// If the context resolver string interner size is invalid, or there is an error creating either of the context
    /// resolvers, an error is returned.
    pub fn new(
        config: &DogStatsDConfiguration, context: &ComponentContext,
        maybe_origin_tags_resolver: Option<DogStatsDOriginTagResolver>,
    ) -> Result<Self, GenericError> {
        // We'll use the same string interner size for both context resolvers, which does mean double the usage, but
        // it's simpler this way for the moment.
        let context_string_interner_size =
            NonZeroUsize::new(config.effective_context_string_interner_bytes().as_u64() as usize)
                .ok_or_else(|| generic_error!("context_string_interner_size must be greater than 0"))?;

        let cached_contexts_limit = config.cached_contexts_limit;
        let cached_tagsets_limit = config.cached_tagsets_limit;
        let context_expiry_seconds = Duration::from_secs(config.context_expiry_seconds);
        let allow_context_heap_allocations = config.allow_context_heap_allocations;

        let interner = GenericMapInterner::new(context_string_interner_size);

        let origin_tags = maybe_origin_tags_resolver.clone();
        let tags_resolver = TagsResolverBuilder::new(format!("{}/dsd/tags", context.component_id()), interner.clone())?
            .with_cached_tagsets_limit(cached_tagsets_limit)
            .with_idle_tagsets_expiration(context_expiry_seconds)
            .with_heap_allocations(allow_context_heap_allocations)
            .with_origin_tags_resolver(
                maybe_origin_tags_resolver
                    .map(|resolver| -> Arc<dyn saluki_context::origin::OriginTagsResolver> { Arc::new(resolver) }),
            )
            .build();

        let primary_resolver = ContextResolverBuilder::from_name(format!("{}/dsd/primary", context.component_id()))?
            .with_interner_capacity_bytes(context_string_interner_size)
            .with_cached_contexts_limit(cached_contexts_limit)
            .with_idle_context_expiration(context_expiry_seconds)
            .with_heap_allocations(allow_context_heap_allocations)
            .with_tags_resolver(Some(tags_resolver.clone()))
            .with_interner(interner.clone())
            .build();

        let no_agg_resolver = ContextResolverBuilder::from_name(format!("{}/dsd/no_agg", context.component_id()))?
            .with_interner_capacity_bytes(context_string_interner_size)
            .without_caching()
            .with_heap_allocations(allow_context_heap_allocations)
            .with_tags_resolver(Some(tags_resolver.clone()))
            .with_interner(interner)
            .build();

        Ok(ContextResolvers {
            primary: primary_resolver,
            no_agg: no_agg_resolver,
            tags: tags_resolver,
            origin_tags,
        })
    }

    #[cfg(test)]
    pub fn manual(primary: ContextResolver, no_agg: ContextResolver, tags: TagsResolver) -> Self {
        ContextResolvers {
            primary,
            no_agg,
            tags,
            origin_tags: None,
        }
    }

    #[cfg(all(test, target_os = "linux"))]
    pub fn manual_with_origin(
        primary: ContextResolver, no_agg: ContextResolver, tags: TagsResolver, origin_tags: DogStatsDOriginTagResolver,
    ) -> Self {
        ContextResolvers {
            primary,
            no_agg,
            tags,
            origin_tags: Some(origin_tags),
        }
    }

    /// Returns a mutable reference to the primary context resolver.
    ///
    /// This context resolver should be used for "regular" metrics that require aggregation.
    pub fn primary(&mut self) -> &mut ContextResolver {
        &mut self.primary
    }

    /// Returns a mutable reference to the no-aggregation context resolver.
    ///
    /// This context resolver should be used for metrics that don't require aggregation, which implies the metrics had
    /// a timestamp specified in the payload.
    pub fn no_agg(&mut self) -> &mut ContextResolver {
        &mut self.no_agg
    }

    /// Returns a mutable reference to the tags resolver.
    pub fn tags(&mut self) -> &mut TagsResolver {
        &mut self.tags
    }

    /// Resolves origin tags using the sender identity pinned when the packet was received.
    pub fn resolve_origin_tags(&self, origin: RawOrigin<'_>, process_origin: Option<&ProcessOrigin>) -> SharedTagSet {
        match &self.origin_tags {
            Some(resolver) => resolver.resolve_origin_tags_with_process_origin(origin, process_origin),
            None => self.tags.resolve_origin_tags(Some(origin)),
        }
    }
}

#[cfg(test)]
mod tests {
    use bytesize::ByteSize;

    use super::*;
    use crate::sources::dogstatsd::DogStatsDConfiguration;

    fn test_context() -> ComponentContext {
        ComponentContext::test_source("dogstatsd_resolver_test")
    }

    #[test]
    fn new_rejects_zero_interner_size() {
        // Documented `# Errors`: an invalid (zero-byte) string interner size must be rejected.
        let config = DogStatsDConfiguration {
            context_string_interner_size_bytes: Some(ByteSize::b(0)),
            ..Default::default()
        };

        let err = ContextResolvers::new(&config, &test_context(), None)
            .err()
            .expect("a zero interner size must be rejected");
        assert!(
            err.to_string()
                .contains("context_string_interner_size must be greater than 0"),
            "unexpected error: {}",
            err
        );
    }

    #[tokio::test]
    async fn new_builds_working_resolvers_from_the_shared_interner() {
        // Exercises the real production constructor (rather than the `manual` test bypass): a valid config yields
        // usable primary and no-aggregation resolvers. The three resolvers share one interner, but that sharing isn't
        // observable through the public API, so we assert instead that both resolvers construct and resolve contexts
        // correctly from it.
        //
        // NOTE: `#[derive(Default)]` uses each field's own default (a zero interner size) rather than the serde config
        // defaults, so we must set a non-zero interner size explicitly to reach the success path.
        let config = DogStatsDConfiguration {
            context_string_interner_size_bytes: Some(ByteSize::kib(64)),
            ..Default::default()
        };
        let mut resolvers =
            ContextResolvers::new(&config, &test_context(), None).expect("valid config should build resolvers");

        let primary = resolvers
            .primary()
            .resolve("my.metric", ["env:prod"], None)
            .expect("primary resolver should resolve a context");
        assert_eq!(primary.name(), "my.metric");
        assert!(primary.tags().has_tag("env:prod"));

        let no_agg = resolvers
            .no_agg()
            .resolve("my.metric", ["env:prod"], None)
            .expect("no-agg resolver should resolve a context");
        assert_eq!(no_agg.name(), "my.metric");
        assert!(no_agg.tags().has_tag("env:prod"));
    }
}
