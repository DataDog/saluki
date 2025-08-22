use std::{num::NonZeroUsize, sync::Arc, time::Duration};

use saluki_context::{ContextResolver, ContextResolverBuilder, TagsResolver, TagsResolverBuilder};
use saluki_core::components::ComponentContext;
use saluki_error::{generic_error, GenericError};
use stringtheory::interning::GenericMapInterner;

use super::{DogStatsDConfiguration, DogStatsDOriginTagResolver};

const RESOLVER_CACHE_EXPIRATION: Duration = Duration::from_secs(30);

/// Context resolvers for the DogStatsD source.
#[derive(Clone)]
pub struct ContextResolvers {
    primary: ContextResolver,
    no_agg: ContextResolver,
    tags: TagsResolver,
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
        let context_string_interner_size = NonZeroUsize::new(config.context_string_interner_bytes.as_u64() as usize)
            .ok_or_else(|| generic_error!("context_string_interner_size must be greater than 0"))?;

        let cached_contexts_limit = config.cached_contexts_limit;
        let cached_tagsets_limit = config.cached_tagsets_limit;

        let interner = GenericMapInterner::new(context_string_interner_size);

        let tags_resolver = TagsResolverBuilder::new(format!("{}/dsd/tags", context.component_id()), interner.clone())?
            .with_cached_tagsets_limit(cached_tagsets_limit)
            .with_idle_tagsets_expiration(RESOLVER_CACHE_EXPIRATION)
            .with_heap_allocations(config.allow_context_heap_allocations)
            .with_origin_tags_resolver(
                maybe_origin_tags_resolver
                    .map(|resolver| -> Arc<dyn saluki_context::origin::OriginTagsResolver> { Arc::new(resolver) }),
            )
            .build();

        let primary_resolver = ContextResolverBuilder::from_name(format!("{}/dsd/primary", context.component_id()))?
            .with_interner_capacity_bytes(context_string_interner_size)
            .with_cached_contexts_limit(cached_contexts_limit)
            .with_idle_context_expiration(RESOLVER_CACHE_EXPIRATION)
            .with_heap_allocations(config.allow_context_heap_allocations)
            .with_tags_resolver(Some(tags_resolver.clone()))
            .with_interner(interner.clone())
            .build();

        let no_agg_resolver = ContextResolverBuilder::from_name(format!("{}/dsd/no_agg", context.component_id()))?
            .with_interner_capacity_bytes(context_string_interner_size)
            .without_caching()
            .with_heap_allocations(config.allow_context_heap_allocations)
            .with_tags_resolver(Some(tags_resolver.clone()))
            .with_interner(interner.clone())
            .build();

        Ok(ContextResolvers {
            primary: primary_resolver,
            no_agg: no_agg_resolver,
            tags: tags_resolver,
        })
    }

    #[cfg(test)]
    pub fn manual(primary: ContextResolver, no_agg: ContextResolver, tags: TagsResolver) -> Self {
        ContextResolvers { primary, no_agg, tags }
    }

    /// Returns a mutable reference to the primary context resolver.
    ///
    /// This context resolver should be used for "regular" metrics that require aggregation.
    pub fn primary(&mut self) -> &mut ContextResolver {
        &mut self.primary
    }

    /// Returns a mutable reference to the no-aggregation context resolver.
    ///
    /// This context resolver should be used for metrics that do not require aggregation, which implies the metrics had
    /// a timestamp specified in the payload.
    pub fn no_agg(&mut self) -> &mut ContextResolver {
        &mut self.no_agg
    }

    /// Returns a mutable reference to the tags resolver.
    pub fn tags(&mut self) -> &mut TagsResolver {
        &mut self.tags
    }
}
