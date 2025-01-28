use std::{num::NonZeroUsize, sync::Arc, time::Duration};

use quick_cache::{sync::Cache, UnitWeighter};
use saluki_error::{generic_error, GenericError};
use saluki_metrics::static_metrics;
use stringtheory::{interning::GenericMapInterner, MetaString};
use tokio::time::sleep;
use tracing::debug;

use crate::{
    context::{Context, ContextInner},
    expiry::{Expiration, ExpirationBuilder, ExpiryCapableLifecycle},
    hash::{hash_context_with_seen, new_fast_hashset, ContextKey, FastHashSet},
    origin::{OriginInfo, OriginTags, OriginTagsResolver},
    tags::TagSet,
};

const DEFAULT_CONTEXT_RESOLVER_CACHED_CONTEXTS_LIMIT: usize = 500_000;

// SAFETY: We know, unquestionably, that this value is not zero.
const DEFAULT_CONTEXT_RESOLVER_INTERNER_CAPACITY_BYTES: NonZeroUsize =
    unsafe { NonZeroUsize::new_unchecked(2 * 1024 * 1024) };

type ContextCache = Cache<ContextKey, Context, UnitWeighter, ahash::RandomState, ExpiryCapableLifecycle<ContextKey>>;

static_metrics! {
    name => Statistics,
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
        counter(intern_fallback_total)
    ],
}

/// Builder for creating a [`ContextResolver`].
///
/// # Missing
///
/// - Support for configuring the size limit of cached contexts. (See note in [`ContextResolver::new`])
pub struct ContextResolverBuilder {
    name: String,
    cached_contexts_limit: Option<usize>,
    idle_context_expiration: Option<Duration>,
    expiration_interval: Option<Duration>,
    interner_capacity_bytes: Option<NonZeroUsize>,
    allow_heap_allocations: Option<bool>,
    origin_tags_resolver: Option<Arc<dyn OriginTagsResolver>>,
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
            cached_contexts_limit: None,
            idle_context_expiration: None,
            expiration_interval: None,
            interner_capacity_bytes: None,
            allow_heap_allocations: None,
            origin_tags_resolver: None,
        })
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
    /// Defaults to being disabled.
    pub fn with_origin_tags_resolver<R>(mut self, resolver: R) -> Self
    where
        R: OriginTagsResolver + 'static,
    {
        self.origin_tags_resolver = Some(Arc::new(resolver));
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
    ///
    /// This is generally only useful for testing purposes, and is exposed publicly in order to be used in cross-crate
    /// testing scenarios.
    pub fn for_tests() -> ContextResolverBuilder {
        ContextResolverBuilder::from_name("noop")
            .expect("resolver name not empty")
            .with_cached_contexts_limit(usize::MAX)
            .with_interner_capacity_bytes(NonZeroUsize::new(1).expect("not zero"))
            .with_heap_allocations(true)
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

        let stats = Statistics::new(self.name);
        stats.interner_capacity_bytes().set(interner.capacity_bytes() as f64);

        let mut builder = ExpirationBuilder::new();
        if let Some(time_to_idle) = self.idle_context_expiration {
            builder = builder.with_time_to_idle(time_to_idle);
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
            ahash::RandomState::default(),
            lifecycle,
        ));

        // If an idle context expiration was specified, spawn a background task to actually drive expiration.
        if let Some(expiration_interval) = self.expiration_interval {
            let context_cache = Arc::clone(&context_cache);
            let expiration = expiration.clone();
            tokio::spawn(drive_expiration(
                context_cache,
                stats.clone(),
                expiration,
                expiration_interval,
            ));
        }

        ContextResolver {
            stats,
            interner,
            context_cache,
            expiration,
            hash_seen_buffer: new_fast_hashset(),
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
    stats: Statistics,
    interner: GenericMapInterner,
    context_cache: Arc<ContextCache>,
    expiration: Expiration<ContextKey>,
    hash_seen_buffer: FastHashSet<u64>,
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
                    self.stats.intern_fallback_total().increment(1);
                    MetaString::from(s)
                })
            })
    }

    fn create_context_key<I, T>(&mut self, name: &str, tags: I, origin_info: Option<OriginInfo<'_>>) -> ContextKey
    where
        I: IntoIterator<Item = T>,
        T: AsRef<str>,
    {
        // If we have an origin enricher configured, and the resolvable value has origin information defined, attempt to
        // look up the origin key for it. We'll pass that along to the hasher to include as part of the context key.
        let origin_key = self
            .origin_tags_resolver
            .as_ref()
            .and_then(|resolver| origin_info.and_then(|info| resolver.resolve_origin_key(info)));

        hash_context_with_seen(name, tags, origin_key, &mut self.hash_seen_buffer)
    }

    fn create_context<I, T>(&self, key: ContextKey, name: &str, tags: I) -> Option<Context>
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

        // Collect any enriched tags based on the origin key of the context, if any.
        let origin_tags = match self.origin_tags_resolver.as_ref() {
            Some(resolver) => key
                .origin_key()
                .map(|key| OriginTags::from_resolved(key, Arc::clone(resolver)))
                .unwrap_or_else(OriginTags::empty),
            None => OriginTags::empty(),
        };

        self.stats.resolved_new_context_total().increment(1);

        Some(Context::from_inner(ContextInner {
            name: context_name,
            tags: context_tags,
            origin_tags,
            key,
            active_count: self.stats.active_contexts().clone(),
        }))
    }

    /// Resolves the given context.
    ///
    /// If the context has not yet been resolved, the name and tags are interned and a new context is created and
    /// stored. Otherwise, the existing context is returned. If an origin enricher is configured, and origin info is
    /// available, any enriched tags will be added to the context.
    ///
    /// `None` may be returned if the interner is full and outside allocations are disallowed. See
    /// `allow_heap_allocations` for more information.
    pub fn resolve<I, T>(&mut self, name: &str, tags: I, origin_info: Option<OriginInfo<'_>>) -> Option<Context>
    where
        I: IntoIterator<Item = T> + Clone,
        T: AsRef<str>,
    {
        let context_key = self.create_context_key(name, tags.clone(), origin_info);
        match self.context_cache.get(&context_key) {
            Some(context) => {
                self.stats.resolved_existing_context_total().increment(1);
                self.expiration.mark_entry_accessed(context_key);
                Some(context)
            }
            None => match self.create_context(context_key, name, tags) {
                Some(context) => {
                    debug!(?context_key, ?context, "Resolved new context.");

                    self.context_cache.insert(context_key, context.clone());
                    self.expiration.mark_entry_accessed(context_key);

                    // TODO: This is lazily updated during resolve, which means this metric might lag behind the actual
                    // count as interned strings are dropped/reclaimed... but we don't have a way to figure out if a given
                    // `MetaString` is an interned string and if dropping it would actually reclaim the interned string...
                    // so this is our next best option short of instrumenting `GenericMapInterner` directly.
                    //
                    // We probably want to do that in the future, but this is just a little cleaner without adding extra
                    // fluff to `GenericMapInterner` which is already complex as-is.
                    self.stats.interner_entries().set(self.interner.len() as f64);
                    self.stats.interner_len_bytes().set(self.interner.len_bytes() as f64);
                    self.stats.resolved_new_context_total().increment(1);
                    self.stats.active_contexts().increment(1);

                    // TODO: This is crappy to have to do every time we resolve, because we need to read all cache
                    // shards. Realistically, what we could do -- to avoid having to do this as a background task --
                    // would be to increment the `cached_contexts` metric here and then decrement it when an entry is
                    // evicted, by pushing that logic into the lifecycle implementation.
                    //
                    // That wouldn't cover direct removals, though, which we only do as a result of expiration, to be
                    // fair... but would still be a little janky as well.
                    self.stats.cached_contexts().set(self.context_cache.len() as f64);

                    Some(context)
                }
                None => None,
            },
        }
    }
}

