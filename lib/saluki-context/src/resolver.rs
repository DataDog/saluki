use std::{num::NonZeroUsize, sync::Arc, time::Duration};

use saluki_common::{
    cache::{weight::ItemCountWeighter, Cache, CacheBuilder},
    collections::PrehashedHashSet,
    hash::NoopU64BuildHasher,
};
use saluki_error::{generic_error, GenericError};
use saluki_metrics::static_metrics;
use stringtheory::{interning::GenericMapInterner, CheapMetaString, MetaString};
use tokio::time::sleep;
use tracing::debug;

use crate::{
    context::{Context, ContextInner},
    hash::{hash_context_with_seen, ContextKey, TagSetKey},
    origin::{OriginTagsResolver, RawOrigin},
    tags::{SharedTagSet, TagSet},
};

// SAFETY: We know, unquestionably, that this value is not zero.
const DEFAULT_CONTEXT_RESOLVER_CACHED_CONTEXTS_LIMIT: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(500_000) };

// SAFETY: We know, unquestionably, that this value is not zero.
const DEFAULT_CONTEXT_RESOLVER_INTERNER_CAPACITY_BYTES: NonZeroUsize =
    unsafe { NonZeroUsize::new_unchecked(2 * 1024 * 1024) };

const SEEN_HASHSET_INITIAL_CAPACITY: usize = 128;

type ContextCache = Cache<ContextKey, Context, ItemCountWeighter, NoopU64BuildHasher>;
type TagSetCache = Cache<TagSetKey, SharedTagSet, ItemCountWeighter, NoopU64BuildHasher>;

static_metrics! {
    name => Telemetry,
    prefix => context_resolver,
    labels => [resolver_id: String],
    metrics => [
        gauge(interner_capacity_bytes),
        gauge(interner_len_bytes),
        gauge(interner_entries),
        counter(intern_fallback_total),

        counter(resolved_existing_context_total),
        counter(resolved_new_context_total),
        gauge(active_contexts),

        counter(resolved_existing_tagset_total),
        counter(resolved_new_tagset_total),
    ],
}

/// Builder for creating a [`ContextResolver`].
///
/// # Missing
///
/// - Support for configuring the size limit of cached contexts. (See note in [`ContextResolver::new`])
pub struct ContextResolverBuilder {
    name: String,
    caching_enabled: bool,
    cached_contexts_limit: Option<NonZeroUsize>,
    idle_context_expiration: Option<Duration>,
    interner_capacity_bytes: Option<NonZeroUsize>,
    allow_heap_allocations: Option<bool>,
    origin_tags_resolver: Option<Arc<dyn OriginTagsResolver>>,
    tags_resolver: Option<TagsResolver>,
    telemetry_enabled: bool,
}

impl ContextResolverBuilder {
    /// Creates a new `ContextResolverBuilder` with the given resolver name.
    ///
    /// The resolver name _should_ be unique, but it is not required to be. Metrics for the resolver will be
    /// emitted using the given name, so in cases where the name is not unique, those metrics will be aggregated
    /// together and it will not be possible to distinguish between the different resolvers.
    ///
    /// # Errors
    ///
    /// If the given resolver name is empty, an error is returned.
    pub fn from_name<S: Into<String>>(name: S) -> Result<Self, GenericError> {
        let name = name.into();
        if name.is_empty() {
            return Err(generic_error!("resolver name must not be empty"));
        }

        Ok(Self {
            name,
            caching_enabled: true,
            cached_contexts_limit: None,
            idle_context_expiration: None,
            interner_capacity_bytes: None,
            allow_heap_allocations: None,
            origin_tags_resolver: None,
            tags_resolver: None,
            telemetry_enabled: true,
        })
    }

    /// Sets whether or not to enable caching of resolved contexts.
    ///
    /// [`ContextResolver`] provides two main benefits: consistent behavior for resolving contexts (interning, origin
    /// tags, etc), and the caching of those resolved contexts to speed up future resolutions. However, caching contexts
    /// means that we pay a memory cost for the cache itself, even if the contexts are not ever reused or are seen
    /// infrequently. While expiration can help free up cache capacity, it cannot help recover the memory used by the
    /// underlying cache data structure once they have expanded to hold the contexts.
    ///
    /// Disabling caching allows normal resolving to take place without the overhead of caching the contexts. This can
    /// lead to lower average memory usage, as contexts will only live as long as they are needed, but it will reduce
    /// memory determinism as memory will be allocated for every resolved context (minus interned strings), which means
    /// that resolving the same context ten times in a row will result in ten separate allocations, and so on.
    ///
    /// Defaults to caching enabled.
    pub fn without_caching(mut self) -> Self {
        self.caching_enabled = false;
        self.idle_context_expiration = None;
        self
    }

