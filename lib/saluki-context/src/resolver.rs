use std::{num::NonZeroUsize, sync::Arc, time::Duration};

use quick_cache::{sync::Cache, UnitWeighter};
use saluki_common::{collections::PrehashedHashSet, hash::NoopU64BuildHasher};
use saluki_error::{generic_error, GenericError};
use saluki_metrics::static_metrics;
use stringtheory::{interning::GenericMapInterner, MetaString};
use tokio::time::sleep;
use tracing::debug;

use crate::{
    context::{Context, ContextInner},
    expiry::{Expiration, ExpirationBuilder, ExpiryCapableLifecycle},
    hash::{hash_context_with_seen, ContextKey},
    origin::{OriginKey, OriginTags, OriginTagsResolver, RawOrigin},
    tags::TagSet,
};

const DEFAULT_CONTEXT_RESOLVER_CACHED_CONTEXTS_LIMIT: usize = 500_000;

// SAFETY: We know, unquestionably, that this value is not zero.
const DEFAULT_CONTEXT_RESOLVER_INTERNER_CAPACITY_BYTES: NonZeroUsize =
    unsafe { NonZeroUsize::new_unchecked(2 * 1024 * 1024) };

const SEEN_HASHSET_INITIAL_CAPACITY: usize = 128;

type ContextCache = Cache<ContextKey, Context, UnitWeighter, NoopU64BuildHasher, ExpiryCapableLifecycle>;

