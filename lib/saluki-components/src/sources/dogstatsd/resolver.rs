use std::{num::NonZeroUsize, time::Duration};

use saluki_context::{ContextResolver, ContextResolverBuilder};
use saluki_error::{generic_error, GenericError};

use super::{DogStatsDConfiguration, DogStatsDOriginTagResolver};

/// Context resolvers for the DogStatsD source.
#[derive(Clone)]
pub struct ContextResolvers {
    primary: ContextResolver,
    no_agg: ContextResolver,
}

impl ContextResolvers {
    /// Creates a new `ContextResolvers` instance from the given configuration.
    ///
    /// # Errors
    ///
    /// If the context resolver string interner size is invalid, or there is an error creating either of the context
    /// resolvers, an error is returned.
    pub fn new(
        config: &DogStatsDConfiguration, maybe_origin_tags_resolver: Option<DogStatsDOriginTagResolver>,
    ) -> Result<Self, GenericError> {
        // We'll use the same string interner size for both context resolvers, which does mean double the usage, but
        // it's simpler this way for the moment.
        let context_string_interner_size = NonZeroUsize::new(config.context_string_interner_bytes.as_u64() as usize)
            .ok_or_else(|| generic_error!("context_string_interner_size must be greater than 0"))?;

        let primary_resolver = ContextResolverBuilder::from_name("dogstatsd_primary")?
            .with_interner_capacity_bytes(context_string_interner_size)
            .with_idle_context_expiration(Duration::from_secs(30))
            .with_expiration_interval(Duration::from_secs(1))
            .with_heap_allocations(config.allow_context_heap_allocations)
            .with_origin_tags_resolver(maybe_origin_tags_resolver.clone())
            .build();

        let no_agg_resolver = ContextResolverBuilder::from_name("dogstatsd_no_agg")?
            .with_interner_capacity_bytes(context_string_interner_size)
            .without_caching()
            .with_heap_allocations(config.allow_context_heap_allocations)
            .with_origin_tags_resolver(maybe_origin_tags_resolver)
            .build();

        Ok(ContextResolvers {
            primary: primary_resolver,
            no_agg: no_agg_resolver,
        })
    }

    #[cfg(test)]
    pub fn manual(primary: ContextResolver, no_agg: ContextResolver) -> Self {
        ContextResolvers { primary, no_agg }
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
}