    /// Sets the limit on the number of cached contexts.
    ///
    /// This is the maximum number of resolved contexts that can be cached at any given time. This limit does not affect
    /// the total number of contexts that can be _alive_ at any given time, which is dependent on the interner capacity
    /// and whether or not heap allocations are allowed.
    ///
    /// Caching contexts is beneficial when the same context is resolved frequently, and it is generally worth
    /// allowing for higher limits on cached contexts when heap allocations are allowed, as this can better amortize the
    /// cost of those heap allocations.
    ///
    /// If value is zero, caching will be disabled, and no contexts will be cached. This is equivalent to calling
    /// `without_caching`.
    ///
    /// Defaults to 500,000.
    pub fn with_cached_contexts_limit(mut self, limit: usize) -> Self {
        match NonZeroUsize::new(limit) {
            Some(limit) => {
                self.cached_contexts_limit = Some(limit);
                self
            }
            None => self.without_caching(),
        }
    }

    /// Sets the time before contexts are considered "idle" and eligible for expiration.
    ///
    /// This controls how long a context will be kept in the cache after its last access or creation time. This value is
    /// a lower bound, as contexts eligible for expiration may not be expired immediately. Contexts may still be removed
    /// prior to their natural expiration time if the cache is full and evictions are required to make room for a new
    /// context.
    ///
    /// Defaults to no expiration.
    pub fn with_idle_context_expiration(mut self, time_to_idle: Duration) -> Self {
        self.idle_context_expiration = Some(time_to_idle);
        self
    }

    /// Sets the capacity of the string interner, in bytes.
    ///
    /// This is the maximum number of bytes that the interner will use for interning strings that are present in
    /// contexts being resolved. This capacity may or may not be allocated entirely when the resolver is built, but the
    /// interner will not exceed the configured capacity when allocating any backing storage.
    ///
    /// This value directly impacts the number of contexts that can be resolved when heap allocations are disabled, as
    /// all resolved contexts must either have values (name or tags) that can be inlined or interned. Once the interner
    /// is full, contexts may fail to be resolved if heap allocations are disabled.
    ///
    /// The optimal value will almost always be workload-dependent, but a good starting point can be to estimate around
    /// 150 - 200 bytes per context based on empirical measurements around common metric name and tag lengths. This
    /// translate to around 5000 unique contexts per 1MB of interner size.
    ///
    /// Defaults to 2MB.
    pub fn with_interner_capacity_bytes(mut self, capacity: NonZeroUsize) -> Self {
        self.interner_capacity_bytes = Some(capacity);
        self
    }

    /// Sets whether or not to allow heap allocations when interning strings.
    ///
    /// In cases where the interner is full, this setting determines whether or not we refuse to resolve a context, or
    /// if we allow it be resolved by allocating strings on the heap. When heap allocations are enabled, the amount of
    /// memory that can be used by the interner is effectively unlimited, as contexts that cannot be interned will be
    /// simply spill to the heap instead of being limited in any way.
    ///
    /// Defaults to `true`.
    pub fn with_heap_allocations(mut self, allow: bool) -> Self {
        self.allow_heap_allocations = Some(allow);
        self
    }

    /// Sets the tags resolver.
    pub fn with_tags_resolver(mut self, resolver: Option<TagsResolver>) -> Self {
        self.tags_resolver = resolver;
        self
    }

    /// Sets whether or not to enable telemetry for this resolver.
    ///
    /// Reporting the telemetry of the resolver requires running an asynchronous task to override adding additional
    /// overhead in the hot path of resolving contexts. In some cases, it may be cumbersome to always create the
    /// resolver in an asynchronous context so that the telemetry task can be spawned. This method allows disabling
    /// telemetry reporting in those cases.
    ///
    /// Defaults to telemetry enabled.
    pub fn without_telemetry(mut self) -> Self {
        self.telemetry_enabled = false;
        self
    }

    /// Configures a [`ContextResolverBuilder`] that is suitable for tests.
    ///
    /// This configures the builder with the following defaults:
    ///
    /// - resolver name of "noop"
    /// - unlimited cache capacity
    /// - no-op interner (all strings are heap-allocated)
    /// - heap allocations allowed
    /// - telemetry disabled
    ///
    /// This is generally only useful for testing purposes, and is exposed publicly in order to be used in cross-crate
    /// testing scenarios.
    pub fn for_tests() -> Self {
        ContextResolverBuilder::from_name("noop")
            .expect("resolver name not empty")
            .with_cached_contexts_limit(usize::MAX)
            .with_interner_capacity_bytes(NonZeroUsize::new(1).expect("not zero"))
            .with_heap_allocations(true)
            .with_tags_resolver(Some(TagsResolverBuilder::for_tests().build()))
            .without_telemetry()
    }

