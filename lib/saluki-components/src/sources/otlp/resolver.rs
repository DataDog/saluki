use std::{num::NonZeroUsize, sync::Arc, time::Duration};

use saluki_context::{ContextResolver, ContextResolverBuilder, TagsResolverBuilder};
use saluki_core::components::ComponentContext;
use saluki_error::{generic_error, GenericError};
use stringtheory::interning::GenericMapInterner;

use crate::sources::{otlp::OtlpOriginTagResolver, OtlpConfiguration};

const RESOLVER_CACHE_EXPIRATION: Duration = Duration::from_secs(30);

/// Creates a new `ContextResolver` instance from the given configuration.
///
/// # Errors
///
/// If the context resolver string interner size is invalid, or there is an error creating either of the context
/// resolvers, an error is returned.
pub fn new(
    config: &OtlpConfiguration, context: &ComponentContext, maybe_origin_tags_resolver: Option<OtlpOriginTagResolver>,
) -> Result<ContextResolver, GenericError> {
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

    let resolver = ContextResolverBuilder::from_name(format!("{}/dsd/primary", context.component_id()))?
        .with_interner_capacity_bytes(context_string_interner_size)
        .with_cached_contexts_limit(cached_contexts_limit)
        .with_idle_context_expiration(RESOLVER_CACHE_EXPIRATION)
        .with_heap_allocations(config.allow_context_heap_allocations)
        .with_tags_resolver(Some(tags_resolver.clone()))
        .with_interner(interner.clone())
        .build();

    Ok(resolver)
}