static_metrics! {
    name => Telemetry,
    prefix => context_resolver,
    labels => [resolver_id: String],
    metrics => [
        counter(resolved_existing_context_total),
        counter(resolved_new_context_total),
        gauge(active_contexts),
        gauge(cached_contexts),
        gauge(interner_capacity_bytes),
        gauge(interner_len_bytes),
        gauge(interner_entries),
        counter(intern_fallback_total),
        counter(contexts_expired),
        histogram(contexts_expired_batch_size),
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
    cached_contexts_limit: Option<usize>,
    idle_context_expiration: Option<Duration>,
    expiration_interval: Option<Duration>,
    interner_capacity_bytes: Option<NonZeroUsize>,
    allow_heap_allocations: Option<bool>,
    origin_tags_resolver: Option<Arc<dyn OriginTagsResolver>>,
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
            expiration_interval: None,
            interner_capacity_bytes: None,
            allow_heap_allocations: None,
            origin_tags_resolver: None,
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
    /// Defaults to 500,000.
    pub fn with_cached_contexts_limit(mut self, limit: usize) -> Self {
        self.cached_contexts_limit = Some(limit);
        self
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

    /// Sets the interval at which the expiration process will run.
    ///
    /// This controls how often the expiration process will run to check for expired contexts. While contexts become
    /// _eligible_ for expiration after being idle for the time-to-idle duration, they are not _guaranteed_ to be
    /// removed immediately: the expiration process must still run to actually find the eligible contexts and remove them.
    ///
    /// This means that the rough upper bound for how long a context may be kept alive after going idle is the sum of
    /// both the time-to-idle value and the expiration interval.
    ///
    /// This value is only relevant if the idle context expiration was set.
    ///
    /// Defaults to 1 second.
    pub fn with_expiration_interval(mut self, expiration_interval: Duration) -> Self {
        self.expiration_interval = Some(expiration_interval);
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

    /// Sets the origin tags resolver to use when building a context.
    ///
    /// In some cases, metrics may have enriched tags based on their origin -- the application/host/container/etc that
    /// emitted the metric -- which has to be considered when build the context itself. As this can be expensive, it is
    /// useful to split the logic of actually grabbing the enriched tags based on the available origin info into a
    /// separate phase, and implementation, that can run separately from the initial hash-based approach of checking if
    /// a context has already been resolved.
    ///
    /// When set, any origin information provided will be considered during hashing when looking up a context, and any
    /// enriched tags attached to the detected origin will be accessible from the context.
    ///
    /// Defaults to unset.
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
    /// - unlimited context cache size
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
            .without_telemetry()
    }

    /// Builds a [`ContextResolver`] from the current configuration.
    pub fn build(self) -> ContextResolver {
        let interner_capacity_bytes = self
            .interner_capacity_bytes
            .unwrap_or(DEFAULT_CONTEXT_RESOLVER_INTERNER_CAPACITY_BYTES);

        let cached_context_limit = self
            .cached_contexts_limit
            .unwrap_or(DEFAULT_CONTEXT_RESOLVER_CACHED_CONTEXTS_LIMIT);

        let allow_heap_allocations = self.allow_heap_allocations.unwrap_or(true);

        let interner = GenericMapInterner::new(interner_capacity_bytes);

        let telemetry = Telemetry::new(self.name);
        telemetry
            .interner_capacity_bytes()
            .set(interner.capacity_bytes() as f64);

        let mut builder = ExpirationBuilder::new();
        if let Some(time_to_idle) = self.idle_context_expiration {
            if self.caching_enabled {
                builder = builder.with_time_to_idle(time_to_idle);
            }
        }
        let (expiration, lifecycle) = builder.build();

        // NOTE: We specifically use the cached context limit for both the estimated items capacity _and_ weight
        // capacity, where weight capacity relates to "maximum size in bytes", because we're using the unit weighter,
        // which counts every cache entry as a weight of one.
        //
        // In the future, if we wanted to weight contexts differently -- heap-allocated contexts "weigh" more than
        // fully-interned contexts, etc -- then we would want to expose those, but for now, it's simpler to have users
        // simply configure a larger interner rather than having to consider the trade-offs between configuring the
        // interner capacity _and_ the overall cached contexts capacity, etc.
        let context_cache = Arc::new(ContextCache::with(
            cached_context_limit,
            cached_context_limit as u64,
            UnitWeighter,
            NoopU64BuildHasher,
            lifecycle,
        ));

        // If an idle context expiration was specified, spawn a background task to actually drive expiration.
        if let Some(expiration_interval) = self.expiration_interval {
            if self.caching_enabled {
                let context_cache = Arc::clone(&context_cache);
                let expiration = expiration.clone();
                tokio::spawn(drive_expiration(
                    context_cache,
                    telemetry.clone(),
                    expiration,
                    expiration_interval,
                ));
            }
        }

        if self.telemetry_enabled {
            tokio::spawn(drive_telemetry(
                Arc::clone(&context_cache),
                interner.clone(),
                telemetry.clone(),
            ));
        }

        ContextResolver {
            telemetry,
            interner,
            caching_enabled: self.caching_enabled,
            context_cache,
            expiration,
            hash_seen_buffer: PrehashedHashSet::with_capacity_and_hasher(
                SEEN_HASHSET_INITIAL_CAPACITY,
                NoopU64BuildHasher,
            ),
            origin_tags_resolver: self.origin_tags_resolver,
            allow_heap_allocations,
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
    context_cache: Arc<ContextCache>,
    expiration: Expiration,
    hash_seen_buffer: PrehashedHashSet<u64>,
    origin_tags_resolver: Option<Arc<dyn OriginTagsResolver>>,
    allow_heap_allocations: bool,
}

impl ContextResolver {
    fn intern(&self, s: &str) -> Option<MetaString> {
        // First we'll see if we can inline the string, and if we can't, then we try to actually intern it. If interning
        // fails, then we just fall back to allocating a new `MetaString` instance.
        MetaString::try_inline(s)
            .or_else(|| self.interner.try_intern(s).map(MetaString::from))
            .or_else(|| {
                self.allow_heap_allocations.then(|| {
                    self.telemetry.intern_fallback_total().increment(1);
                    MetaString::from(s)
                })
            })
    }

    fn resolve_origin_tags(&self, maybe_origin: Option<RawOrigin<'_>>) -> OriginTags {
        self.origin_tags_resolver
            .as_ref()
            .and_then(|resolver| {
                maybe_origin
                    .and_then(|origin| resolver.resolve_origin_key(origin))
                    .map(|origin_key| OriginTags::from_resolved(origin_key, Arc::clone(resolver)))
            })
            .unwrap_or_else(OriginTags::empty)
    }

    fn create_context_key<I, T>(&mut self, name: &str, tags: I, maybe_origin_key: Option<OriginKey>) -> ContextKey
    where
        I: IntoIterator<Item = T>,
        T: AsRef<str>,
    {
        hash_context_with_seen(name, tags, maybe_origin_key, &mut self.hash_seen_buffer)
    }

    fn create_context<I, T>(&self, key: ContextKey, name: &str, tags: I, origin_tags: OriginTags) -> Option<Context>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<str>,
    {
        // Intern the name and tags of the context.
        let context_name = self.intern(name)?;

        let mut context_tags = TagSet::default();
        for tag in tags {
            let tag = self.intern(tag.as_ref())?;
            context_tags.insert_tag(tag);
        }

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
    pub fn resolve<I, T>(&mut self, name: &str, tags: I, maybe_origin: Option<RawOrigin<'_>>) -> Option<Context>
    where
        I: IntoIterator<Item = T> + Clone,
        T: AsRef<str>,
    {
        // Try and resolve our origin tags from the provided origin information, if any.
        let origin_tags = self.resolve_origin_tags(maybe_origin);

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
    pub fn resolve_with_origin_tags<I, T>(&mut self, name: &str, tags: I, origin_tags: OriginTags) -> Option<Context>
    where
        I: IntoIterator<Item = T> + Clone,
        T: AsRef<str>,
    {
        self.resolve_inner(name, tags, origin_tags)
    }

    fn resolve_inner<I, T>(&mut self, name: &str, tags: I, origin_tags: OriginTags) -> Option<Context>
    where
        I: IntoIterator<Item = T> + Clone,
        T: AsRef<str>,
    {
        let context_key = self.create_context_key(name, tags.clone(), origin_tags.key());

        // Fast path to avoid looking up the context in the cache if caching is disabled.
        if !self.caching_enabled {
            let context = self.create_context(context_key, name, tags, origin_tags)?;

            debug!(?context_key, ?context, "Resolved new non-cached context.");
            return Some(context)
        }

        match self.context_cache.get(&context_key) {
            Some(context) => {
                self.telemetry.resolved_existing_context_total().increment(1);
                self.expiration.mark_entry_accessed(context_key);

                Some(context)
            }
            None => {
                let context = self.create_context(context_key, name, tags, origin_tags)?;

                self.context_cache.insert(context_key, context.clone());
                self.expiration.mark_entry_accessed(context_key);

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
            context_cache: Arc::clone(&self.context_cache),
            expiration: self.expiration.clone(),
            hash_seen_buffer: PrehashedHashSet::with_capacity_and_hasher(
                SEEN_HASHSET_INITIAL_CAPACITY,
                NoopU64BuildHasher,
            ),
            origin_tags_resolver: self.origin_tags_resolver.clone(),
            allow_heap_allocations: self.allow_heap_allocations,
        }
    }
}

async fn drive_expiration(
    context_cache: Arc<ContextCache>, telemetry: Telemetry, expiration: Expiration, expiration_interval: Duration,
) {
    let mut expired_entries = Vec::new();

    loop {
        sleep(expiration_interval).await;

        expiration.drain_expired_entries(&mut expired_entries);

        let num_expired_contexts = expired_entries.len();
        if num_expired_contexts != 0 {
            telemetry.contexts_expired().increment(num_expired_contexts as u64);
            telemetry
                .contexts_expired_batch_size()
                .record(num_expired_contexts as f64);
        }

        debug!(num_expired_contexts, "Found expired contexts.");

        for entry in expired_entries.drain(..) {
            context_cache.remove(&entry);
        }

        debug!(num_expired_contexts, "Removed expired contexts.");
    }
}

async fn drive_telemetry(context_cache: Arc<ContextCache>, interner: GenericMapInterner, telemetry: Telemetry) {
    loop {
        sleep(Duration::from_secs(1)).await;

        telemetry.interner_entries().set(interner.len() as f64);
        telemetry
            .interner_capacity_bytes()
            .set(interner.capacity_bytes() as f64);
        telemetry.interner_len_bytes().set(interner.len_bytes() as f64);

        telemetry.cached_contexts().set(context_cache.len() as f64);
    }
}

#[cfg(test)]
mod tests {
    use metrics::{SharedString, Unit};
    use metrics_util::{
        debugging::{DebugValue, DebuggingRecorder},
        CompositeKey,
    };

    use super::*;
    use crate::tags::TagVisitor;

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
        fn resolve_origin_key(&self, info: RawOrigin<'_>) -> Option<OriginKey> {
            Some(OriginKey::from_opaque(info))
        }

        fn visit_origin_tags(&self, _: OriginKey, _: &mut dyn TagVisitor) {}
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

        // Now build a context resolver with an origin tags resolver that trivially returns the origin key based on the
        // hash of the origin info, which should result in the contexts incorporating the origin information into their
        // equality/hashing, thus no longer comparing as equal:
        let mut resolver = ContextResolverBuilder::for_tests()
            .with_origin_tags_resolver(Some(DummyOriginTagsResolver))
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
        let mut resolver = ContextResolverBuilder::for_tests()
            .without_caching()
            .with_origin_tags_resolver(Some(DummyOriginTagsResolver))
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
}