    /// Builds a [`ContextResolver`] from the current configuration.
    pub fn build(self) -> ContextResolver {
        let interner_capacity_bytes = self
            .interner_capacity_bytes
            .unwrap_or(DEFAULT_CONTEXT_RESOLVER_INTERNER_CAPACITY_BYTES);

        let interner = GenericMapInterner::new(interner_capacity_bytes);

        let cached_context_limit = self
            .cached_contexts_limit
            .unwrap_or(DEFAULT_CONTEXT_RESOLVER_CACHED_CONTEXTS_LIMIT);

        let allow_heap_allocations = self.allow_heap_allocations.unwrap_or(true);

        let telemetry = Telemetry::new(self.name.clone());
        telemetry
            .interner_capacity_bytes()
            .set(interner.capacity_bytes() as f64);

        // NOTE: We should switch to using a size-based weighter so that we can do more firm bounding of what we cache.
        let context_cache = CacheBuilder::from_identifier(format!("{}/contexts", self.name))
            .expect("cache identifier cannot possibly be empty")
            .with_capacity(cached_context_limit)
            .with_time_to_idle(self.idle_context_expiration)
            .with_hasher::<NoopU64BuildHasher>()
            .with_telemetry(self.telemetry_enabled)
            .build();

        if self.telemetry_enabled {
            tokio::spawn(drive_telemetry(interner.clone(), telemetry.clone()));
        }

        ContextResolver {
            telemetry,
            interner,
            caching_enabled: self.caching_enabled,
            context_cache,
            hash_seen_buffer: PrehashedHashSet::with_capacity_and_hasher(
                SEEN_HASHSET_INITIAL_CAPACITY,
                NoopU64BuildHasher,
            ),
            origin_tags_resolver: self.origin_tags_resolver,
            allow_heap_allocations,
            tags_resolver: self.tags_resolver,
        }
    }
}

/// A centralized store for resolved contexts.
///
/// Contexts are the combination of a name and a set of tags. They are used to identify a specific metric series. As contexts
/// are constructed entirely of strings, they are expensive to construct in a way that allows sending between tasks, as
/// this usually requires allocations. Even further, the same context may be "hot", used frequently by the
/// applications/services sending us metrics.
///
/// In order to optimize this, the context resolver is responsible for both interning the strings involved where
/// possible, as well as keeping a map of contexts that can be referred to with a cheap handle. We can cheaply search
/// for an existing context without needing to allocate an entirely new one, and get a clone of the handle to use going
/// forward.
///
/// # Design
///
/// `ContextResolver` specifically manages interning and mapping of contexts. It can be cheaply cloned itself.
///
/// In order to resolve a context, `resolve` must be called which requires taking a lock to check for an existing
/// context. A read/write lock is used in order to prioritize lookups over inserts, as lookups are expected to be more
/// common given how often a given context is used and resolved.
///
/// Once a context is resolved, a cheap handle -- `Context` -- is returned. This handle, like `ContextResolver`, can be
/// cheaply cloned. It points directly to the underlying context data (name and tags) and provides access to these
/// components.
pub struct ContextResolver {
    telemetry: Telemetry,
    interner: GenericMapInterner,
    caching_enabled: bool,
    context_cache: ContextCache,
    hash_seen_buffer: PrehashedHashSet<u64>,
    origin_tags_resolver: Option<Arc<dyn OriginTagsResolver>>,
    allow_heap_allocations: bool,
    tags_resolver: Option<TagsResolver>,
}

impl ContextResolver {
    fn intern<S>(&self, s: S) -> Option<MetaString>
    where
        S: AsRef<str> + CheapMetaString,
    {
        // Try to cheaply clone the string, and if that fails, try to intern it. If that fails, then we fall back to
        // allocating it on the heap if we allow it.
        s.try_cheap_clone()
            .or_else(|| self.interner.try_intern(s.as_ref()).map(MetaString::from))
            .or_else(|| {
                self.allow_heap_allocations.then(|| {
                    self.telemetry.intern_fallback_total().increment(1);
                    MetaString::from(s.as_ref())
                })
            })
    }

    fn create_context_key<N, I, I2, T, T2>(&mut self, name: N, tags: I, origin_tags: I2) -> (ContextKey, TagSetKey)
    where
        N: AsRef<str>,
        I: IntoIterator<Item = T>,
        T: AsRef<str>,
        I2: IntoIterator<Item = T2>,
        T2: AsRef<str>,
    {
        hash_context_with_seen(name.as_ref(), tags, origin_tags, &mut self.hash_seen_buffer)
    }