impl Clone for ContextResolver {
    fn clone(&self) -> Self {
        Self {
            stats: self.stats.clone(),
            interner: self.interner.clone(),
            context_cache: Arc::clone(&self.context_cache),
            expiration: self.expiration.clone(),
            hash_seen_buffer: new_fast_hashset(),
            origin_tags_resolver: self.origin_tags_resolver.clone(),
            allow_heap_allocations: self.allow_heap_allocations,
        }
    }
}

async fn drive_expiration(
    context_cache: Arc<ContextCache>, stats: Statistics, expiration: Expiration<ContextKey>,
    expiration_interval: Duration,
) {
    let mut expired_entries = Vec::new();

    loop {
        sleep(expiration_interval).await;

        expiration.drain_expired_entries(&mut expired_entries);

        let num_expired_contexts = expired_entries.len();
        debug!(num_expired_contexts, "Found expired contexts.");

        for entry in expired_entries.drain(..) {
            context_cache.remove(&entry);
        }

        debug!(num_expired_contexts, "Removed expired contexts.");

        stats.cached_contexts().set(context_cache.len() as f64);
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
    use crate::origin::{OriginKey, OriginTagVisitor};

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
        fn resolve_origin_key(&self, info: OriginInfo<'_>) -> Option<OriginKey> {
            Some(OriginKey::from_opaque(info))
        }

        fn visit_origin_tags(&self, _: OriginKey, _: &mut dyn OriginTagVisitor) {}
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
        let active_contexts = get_gauge_value(&metrics_before, Statistics::active_contexts_name());
        assert_eq!(active_contexts, 1.0);

        // Now drop the context, and observe the active context count drop to zero:
        drop(context);
        let metrics_after = snapshotter.snapshot().into_vec();
        let active_contexts = get_gauge_value(&metrics_after, Statistics::active_contexts_name());
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
        let mut origin1 = OriginInfo::default();
        origin1.set_container_id("container1");
        let mut origin2 = OriginInfo::default();
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
            .with_origin_tags_resolver(DummyOriginTagsResolver)
            .build();

        let context1 = resolver
            .resolve(name, &tags[..], Some(origin1))
            .expect("should not fail to resolve");
        let context2 = resolver
            .resolve(name, &tags[..], Some(origin2))
            .expect("should not fail to resolve");

        assert_ne!(context1, context2);
    }
}