    fn create_context<N>(
        &self, key: ContextKey, name: N, context_tags: SharedTagSet, origin_tags: SharedTagSet,
    ) -> Option<Context>
    where
        N: AsRef<str> + CheapMetaString,
    {
        // Intern the name and tags of the context.
        let context_name = self.intern(name)?;

        self.telemetry.resolved_new_context_total().increment(1);
        self.telemetry.active_contexts().increment(1);

        Some(Context::from_inner(ContextInner::from_parts(
            key,
            context_name,
            context_tags,
            origin_tags,
            self.telemetry.active_contexts().clone(),
        )))
    }

    /// Resolves the given context.
    ///
    /// If the context has not yet been resolved, the name and tags are interned and a new context is created and
    /// stored. Otherwise, the existing context is returned. If an origin tags resolver is configured, and origin info
    /// is available, any enriched tags will be added to the context.
    ///
    /// `None` may be returned if the interner is full and outside allocations are disallowed. See
    /// `allow_heap_allocations` for more information.
    pub fn resolve<N, I, T>(&mut self, name: N, tags: I, maybe_origin: Option<RawOrigin<'_>>) -> Option<Context>
    where
        N: AsRef<str> + CheapMetaString,
        I: IntoIterator<Item = T> + Clone,
        T: AsRef<str> + CheapMetaString,
    {
        // Try and resolve our origin tags from the provided origin information, if any.
        let origin_tags = self
            .tags_resolver
            .as_ref()
            .map(|resolver| resolver.resolve_origin_tags(maybe_origin))
            .unwrap_or_default();

        self.resolve_inner(name, tags, origin_tags)
    }

    /// Resolves the given context using the provided origin tags.
    ///
    /// If the context has not yet been resolved, the name and tags are interned and a new context is created and
    /// stored. Otherwise, the existing context is returned. The provided origin tags are used to enrich the context.
    ///
    /// `None` may be returned if the interner is full and outside allocations are disallowed. See
    /// `allow_heap_allocations` for more information.
    ///
    /// ## Origin tags resolver mismatch
    ///
    /// When passing in origin tags, they will be inherently tied to a specific `OriginTagsResolver`, which may
    /// differ from the configured origin tags resolver in this context resolver. This means that the context that is
    /// generated and cached may not be reused in the future if an attempt is made to resolve it using the raw origin
    /// information instead.
    ///
    /// This method is intended primarily to allow for resolving contexts in a consistent way while _reusing_ the origin
    /// tags from another context, such as when remapping the name and/or instrumented tags of a given metric, while
    /// maintaining its origin association.
    pub fn resolve_with_origin_tags<N, I, T>(&mut self, name: N, tags: I, origin_tags: SharedTagSet) -> Option<Context>
    where
        N: AsRef<str> + CheapMetaString,
        I: IntoIterator<Item = T> + Clone,
        T: AsRef<str> + CheapMetaString,
    {
        self.resolve_inner(name, tags, origin_tags)
    }

    fn resolve_inner<N, I, T>(&mut self, name: N, tags: I, origin_tags: SharedTagSet) -> Option<Context>
    where
        N: AsRef<str> + CheapMetaString,
        I: IntoIterator<Item = T> + Clone,
        T: AsRef<str> + CheapMetaString,
    {
        let (context_key, tagset_key) = self.create_context_key(&name, tags.clone(), &origin_tags);

        // Fast path to avoid looking up the context in the cache if caching is disabled.
        if !self.caching_enabled {
            let tag_set = self
                .tags_resolver
                .as_mut()
                .and_then(|resolver| resolver.create_tag_set(tags))
                .unwrap_or_default();

            let context = self.create_context(context_key, name, tag_set, origin_tags)?;

            debug!(?context_key, ?context, "Resolved new non-cached context.");
            return Some(context);
        }

        match self.context_cache.get(&context_key) {
            Some(context) => {
                self.telemetry.resolved_existing_context_total().increment(1);
                Some(context)
            }
            None => {
                // Try seeing if we have the tagset cached already, and create it if not.
                let tag_set = match self
                    .tags_resolver
                    .as_mut()
                    .and_then(|resolver| resolver.get_tag_set(tagset_key))
                {
                    Some(tag_set) => {
                        self.telemetry.resolved_existing_tagset_total().increment(1);
                        tag_set
                    }
                    None => {
                        // If the tagset is not cached, we need to create it.
                        let tag_set = self
                            .tags_resolver
                            .as_mut()
                            .and_then(|resolver| resolver.create_tag_set(tags.clone()))
                            .unwrap_or_default();

                        if let Some(resolver) = self.tags_resolver.as_ref() {
                            resolver.insert_tag_set(tagset_key, tag_set.clone());
                        }

                        tag_set
                    }
                };

                let context = self.create_context(context_key, name, tag_set, origin_tags)?;
                self.context_cache.insert(context_key, context.clone());

                debug!(?context_key, ?context, "Resolved new context.");
                Some(context)
            }
        }
    }
}

impl Clone for ContextResolver {
    fn clone(&self) -> Self {
        Self {
            telemetry: self.telemetry.clone(),
            interner: self.interner.clone(),
            caching_enabled: self.caching_enabled,
            context_cache: self.context_cache.clone(),
            hash_seen_buffer: PrehashedHashSet::with_capacity_and_hasher(
                SEEN_HASHSET_INITIAL_CAPACITY,
                NoopU64BuildHasher,
            ),
            origin_tags_resolver: self.origin_tags_resolver.clone(),
            allow_heap_allocations: self.allow_heap_allocations,
            tags_resolver: self.tags_resolver.clone(),
        }
    }
}

async fn drive_telemetry(interner: GenericMapInterner, telemetry: Telemetry) {
    loop {
        sleep(Duration::from_secs(1)).await;

        telemetry.interner_entries().set(interner.len() as f64);
        telemetry
            .interner_capacity_bytes()
            .set(interner.capacity_bytes() as f64);
        telemetry.interner_len_bytes().set(interner.len_bytes() as f64);
    }
}

/// A builder for a tag resolver.
pub struct TagsResolverBuilder {
    name: String,
    caching_enabled: bool,
    cached_contexts_limit: Option<NonZeroUsize>,
    idle_context_expiration: Option<Duration>,
    interner_capacity_bytes: Option<NonZeroUsize>,
    allow_heap_allocations: Option<bool>,
    origin_tags_resolver: Option<Arc<dyn OriginTagsResolver>>,
    telemetry_enabled: bool,
}

impl TagsResolverBuilder {
    /// TODO: Document.
    pub fn from_name<S: Into<String>>(name: S) -> Result<Self, GenericError> {
        let name = name.into();
        if name.is_empty() {
            return Err(generic_error!("resolver name must not be empty"));
        }

        Ok(Self {
            name,
            caching_enabled: true,
            cached_contexts_limit: None,
            idle_context_expiration: None,
            interner_capacity_bytes: None,
            allow_heap_allocations: None,
            origin_tags_resolver: None,
            telemetry_enabled: true,
        })
    }

    /// TODO: Document.
    pub fn without_caching(mut self) -> Self {
        self.caching_enabled = false;
        self.idle_context_expiration = None;
        self
    }

    /// TODO: Document.
    pub fn with_cached_contexts_limit(mut self, limit: usize) -> Self {
        match NonZeroUsize::new(limit) {
            Some(limit) => {
                self.cached_contexts_limit = Some(limit);
                self
            }
            None => self.without_caching(),
        }
    }

    /// TODO: Document.
    pub fn with_idle_context_expiration(mut self, time_to_idle: Duration) -> Self {
        self.idle_context_expiration = Some(time_to_idle);
        self
    }

    /// TODO: Document.
    pub fn with_interner_capacity_bytes(mut self, capacity: NonZeroUsize) -> Self {
        self.interner_capacity_bytes = Some(capacity);
        self
    }

    /// TODO: Document.
    pub fn with_heap_allocations(mut self, allow: bool) -> Self {
        self.allow_heap_allocations = Some(allow);
        self
    }

    /// TODO: Document.
    pub fn with_origin_tags_resolver<R>(mut self, resolver: Option<R>) -> Self
    where
        R: OriginTagsResolver + 'static,
    {
        self.origin_tags_resolver = match resolver {
            Some(resolver) => {
                // We do in fact need this big match statement, instead of a simple `resolver.map(Arc::new)`, in order
                // to drive the compiler towards coercing our `Arc<R>` into `Arc<dyn OriginTagsResolver>`. ¯\_(ツ)_/¯
                let resolver = Arc::new(resolver);
                Some(resolver)
            }
            None => None,
        };
        self
    }

    /// TODO: Document.
    pub fn without_telemetry(mut self) -> Self {
        self.telemetry_enabled = false;
        self
    }

    /// Builds a [`TagsResolver`] from the current configuration.
    pub fn build(self) -> TagsResolver {
        let interner_capacity_bytes = self
            .interner_capacity_bytes
            .unwrap_or(DEFAULT_CONTEXT_RESOLVER_INTERNER_CAPACITY_BYTES);

        let interner = GenericMapInterner::new(interner_capacity_bytes);

        let cached_context_limit = self
            .cached_contexts_limit
            .unwrap_or(DEFAULT_CONTEXT_RESOLVER_CACHED_CONTEXTS_LIMIT);

        let allow_heap_allocations = self.allow_heap_allocations.unwrap_or(true);

        let telemetry = Telemetry::new(self.name.clone());
        telemetry
            .interner_capacity_bytes()
            .set(interner.capacity_bytes() as f64);

        let tagset_cache = CacheBuilder::from_identifier(format!("{}/tagsets", self.name))
            .expect("cache identifier cannot possibly be empty")
            .with_capacity(cached_context_limit)
            .with_time_to_idle(self.idle_context_expiration)
            .with_hasher::<NoopU64BuildHasher>()
            .with_telemetry(self.telemetry_enabled)
            .build();

        TagsResolver {
            telemetry,
            interner,
            caching_enabled: self.caching_enabled,
            tagset_cache,
            origin_tags_resolver: self.origin_tags_resolver,
            allow_heap_allocations,
        }
    }

    /// TODO: Document.
    pub fn for_tests() -> Self {
        TagsResolverBuilder::from_name("noop")
            .expect("resolver name not empty")
            .with_cached_contexts_limit(usize::MAX)
            .with_interner_capacity_bytes(NonZeroUsize::new(1).expect("not zero"))
            .with_heap_allocations(true)
            .without_telemetry()
    }
}

/// A resolver for tags.
pub struct TagsResolver {
    telemetry: Telemetry,
    interner: GenericMapInterner,
    caching_enabled: bool,
    tagset_cache: TagSetCache,
    origin_tags_resolver: Option<Arc<dyn OriginTagsResolver>>,
    allow_heap_allocations: bool,
}

impl TagsResolver {
    fn intern<S>(&self, s: S) -> Option<MetaString>
    where
        S: AsRef<str> + CheapMetaString,
    {
        // Try to cheaply clone the string, and if that fails, try to intern it. If that fails, then we fall back to
        // allocating it on the heap if we allow it.
        s.try_cheap_clone()
            .or_else(|| self.interner.try_intern(s.as_ref()).map(MetaString::from))
            .or_else(|| {
                self.allow_heap_allocations.then(|| {
                    self.telemetry.intern_fallback_total().increment(1);
                    MetaString::from(s.as_ref())
                })
            })
    }

    /// TODO: Document.
    pub fn create_tag_set<I, T>(&mut self, tags: I) -> Option<SharedTagSet>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<str> + CheapMetaString,
    {
        let mut context_tags = TagSet::default();
        for tag in tags {
            let context_tag = self.intern(tag)?;
            context_tags.insert_tag(context_tag);
        }

        self.telemetry.resolved_new_tagset_total().increment(1);

        Some(context_tags.into_shared())
    }

    /// TODO: Document.
    pub fn resolve_origin_tags(&self, maybe_origin: Option<RawOrigin<'_>>) -> SharedTagSet {
        self.origin_tags_resolver
            .as_ref()
            .and_then(|resolver| maybe_origin.map(|origin| resolver.resolve_origin_tags(origin)))
            .unwrap_or_default()
    }

    fn get_tag_set(&self, key: TagSetKey) -> Option<SharedTagSet> {
        self.tagset_cache.get(&key)
    }

    fn insert_tag_set(&self, key: TagSetKey, tag_set: SharedTagSet) {
        self.tagset_cache.insert(key, tag_set);
    }
}

impl Clone for TagsResolver {
    fn clone(&self) -> Self {
        Self {
            telemetry: self.telemetry.clone(),
            interner: self.interner.clone(),
            caching_enabled: self.caching_enabled,
            tagset_cache: self.tagset_cache.clone(),
            origin_tags_resolver: self.origin_tags_resolver.clone(),
            allow_heap_allocations: self.allow_heap_allocations,
        }
    }
}

#[cfg(test)]
mod tests {
    use metrics::{SharedString, Unit};
    use metrics_util::{
        debugging::{DebugValue, DebuggingRecorder},
        CompositeKey,
    };
    use saluki_common::hash::hash_single_fast;

    use super::*;

    fn get_gauge_value(metrics: &[(CompositeKey, Option<Unit>, Option<SharedString>, DebugValue)], key: &str) -> f64 {
        metrics
            .iter()
            .find(|(k, _, _, _)| k.key().name() == key)
            .map(|(_, _, _, value)| match value {
                DebugValue::Gauge(value) => value.into_inner(),
                other => panic!("expected a gauge, got: {:?}", other),
            })
            .unwrap_or_else(|| panic!("no metric found with key: {}", key))
    }

    struct DummyOriginTagsResolver;

    impl OriginTagsResolver for DummyOriginTagsResolver {
        fn resolve_origin_tags(&self, origin: RawOrigin<'_>) -> SharedTagSet {
            let origin_key = hash_single_fast(origin);

            let mut tags = TagSet::default();
            tags.insert_tag(format!("origin_key:{}", origin_key));
            tags.into_shared()
        }
    }

    #[test]
    fn basic() {
        let mut resolver = ContextResolverBuilder::for_tests().build();

        // Create two distinct contexts with the same name but different tags:
        let name = "metric_name";
        let tags1: [&str; 0] = [];
        let tags2 = ["tag1"];

        assert_ne!(&tags1[..], &tags2[..]);

        let context1 = resolver
            .resolve(name, &tags1[..], None)
            .expect("should not fail to resolve");
        let context2 = resolver
            .resolve(name, &tags2[..], None)
            .expect("should not fail to resolve");

        // The contexts should not be equal to each other, and should have distinct underlying pointers to the shared
        // context state:
        assert_ne!(context1, context2);
        assert!(!context1.ptr_eq(&context2));

        // If we create the context references again, we _should_ get back the same contexts as before:
        let context1_redo = resolver
            .resolve(name, &tags1[..], None)
            .expect("should not fail to resolve");
        let context2_redo = resolver
            .resolve(name, &tags2[..], None)
            .expect("should not fail to resolve");

        assert_ne!(context1_redo, context2_redo);
        assert_eq!(context1, context1_redo);
        assert_eq!(context2, context2_redo);
        assert!(context1.ptr_eq(&context1_redo));
        assert!(context2.ptr_eq(&context2_redo));
    }

    #[test]
    fn tag_order() {
        let mut resolver = ContextResolverBuilder::for_tests().build();

        // Create two distinct contexts with the same name and tags, but with the tags in a different order:
        let name = "metric_name";
        let tags1 = ["tag1", "tag2"];
        let tags2 = ["tag2", "tag1"];

        assert_ne!(&tags1[..], &tags2[..]);

        let context1 = resolver
            .resolve(name, &tags1[..], None)
            .expect("should not fail to resolve");
        let context2 = resolver
            .resolve(name, &tags2[..], None)
            .expect("should not fail to resolve");

        // The contexts should be equal to each other, and should have the same underlying pointer to the shared context
        // state:
        assert_eq!(context1, context2);
        assert!(context1.ptr_eq(&context2));
    }

    #[test]
    fn active_contexts() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        // Create our resolver and then create a context, which will have its metrics attached to our local recorder:
        let context = metrics::with_local_recorder(&recorder, || {
            let mut resolver = ContextResolverBuilder::for_tests().build();
            resolver
                .resolve("name", &["tag"][..], None)
                .expect("should not fail to resolve")
        });

        // We should be able to see that the active context count is one, representing the context we created:
        let metrics_before = snapshotter.snapshot().into_vec();
        let active_contexts = get_gauge_value(&metrics_before, Telemetry::active_contexts_name());
        assert_eq!(active_contexts, 1.0);

        // Now drop the context, and observe the active context count drop to zero:
        drop(context);
        let metrics_after = snapshotter.snapshot().into_vec();
        let active_contexts = get_gauge_value(&metrics_after, Telemetry::active_contexts_name());
        assert_eq!(active_contexts, 0.0);
    }

    #[test]
    fn duplicate_tags() {
        let mut resolver = ContextResolverBuilder::for_tests().build();

        // Two contexts with the same name, but each with a different set of duplicate tags:
        let name = "metric_name";
        let tags1 = ["tag1"];
        let tags1_duplicated = ["tag1", "tag1"];
        let tags2 = ["tag2"];
        let tags2_duplicated = ["tag2", "tag2"];

        let context1 = resolver
            .resolve(name, &tags1[..], None)
            .expect("should not fail to resolve");
        let context1_duplicated = resolver
            .resolve(name, &tags1_duplicated[..], None)
            .expect("should not fail to resolve");
        let context2 = resolver
            .resolve(name, &tags2[..], None)
            .expect("should not fail to resolve");
        let context2_duplicated = resolver
            .resolve(name, &tags2_duplicated[..], None)
            .expect("should not fail to resolve");

        // Each non-duplicated/duplicated context pair should be equal to one another:
        assert_eq!(context1, context1_duplicated);
        assert_eq!(context2, context2_duplicated);

        // Each pair should not be equal to the other pair, however.
        //
        // What we're asserting here is that, if we didn't handle duplicate tags correctly, the XOR hashing of [tag1,
        // tag1] and [tag2, tag2] would result in the same hash value, since the second duplicate hash of tag1/tag2
        // would cancel out the first... and thus all that would be left is the hash of the name itself, which is the
        // same in this test. This would lead to the contexts being equal, which is obviously wrong.
        //
        // If we're handling duplicates properly, then the resulting context hashes _shouldn't_ be equal.
        assert_ne!(context1, context2);
        assert_ne!(context1_duplicated, context2_duplicated);
        assert_ne!(context1, context2_duplicated);
        assert_ne!(context2, context1_duplicated);
    }

    #[test]
    fn differing_origins_with_without_resolver() {
        // Create a regular context resolver, without any origin tags resolver, which should result in contexts being
        // the same so long as the name and tags are the same, disregarding any difference in origin information:
        let mut resolver = ContextResolverBuilder::for_tests().build();

        let name = "metric_name";
        let tags = ["tag1"];
        let mut origin1 = RawOrigin::default();
        origin1.set_container_id("container1");
        let mut origin2 = RawOrigin::default();
        origin2.set_container_id("container2");

        let context1 = resolver
            .resolve(name, &tags[..], Some(origin1.clone()))
            .expect("should not fail to resolve");
        let context2 = resolver
            .resolve(name, &tags[..], Some(origin2.clone()))
            .expect("should not fail to resolve");

        assert_eq!(context1, context2);

        let tags_resolver = TagsResolverBuilder::for_tests()
            .with_origin_tags_resolver(Some(DummyOriginTagsResolver))
            .build();
        // Now build a context resolver with an origin tags resolver that trivially returns the hash of the origin info
        // as a tag, which should result in differeing sets of origin tags between the two origins, thus no longer
        // comparing as equal:
        let mut resolver = ContextResolverBuilder::for_tests()
            .with_tags_resolver(Some(tags_resolver))
            .build();

        let context1 = resolver
            .resolve(name, &tags[..], Some(origin1))
            .expect("should not fail to resolve");
        let context2 = resolver
            .resolve(name, &tags[..], Some(origin2))
            .expect("should not fail to resolve");

        assert_ne!(context1, context2);
    }

    #[test]
    fn caching_disabled() {
        let tags_resolver = TagsResolverBuilder::for_tests()
            .with_origin_tags_resolver(Some(DummyOriginTagsResolver))
            .build();
        let mut resolver = ContextResolverBuilder::for_tests()
            .without_caching()
            .with_tags_resolver(Some(tags_resolver))
            .build();

        let name = "metric_name";
        let tags = ["tag1"];
        let mut origin1 = RawOrigin::default();
        origin1.set_container_id("container1");

        // Create a context with caching disabled, and verify that the context is not cached:
        let context1 = resolver
            .resolve(name, &tags[..], Some(origin1.clone()))
            .expect("should not fail to resolve");
        assert_eq!(resolver.context_cache.len(), 0);

        // Create a second context with the same name and tags, and verify that it is not cached:
        let context2 = resolver
            .resolve(name, &tags[..], Some(origin1))
            .expect("should not fail to resolve");
        assert_eq!(resolver.context_cache.len(), 0);

        // The contexts should be equal to each other, but the underlying `Arc` pointers should be different since
        // they're two distinct contexts in terms of not being cached:
        assert_eq!(context1, context2);
        assert!(!context1.ptr_eq(&context2));
    }

    #[test]
    fn cheaply_cloneable_name_and_tags() {
        const BIG_TAG_ONE: &str = "long-tag-that-cannot-be-inlined-just-to-be-doubly-sure-on-top-of-being-static";
        const BIG_TAG_TWO: &str = "another-long-boye-that-we-are-also-sure-wont-be-inlined-and-we-stand-on-that";

        // Create a context resolver with a proper string interner configured:
        let mut resolver = ContextResolverBuilder::for_tests()
            .with_interner_capacity_bytes(NonZeroUsize::new(1024).expect("not zero"))
            .build();

        // Create our context with cheaply cloneable tags, aka static strings:
        let name = MetaString::from_static("long-metric-name-that-shouldnt-be-inlined-and-should-end-up-interned");
        let tags = [
            MetaString::from_static(BIG_TAG_ONE),
            MetaString::from_static(BIG_TAG_TWO),
        ];
        assert!(tags[0].is_cheaply_cloneable());
        assert!(tags[1].is_cheaply_cloneable());

        // Make sure the interner is empty before we resolve the context, and that it's empty afterwards, since we
        // should be able to cheaply clone both the metric name and both tags:
        assert_eq!(resolver.interner.len(), 0);
        assert_eq!(resolver.interner.len_bytes(), 0);

        let context = resolver
            .resolve(&name, &tags[..], None)
            .expect("should not fail to resolve");
        assert_eq!(resolver.interner.len(), 0);
        assert_eq!(resolver.interner.len_bytes(), 0);

        // And just a sanity check that we have the expected name and tags in the context:
        assert_eq!(context.name(), &name);

        let context_tags = context.tags();
        assert_eq!(context_tags.len(), 2);
        assert!(context_tags.has_tag(&tags[0]));
        assert!(context_tags.has_tag(&tags[1]));
    }
}
