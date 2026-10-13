use std::{num::NonZeroUsize, sync::Arc, time::Duration};

use saluki_common::{collections::PrehashedHashSet, hash::NoopU64BuildHasher, time::get_coarse_unix_timestamp};
use saluki_error::{generic_error, GenericError};
use saluki_metrics::{static_metrics, Counter, Gauge};
use stringtheory::{
    interning::{GenericMapInterner, Interner as _},
    CheapMetaString, MetaString,
};
use tokio::time::sleep;
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::debug;

use super::{
    front::{FrontCache, Lookup},
    hash::{hash_context_with_host_and_seen, ContextKey, TagSetKey},
    Context, ContextInner,
};
use crate::{
    cache::{
        weight::ItemCountWeighter, AccumulatedCounters, Cache, CacheBuilder, ChangeStamps, GaugeContribution,
        GaugeMode, SummedGauge,
    },
    data_model::{
        origin::{OriginTagsResolver, RawOrigin},
        tags::{SharedTagSet, TagSet},
    },
};

// SAFETY: We know, unquestionably, that this value is not zero.
const DEFAULT_CONTEXT_RESOLVER_CACHED_CONTEXTS_LIMIT: NonZeroUsize = NonZeroUsize::new(500_000).unwrap();

// SAFETY: We know, unquestionably, that this value is not zero.
const DEFAULT_CONTEXT_RESOLVER_INTERNER_CAPACITY_BYTES: NonZeroUsize = NonZeroUsize::new(2 * 1024 * 1024).unwrap();

const SEEN_HASHSET_INITIAL_CAPACITY: usize = 128;

pub(super) type ContextCache = Cache<ContextKey, Context, ItemCountWeighter, NoopU64BuildHasher>;
type TagSetCache = Cache<TagSetKey, SharedTagSet, ItemCountWeighter, NoopU64BuildHasher>;

#[static_metrics(prefix = context_resolver, labels(resolver_id))]
#[derive(Clone)]
struct Telemetry {
    interner_capacity_bytes: Gauge,
    interner_len_bytes: Gauge,
    interner_entries: Gauge,
    #[metric(level = debug)]
    intern_fallback_total: Counter,
    #[metric(level = debug)]
    resolved_existing_context_total: Counter,
    #[metric(level = debug)]
    resolved_new_context_total: Counter,
    active_contexts: Gauge,
    #[metric(level = debug)]
    resolved_existing_tagset_total: Counter,
    #[metric(level = debug)]
    resolved_new_tagset_total: Counter,
}

/// Builder for creating a [`ContextResolver`].
///
/// # Missing
///
/// - Support for configuring the size limit of cached contexts.
pub struct ContextResolverBuilder {
    name: String,
    caching_enabled: bool,
    cached_contexts_limit: Option<NonZeroUsize>,
    idle_context_expiration: Option<Duration>,
    interner_capacity_bytes: Option<NonZeroUsize>,
    allow_heap_allocations: Option<bool>,
    tags_resolver: Option<TagsResolver>,
    interner: Option<GenericMapInterner>,
    origin_tags_resolver: Option<Arc<dyn OriginTagsResolver>>,
    telemetry_enabled: bool,
    accumulated_telemetry: bool,
    gauges: GaugeMode,
    front_caches: bool,
}

impl ContextResolverBuilder {
    /// Creates a new `ContextResolverBuilder` with the given resolver name.
    ///
    /// The resolver name _should_ be unique, but it isn't required to be. Metrics for the resolver will be
    /// emitted using the given name, so in cases where the name isn't unique, those metrics will be aggregated
    /// together and it won't be possible to distinguish between the different resolvers.
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
            tags_resolver: None,
            interner: None,
            origin_tags_resolver: None,
            telemetry_enabled: true,
            accumulated_telemetry: false,
            gauges: GaugeMode::Set,
            front_caches: false,
        })
    }

    /// Sets whether or not to enable caching of resolved contexts.
    ///
    /// [`ContextResolver`] provides two main benefits: consistent behavior for resolving contexts (interning, origin
    /// tags, etc), and the caching of those resolved contexts to speed up future resolutions. However, caching contexts
    /// means that we pay a memory cost for the cache itself, even if the contexts aren't ever reused or are seen
    /// infrequently. While expiration can help free up cache capacity, it can't help recover the memory used by the
    /// underlying cache data structure once they have expanded to hold the contexts.
    ///
    /// Disabling caching allows normal resolving to take place without the overhead of caching the contexts. This can
    /// lead to lower average memory usage, as contexts will only live as long as they're needed, but it will reduce
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
    /// This is the maximum number of resolved contexts that can be cached at any given time. This limit doesn't affect
    /// the total number of contexts that can be _alive_ at any given time, which is dependent on the interner capacity
    /// and whether or not heap allocations are allowed.
    ///
    /// Caching contexts is beneficial when the same context is resolved frequently, and it's generally worth
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
    /// interner won't exceed the configured capacity when allocating any backing storage.
    ///
    /// This value directly impacts the number of contexts that can be resolved when heap allocations are disabled, as
    /// all resolved contexts must either have values (name or tags) that can be inlined or interned. Once the interner
    /// is full, contexts may fail to be resolved if heap allocations are disabled.
    ///
    /// The optimal value will almost always be workload-dependent, but a good starting point can be to estimate around
    /// 150 - 200 bytes per context based on empirical measurements around common metric name and tag lengths. This
    /// translate to around 5000 unique contexts per 1 MB of interner size.
    ///
    /// Defaults to 2 MB.
    pub fn with_interner_capacity_bytes(mut self, capacity: NonZeroUsize) -> Self {
        self.interner_capacity_bytes = Some(capacity);
        self
    }

    /// Sets whether or not to allow heap allocations when interning strings.
    ///
    /// In cases where the interner is full, this setting determines whether or not we refuse to resolve a context, or
    /// if we allow it be resolved by allocating strings on the heap. When heap allocations are enabled, the amount of
    /// memory that can be used by the interner is effectively unlimited, as contexts that can't be interned will be
    /// simply spill to the heap instead of being limited in any way.
    ///
    /// Defaults to `true`.
    pub fn with_heap_allocations(mut self, allow: bool) -> Self {
        self.allow_heap_allocations = Some(allow);
        self
    }

    /// Sets the tags resolver.
    ///
    /// Defaults to unset.
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

    /// Makes the resolver count what it resolves, and its caches what they look up, in counters of their own, which
    /// [`ContextResolver::publish_telemetry`] adds to their telemetry, instead of writing the telemetry on every
    /// resolution.
    ///
    /// Resolvers with one name share their telemetry, so resolvers used in parallel, such as one per task of a
    /// component, would contend on it on every resolution. Whatever owns the resolver publishes it periodically, about
    /// once a second; see [`CacheBuilder::with_accumulated_telemetry`]. The tags resolver the resolver creates
    /// accumulates too; one set with [`with_tags_resolver`][Self::with_tags_resolver] keeps its own setting. The gauge
    /// of active contexts, which every context updates when it is dropped, is written as before.
    ///
    /// Defaults to writing the telemetry on every resolution.
    pub fn with_accumulated_telemetry(mut self) -> Self {
        self.accumulated_telemetry = true;
        self
    }

    /// Sets how the resolver reports its interner's capacity, length, and entries, and its caches their items, weight,
    /// and limits: set to its own values, or summed with the other resolvers of its name (see [`GaugeMode`]).
    ///
    /// A summed resolver adds its interner's capacity when it is built, and its length and entries once a second, as
    /// their changes, and withdraws them once its last clone is dropped, as its caches do theirs
    /// ([`CacheBuilder::with_gauges`]), so the series of resolvers used in parallel under one name, such as one per
    /// task of a component, show what all of them hold. The tags resolver the resolver creates sums too; one set with
    /// [`with_tags_resolver`][Self::with_tags_resolver] keeps its own setting.
    ///
    /// Defaults to [`GaugeMode::Set`].
    pub fn with_gauges(mut self, gauges: GaugeMode) -> Self {
        self.gauges = gauges;
        self
    }

    /// Makes the context cache of the resolver support front caches: clones of the resolver can then be given one with
    /// [`ContextResolver::with_front_cache`].
    ///
    /// A front cache needs to know whether the shared cache still holds what it copied, so the context cache then keeps
    /// a change stamp per bucket of keys, which every insertion, removal, eviction, and expiry advances: it allocates
    /// [`ContextResolver::change_stamps_bytes`] when it is built, and pays one atomic addition per change, none per
    /// lookup. A resolver without caching has no cache to support, and ignores this.
    ///
    /// Defaults to no front caches.
    pub fn with_front_caches(mut self) -> Self {
        self.front_caches = true;
        self
    }

    /// Sets the interner to use for this resolver.
    ///
    /// If an interner isn't provided, an interner will be created in [`ContextResolverBuilder::build`]
    pub fn with_interner(mut self, interner: GenericMapInterner) -> Self {
        self.interner = Some(interner);
        self
    }

    /// Configures a [`ContextResolverBuilder`] that's suitable for tests.
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

        let interner = match self.interner {
            Some(interner) => interner,
            None => GenericMapInterner::new(interner_capacity_bytes),
        };

        let cached_context_limit = self
            .cached_contexts_limit
            .unwrap_or(DEFAULT_CONTEXT_RESOLVER_CACHED_CONTEXTS_LIMIT);

        let allow_heap_allocations = self.allow_heap_allocations.unwrap_or(true);

        let telemetry = Telemetry::new(&self.name);
        let summed = self.telemetry_enabled && self.gauges == GaugeMode::Summed;
        let interner_capacity = interner_capacity(&telemetry, &interner, summed);

        // NOTE: We should switch to using a size-based weighter so that we can do more firm bounding of what we cache.
        let context_cache = CacheBuilder::from_identifier(format!("{}/contexts", self.name))
            .expect("cache identifier cannot possibly be empty")
            .with_capacity(cached_context_limit)
            .with_time_to_idle(self.idle_context_expiration)
            .with_hasher::<NoopU64BuildHasher>()
            .with_telemetry(self.telemetry_enabled)
            .with_gauges(self.gauges);
        // Change stamps let a clone's front cache tell whether the shared cache still holds what it copied.
        let context_cache = if self.front_caches && self.caching_enabled {
            context_cache.with_change_stamps()
        } else {
            context_cache
        };
        let context_cache = if self.accumulated_telemetry {
            context_cache.with_accumulated_telemetry().build()
        } else {
            context_cache.build()
        };

        // If no tags resolver is provided, we need to create one using the same interner used for the context resolver.
        let tags_resolver = match self.tags_resolver {
            Some(tags_resolver) => tags_resolver,
            None => {
                let builder = TagsResolverBuilder::new(format!("{}/tags", self.name), interner.clone())
                    .expect("tags resolver name not empty")
                    .with_cached_tagsets_limit(cached_context_limit.get())
                    .with_idle_tagsets_expiration(self.idle_context_expiration.unwrap_or_default())
                    .with_heap_allocations(allow_heap_allocations)
                    .with_origin_tags_resolver(self.origin_tags_resolver.clone())
                    .with_gauges(self.gauges);
                if self.accumulated_telemetry {
                    builder.with_accumulated_telemetry().build()
                } else {
                    builder.build()
                }
            }
        };

        // The telemetry task stops once the last clone of the resolver is dropped.
        let task = self.telemetry_enabled.then(|| {
            let shutdown = CancellationToken::new();
            tokio::spawn(drive_telemetry(
                interner.clone(),
                telemetry.clone(),
                summed,
                shutdown.clone(),
            ));
            shutdown.drop_guard()
        });

        let (counters, accumulated) = ResolverCounters::new(&telemetry, self.accumulated_telemetry);
        ContextResolver {
            telemetry,
            counters,
            accumulated,
            background: Arc::new(Background {
                _interner_capacity: interner_capacity,
                _task: task,
            }),
            interner,
            caching_enabled: self.caching_enabled,
            context_cache,
            hash_seen_buffer: PrehashedHashSet::with_capacity_and_hasher(
                SEEN_HASHSET_INITIAL_CAPACITY,
                NoopU64BuildHasher,
            ),
            allow_heap_allocations,
            tags_resolver,
            front: None,
        }
    }
}

/// The counters a resolver writes as it resolves: its telemetry's handles, or the counts of its own that an
/// accumulating resolver keeps.
#[derive(Clone)]
struct ResolverCounters {
    intern_fallback: Counter,
    resolved_existing_context: Counter,
    resolved_new_context: Counter,
    resolved_existing_tagset: Counter,
    resolved_new_tagset: Counter,
}

impl ResolverCounters {
    /// Returns the counters of a resolver whose telemetry is `telemetry`, and the counts they accumulate in, if
    /// `accumulated`.
    fn new(telemetry: &Telemetry, accumulated: bool) -> (Self, Option<Arc<AccumulatedCounters>>) {
        let handles = vec![
            telemetry.intern_fallback_total().clone(),
            telemetry.resolved_existing_context_total().clone(),
            telemetry.resolved_new_context_total().clone(),
            telemetry.resolved_existing_tagset_total().clone(),
            telemetry.resolved_new_tagset_total().clone(),
        ];
        let accumulated = accumulated.then(|| Arc::new(AccumulatedCounters::new(handles.clone())));
        let counter = |index: usize| match &accumulated {
            Some(accumulated) => accumulated.local(index),
            None => handles[index].clone(),
        };
        let counters = Self {
            intern_fallback: counter(0),
            resolved_existing_context: counter(1),
            resolved_new_context: counter(2),
            resolved_existing_tagset: counter(3),
            resolved_new_tagset: counter(4),
        };
        (counters, accumulated)
    }
}

/// A centralized store for resolved contexts.
///
/// Contexts are the combination of a name and a set of tags. They're used to identify a specific metric series. As
/// contexts are constructed entirely of strings, they're expensive to construct in a way that allows sending between
/// tasks, as this usually requires allocations. Additionally, some context are "hotter" than others, used frequently by
/// the applications/services sending us metrics.
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
    counters: ResolverCounters,
    /// What an accumulating resolver counted and has not published, shared by its clones.
    accumulated: Option<Arc<AccumulatedCounters>>,
    /// The telemetry task of the resolver and its share of summed gauges, shared by its clones and let go of with the
    /// last.
    background: Arc<Background>,
    interner: GenericMapInterner,
    caching_enabled: bool,
    context_cache: ContextCache,
    hash_seen_buffer: PrehashedHashSet<u64>,
    allow_heap_allocations: bool,
    tags_resolver: TagsResolver,
    /// This clone's own front cache, if it was given one.
    front: Option<FrontCache>,
}

impl ContextResolver {
    /// Gives this resolver a front cache of `entries` contexts, rounded up to a power of two, of its own.
    ///
    /// Clones of a resolver share one context cache, which is safe to use from many tasks at once: every lookup that
    /// hits takes a shard lock, records the access for expiration, and counts the hit, in memory every clone writes, so
    /// clones used in parallel contend on every resolution. A front cache is a direct-mapped cache owned by this clone,
    /// in front of the shared one: a resolution it answers compares a key and a change stamp the shared cache keeps for
    /// it, clones the context, and writes nothing else that is shared. It answers only with what the shared cache holds
    /// for the key, and sends the first resolution of each context in every coarse second to the shared cache, so
    /// contexts expire, and interned strings are freed, as they would without it.
    ///
    /// Its hits reach the hit counters of the resolver and of the context cache when the front cache is maintained (see
    /// [`maintain_front_cache`][Self::maintain_front_cache]), at the latest at its first use in each coarse second, and
    /// when the resolver is dropped. Its slots hold contexts the shared cache holds; a slot whose context the shared
    /// cache let go of is dropped at its next use or maintenance.
    ///
    /// Each context has one slot, chosen by its key, so a front cache answers well for working sets up to about its
    /// size; while the contexts resolved are far more than it can hold (fewer than one resolution in 16 finds its
    /// context in a slot), it steps aside for a while and every resolution takes the shared path, as without it.
    ///
    /// A clone of a resolver with a front cache gets an empty one of the same size. A front cache needs a resolver
    /// built with [`ContextResolverBuilder::with_front_caches`]: a resolver built without it, or without caching, has
    /// nothing to put a front cache in front of, and ignores this. A front cache of 4,096 entries takes 96 KiB on
    /// 64-bit targets (see [`front_cache_bytes`][Self::front_cache_bytes]).
    pub fn with_front_cache(mut self, entries: NonZeroUsize) -> Self {
        if self.caching_enabled {
            self.front = FrontCache::new(
                entries,
                self.context_cache.clone(),
                self.counters.resolved_existing_context.clone(),
            );
        }
        self
    }

    /// Returns the bytes a front cache of `entries` entries allocates.
    pub fn front_cache_bytes(entries: NonZeroUsize) -> usize {
        FrontCache::slot_bytes(entries)
    }

    /// Returns the bytes the context cache of a resolver built with
    /// [`ContextResolverBuilder::with_front_caches`] allocates for its change stamps, once, whatever its clones.
    pub const fn change_stamps_bytes() -> usize {
        ChangeStamps::<NoopU64BuildHasher>::BYTES
    }

    /// Adds the hits the front cache counted since it was last maintained to the counters of the resolver and of the
    /// context cache hit counters, and drops its copies of contexts the shared cache may have let go of; without a
    /// front cache, does nothing.
    ///
    /// Call it periodically, such as from a task's idle timer, so that the counters lag by at most that period and an
    /// idle task holds no context the shared cache expired.
    pub fn maintain_front_cache(&mut self) {
        if let Some(front) = &mut self.front {
            front.maintain();
        }
    }

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
                    // Heap spill: with `allow_context_heap_allocations` true (the default), a full interner silently
                    // falls back to the heap, so the bounded-memory guarantee no longer holds. Anchor that this path is
                    // reached so the unbounded-growth behavior is observable.
                    saluki_antithesis::sometimes!(
                        true,
                        "context string interner spilled to the heap (unbounded under default config)"
                    );
                    self.counters.intern_fallback.increment(1);
                    MetaString::from(s.as_ref())
                })
            })
    }

    fn create_context_key_with_host<N, H, I, I2, T, T2>(
        &mut self, name: N, host: Option<&H>, tags: I, origin_tags: I2,
    ) -> (ContextKey, TagSetKey)
    where
        N: AsRef<str>,
        H: AsRef<str>,
        I: IntoIterator<Item = T>,
        T: AsRef<str>,
        I2: IntoIterator<Item = T2>,
        T2: AsRef<str>,
    {
        hash_context_with_host_and_seen(
            name.as_ref(),
            host.map(AsRef::as_ref),
            tags,
            origin_tags,
            &mut self.hash_seen_buffer,
        )
    }

    fn create_context<N, H>(
        &self, key: ContextKey, name: N, host: Option<H>, context_tags: SharedTagSet, origin_tags: SharedTagSet,
    ) -> Option<Context>
    where
        N: AsRef<str> + CheapMetaString,
        H: AsRef<str> + CheapMetaString,
    {
        // Intern the name, host, and tags of the context.
        let context_name = self.intern(name)?;
        let context_host = match host {
            Some(host) => Some(self.intern(host)?),
            None => None,
        };

        self.counters.resolved_new_context.increment(1);
        self.telemetry.active_contexts().increment(1);

        Some(Context::from_inner(ContextInner::from_parts(
            key,
            context_name,
            context_host,
            context_tags.into(),
            origin_tags.into(),
            self.telemetry.active_contexts().clone(),
        )))
    }

    /// Resolves the given context.
    ///
    /// If the context hasn't yet been resolved, the name and tags are interned and a new context is created and
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
        let origin_tags = self.tags_resolver.resolve_origin_tags(maybe_origin);

        self.resolve_inner(name, tags, origin_tags)
    }

    /// Resolves the given context using the provided origin tags.
    ///
    /// If the context hasn't yet been resolved, the name and tags are interned and a new context is created and
    /// stored. Otherwise, the existing context is returned. The provided origin tags are used to enrich the context.
    ///
    /// `None` may be returned if the interner is full and outside allocations are disallowed. See
    /// `allow_heap_allocations` for more information.
    ///
    /// ## Origin tags resolver mismatch
    ///
    /// When passing in origin tags, they will be inherently tied to a specific `OriginTagsResolver`, which may
    /// differ from the configured origin tags resolver in this context resolver. This means that the context that's
    /// generated and cached may not be reused in the future if an attempt is made to resolve it using the raw origin
    /// information instead.
    ///
    /// This method is intended primarily to allow for resolving contexts in a consistent way while _reusing_ the origin
    /// tags from another context, such as when remapping the name and/or instrumented tags of a given metric, while
    /// maintaining its origin association.
    pub fn resolve_with_origin_tags<N, I, T>(
        &mut self, name: N, tags: I, origin_tags: impl Into<SharedTagSet>,
    ) -> Option<Context>
    where
        N: AsRef<str> + CheapMetaString,
        I: IntoIterator<Item = T> + Clone,
        T: AsRef<str> + CheapMetaString,
    {
        self.resolve_inner(name, tags, origin_tags.into())
    }

    /// Resolves the given context using the provided host and origin tags.
    pub fn resolve_with_host_and_origin_tags<N, H, I, T>(
        &mut self, name: N, host: H, tags: I, origin_tags: impl Into<SharedTagSet>,
    ) -> Option<Context>
    where
        N: AsRef<str> + CheapMetaString,
        H: AsRef<str> + CheapMetaString,
        I: IntoIterator<Item = T> + Clone,
        T: AsRef<str> + CheapMetaString,
    {
        self.resolve_inner_with_host(name, Some(host), tags, origin_tags.into())
    }

    /// Resolves the given context using the provided optional host and origin tags.
    pub fn resolve_with_optional_host_and_origin_tags<N, H, I, T>(
        &mut self, name: N, host: Option<H>, tags: I, origin_tags: impl Into<SharedTagSet>,
    ) -> Option<Context>
    where
        N: AsRef<str> + CheapMetaString,
        H: AsRef<str> + CheapMetaString,
        I: IntoIterator<Item = T> + Clone,
        T: AsRef<str> + CheapMetaString,
    {
        self.resolve_inner_with_host(name, host, tags, origin_tags.into())
    }

    /// Resolves the given context with an explicit host dimension.
    ///
    /// The host participates in context identity but is not part of the visible tag set.
    pub fn resolve_with_host<N, H, I, T>(
        &mut self, name: N, host: H, tags: I, maybe_origin: Option<RawOrigin<'_>>,
    ) -> Option<Context>
    where
        N: AsRef<str> + CheapMetaString,
        H: AsRef<str> + CheapMetaString,
        I: IntoIterator<Item = T> + Clone,
        T: AsRef<str> + CheapMetaString,
    {
        self.resolve_with_optional_host(name, Some(host), tags, maybe_origin)
    }

    /// Resolves the given context with an optional host dimension.
    pub fn resolve_with_optional_host<N, H, I, T>(
        &mut self, name: N, host: Option<H>, tags: I, maybe_origin: Option<RawOrigin<'_>>,
    ) -> Option<Context>
    where
        N: AsRef<str> + CheapMetaString,
        H: AsRef<str> + CheapMetaString,
        I: IntoIterator<Item = T> + Clone,
        T: AsRef<str> + CheapMetaString,
    {
        let origin_tags = self.tags_resolver.resolve_origin_tags(maybe_origin);

        self.resolve_inner_with_host(name, host, tags, origin_tags)
    }

    fn resolve_inner<N, I, T>(&mut self, name: N, tags: I, origin_tags: SharedTagSet) -> Option<Context>
    where
        N: AsRef<str> + CheapMetaString,
        I: IntoIterator<Item = T> + Clone,
        T: AsRef<str> + CheapMetaString,
    {
        self.resolve_inner_with_host(name, None::<&str>, tags, origin_tags)
    }

    fn resolve_inner_with_host<N, H, I, T>(
        &mut self, name: N, host: Option<H>, tags: I, origin_tags: SharedTagSet,
    ) -> Option<Context>
    where
        N: AsRef<str> + CheapMetaString,
        H: AsRef<str> + CheapMetaString,
        I: IntoIterator<Item = T> + Clone,
        T: AsRef<str> + CheapMetaString,
    {
        let (context_key, tagset_key) =
            self.create_context_key_with_host(&name, host.as_ref(), tags.clone(), &origin_tags);

        // Fast path to avoid looking up the context in the cache if caching is disabled.
        if !self.caching_enabled {
            let tag_set = self.tags_resolver.create_tag_set(tags).unwrap_or_default();

            let context = self.create_context(context_key, name, host, tag_set, origin_tags)?;

            debug!(?context_key, ?context, "Resolved new non-cached context.");
            return Some(context);
        }

        // A front cache answers for what the shared cache holds; whatever it does not answer, the shared cache does.
        // The key's change stamp is read before the shared lookup, so a copy of what it returns is never newer than
        // its stamp.
        let mut front_fill = None;
        if let Some(front) = &mut self.front {
            let now = get_coarse_unix_timestamp() as u32;
            match front.lookup(context_key, now) {
                Lookup::Hit(context) => return Some(context),
                Lookup::Miss(stamp) => front_fill = stamp.map(|stamp| (stamp, now)),
            }
        }

        match self.context_cache.get(&context_key) {
            Some(context) => {
                self.counters.resolved_existing_context.increment(1);
                if let (Some(front), Some((stamp, now))) = (&mut self.front, front_fill) {
                    front.fill(context_key, &context, stamp, now);
                }
                Some(context)
            }
            None => {
                // Try seeing if we have the tagset cached already, and create it if not.
                let tag_set = match self.tags_resolver.get_tag_set(tagset_key) {
                    Some(tag_set) => {
                        self.counters.resolved_existing_tagset.increment(1);
                        tag_set
                    }
                    None => {
                        // If the tagset is not cached, we need to create it.
                        let tag_set = self.tags_resolver.create_tag_set(tags.clone()).unwrap_or_default();

                        self.tags_resolver.insert_tag_set(tagset_key, tag_set.clone());

                        tag_set
                    }
                };

                let context = self.create_context(context_key, name, host, tag_set, origin_tags)?;
                self.context_cache.insert(context_key, context.clone());

                debug!(?context_key, ?context, "Resolved new context.");
                Some(context)
            }
        }
    }

    /// Adds what the resolver, its context cache, and its tags resolver counted since they last published to their
    /// telemetry, if they were built with
    /// [accumulated telemetry][ContextResolverBuilder::with_accumulated_telemetry]; otherwise, does nothing.
    ///
    /// Clones of a resolver share its counts, so publishing from any of them publishes everything counted once.
    pub fn publish_telemetry(&self) {
        if let Some(accumulated) = &self.accumulated {
            accumulated.publish();
        }
        self.context_cache.publish_telemetry();
        self.tags_resolver.publish_telemetry();
    }
}

impl Drop for ContextResolver {
    fn drop(&mut self) {
        // The front cache publishes the hits it has not published yet when it drops, into the resolver's counters,
        // and an accumulating resolver's counts publish what they hold when the last clone drops them: the front cache
        // goes first, so that its last hits reach the telemetry too.
        drop(self.front.take());
    }
}

impl Clone for ContextResolver {
    fn clone(&self) -> Self {
        Self {
            telemetry: self.telemetry.clone(),
            counters: self.counters.clone(),
            accumulated: self.accumulated.clone(),
            background: Arc::clone(&self.background),
            interner: self.interner.clone(),
            caching_enabled: self.caching_enabled,
            context_cache: self.context_cache.clone(),
            hash_seen_buffer: PrehashedHashSet::with_capacity_and_hasher(
                SEEN_HASHSET_INITIAL_CAPACITY,
                NoopU64BuildHasher,
            ),
            allow_heap_allocations: self.allow_heap_allocations,
            tags_resolver: self.tags_resolver.clone(),
            front: self.front.as_ref().map(FrontCache::empty_like),
        }
    }
}

/// What a context resolver runs or holds for its telemetry, for as long as any clone of it lives: its telemetry task,
/// which stops when this is dropped, and its share of its interner's summed capacity.
struct Background {
    _interner_capacity: Option<GaugeContribution>,
    _task: Option<DropGuard>,
}

/// Reports the capacity of `interner` to its gauge in `telemetry`: adds it, returning the share to withdraw once the
/// resolver is gone, when `summed`, and sets it otherwise.
fn interner_capacity(telemetry: &Telemetry, interner: &GenericMapInterner, summed: bool) -> Option<GaugeContribution> {
    let capacity = interner.capacity_bytes() as f64;
    if summed {
        Some(GaugeContribution::add(
            telemetry.interner_capacity_bytes().clone(),
            capacity,
        ))
    } else {
        telemetry.interner_capacity_bytes().set(capacity);
        None
    }
}

/// Reports the interner's entries and length once a second until `shutdown`, and, unless `summed`, its capacity: sets
/// them, or, when `summed`, adds their changes and withdraws what it added when it stops.
async fn drive_telemetry(
    interner: GenericMapInterner, telemetry: Telemetry, summed: bool, shutdown: CancellationToken,
) {
    let mut entries = SummedGauge::new(telemetry.interner_entries().clone(), summed);
    let mut len_bytes = SummedGauge::new(telemetry.interner_len_bytes().clone(), summed);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = sleep(Duration::from_secs(1)) => {}
        }

        entries.report(interner.len() as f64);
        if !summed {
            telemetry
                .interner_capacity_bytes()
                .set(interner.capacity_bytes() as f64);
        }
        len_bytes.report(interner.len_bytes() as f64);
    }
}

/// A builder for a tag resolver.
pub struct TagsResolverBuilder {
    name: String,
    caching_enabled: bool,
    cached_tagset_limit: Option<NonZeroUsize>,
    idle_tagset_expiration: Option<Duration>,
    allow_heap_allocations: Option<bool>,
    origin_tags_resolver: Option<Arc<dyn OriginTagsResolver>>,
    telemetry_enabled: bool,
    interner: GenericMapInterner,
    accumulated_telemetry: bool,
    gauges: GaugeMode,
}

impl TagsResolverBuilder {
    /// Creates a new [`TagsResolverBuilder`] with the given name and interner.
    pub fn new<S: Into<String>>(name: S, interner: GenericMapInterner) -> Result<Self, GenericError> {
        let name = name.into();
        if name.is_empty() {
            return Err(generic_error!("resolver name must not be empty"));
        }

        Ok(Self {
            name,
            caching_enabled: true,
            cached_tagset_limit: None,
            idle_tagset_expiration: None,
            allow_heap_allocations: None,
            origin_tags_resolver: None,
            telemetry_enabled: true,
            interner,
            accumulated_telemetry: false,
            gauges: GaugeMode::Set,
        })
    }

    /// Sets the interner to use for this resolver.
    ///
    /// This is used when we want to use a separate internet for tagsets, different from the one used for contexts.
    ///
    /// Defaults to using the interner passed to the builder.
    pub fn with_interner(mut self, interner: GenericMapInterner) -> Self {
        self.interner = interner;
        self
    }

    /// Sets whether or not to enable caching of resolved tag sets.
    ///
    /// [`TagsResolver`] provides two main benefits: consistent behavior for resolving tag sets (interning, origin
    /// tags, etc), and the caching of those resolved tag sets to speed up future resolutions. However, caching tag
    /// sets means that we pay a memory cost for the cache itself, even if the tag sets aren't ever reused or are seen
    /// infrequently. While expiration can help free up cache capacity, it can't help recover the memory used by the
    /// underlying cache data structure once they have expanded to hold the tag sets.
    ///
    /// Disabling caching allows normal resolving to take place without the overhead of caching the tag sets. This can
    /// lead to lower average memory usage, as tag sets will only live as long as they're needed, but it will reduce
    /// memory determinism as memory will be allocated for every resolved tag set (minus interned strings), which means
    /// that resolving the same tag set ten times in a row will result in ten separate allocations, and so on.
    ///
    /// Defaults to caching enabled.
    pub fn without_caching(mut self) -> Self {
        self.caching_enabled = false;
        self.idle_tagset_expiration = None;
        self
    }

    /// Sets the limit on the number of cached tagsets.
    ///
    /// This is the maximum number of resolved tag sets that can be cached at any given time. This limit doesn't affect
    /// the total number of tag sets that can be _alive_ at any given time, which is dependent on the interner capacity
    /// and whether or not heap allocations are allowed.
    ///
    /// Caching tag sets is beneficial when the same tag set is resolved frequently, and it's generally worth
    /// allowing for higher limits on cached tag sets when heap allocations are allowed, as this can better amortize the
    /// cost of those heap allocations.
    ///
    /// If value is zero, caching will be disabled, and no tag sets will be cached. This is equivalent to calling
    /// `without_caching`.
    ///
    /// Defaults to 500,000.
    pub fn with_cached_tagsets_limit(mut self, limit: usize) -> Self {
        match NonZeroUsize::new(limit) {
            Some(limit) => {
                self.cached_tagset_limit = Some(limit);
                self
            }
            None => self.without_caching(),
        }
    }

    /// Sets the time before tag sets are considered "idle" and eligible for expiration.
    ///
    /// This controls how long a tag set will be kept in the cache after its last access or creation time. This value is
    /// a lower bound, as tag sets eligible for expiration may not be expired immediately. Tag sets may still be removed
    /// prior to their natural expiration time if the cache is full and evictions are required to make room for a new
    /// context.
    ///
    /// Defaults to no expiration.
    pub fn with_idle_tagsets_expiration(mut self, time_to_idle: Duration) -> Self {
        self.idle_tagset_expiration = Some(time_to_idle);
        self
    }

    /// Sets whether or not to allow heap allocations when interning strings.
    ///
    /// In cases where the interner is full, this setting determines whether or not we refuse to resolve a context, or
    /// if we allow it be resolved by allocating strings on the heap. When heap allocations are enabled, the amount of
    /// memory that can be used by the interner is effectively unlimited, as contexts that can't be interned will be
    /// simply spill to the heap instead of being limited in any way.
    ///
    /// Defaults to `true`.
    pub fn with_heap_allocations(mut self, allow: bool) -> Self {
        self.allow_heap_allocations = Some(allow);
        self
    }

    /// Sets the origin tags resolver to use when building a context.
    ///
    /// In some cases, metrics, events, and service checks may have enriched tags based on their origin -- the
    /// application/host/container/etc that emitted the metric -- which has to be considered when building the context
    /// itself. As this can be expensive, it's useful to split the logic of actually grabbing the enriched tags based
    /// on the available origin info into a separate phase, and implementation, that can run separately from the
    /// initial hash-based approach of checking if a context has already been resolved.
    ///
    /// When set, any origin information provided will be considered during hashing when looking up a context, and any
    /// enriched tags attached to the detected origin will be accessible from the context.
    ///
    /// Defaults to unset.
    pub fn with_origin_tags_resolver(mut self, resolver: Option<Arc<dyn OriginTagsResolver>>) -> Self {
        self.origin_tags_resolver = resolver;
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

    /// Makes the resolver, and its cache, count what they resolve and look up in counters of their own, which
    /// [`TagsResolver::publish_telemetry`] adds to their telemetry; see
    /// [`ContextResolverBuilder::with_accumulated_telemetry`].
    ///
    /// Defaults to writing the telemetry on every resolution.
    pub fn with_accumulated_telemetry(mut self) -> Self {
        self.accumulated_telemetry = true;
        self
    }

    /// Sets how the resolver reports its interner's capacity and its cache its items, weight, and limit: set to its
    /// own values, or summed with the other resolvers of its name, its share withdrawn once its last clone is dropped;
    /// see [`ContextResolverBuilder::with_gauges`].
    ///
    /// Defaults to [`GaugeMode::Set`].
    pub fn with_gauges(mut self, gauges: GaugeMode) -> Self {
        self.gauges = gauges;
        self
    }

    /// Builds a [`TagsResolver`] from the current configuration.
    pub fn build(self) -> TagsResolver {
        let cached_tagsets_limit = self
            .cached_tagset_limit
            .unwrap_or(DEFAULT_CONTEXT_RESOLVER_CACHED_CONTEXTS_LIMIT);

        let allow_heap_allocations = self.allow_heap_allocations.unwrap_or(true);

        let telemetry = Telemetry::new(self.name.clone());
        let summed = self.telemetry_enabled && self.gauges == GaugeMode::Summed;
        let interner_capacity = interner_capacity(&telemetry, &self.interner, summed).map(Arc::new);

        let tagset_cache = CacheBuilder::from_identifier(format!("{}/tagsets", self.name))
            .expect("cache identifier cannot possibly be empty")
            .with_capacity(cached_tagsets_limit)
            .with_time_to_idle(self.idle_tagset_expiration)
            .with_hasher::<NoopU64BuildHasher>()
            .with_telemetry(self.telemetry_enabled)
            .with_gauges(self.gauges);
        let tagset_cache = if self.accumulated_telemetry {
            tagset_cache.with_accumulated_telemetry().build()
        } else {
            tagset_cache.build()
        };

        let (counters, accumulated) = ResolverCounters::new(&telemetry, self.accumulated_telemetry);
        TagsResolver {
            telemetry,
            counters,
            accumulated,
            _interner_capacity: interner_capacity,
            interner: self.interner,
            caching_enabled: self.caching_enabled,
            tagset_cache,
            origin_tags_resolver: self.origin_tags_resolver,
            allow_heap_allocations,
        }
    }

    /// Configures a [`TagsResolverBuilder`] that's suitable for tests.
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
        TagsResolverBuilder::new("noop", GenericMapInterner::new(NonZeroUsize::new(1).expect("not zero")))
            .expect("resolver name not empty")
            .with_cached_tagsets_limit(usize::MAX)
            .with_heap_allocations(true)
            .without_telemetry()
    }
}

/// A resolver for tags.
pub struct TagsResolver {
    telemetry: Telemetry,
    counters: ResolverCounters,
    /// What an accumulating resolver counted and has not published, shared by its clones.
    accumulated: Option<Arc<AccumulatedCounters>>,
    /// The share of the interner's capacity that a summed resolver reports, shared by its clones and withdrawn with the
    /// last.
    _interner_capacity: Option<Arc<GaugeContribution>>,
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
                    // Heap spill: with `allow_context_heap_allocations` true (the default), a full interner silently
                    // falls back to the heap, so the bounded-memory guarantee no longer holds. Anchor that this path is
                    // reached so the unbounded-growth behavior is observable.
                    saluki_antithesis::sometimes!(
                        true,
                        "tag string interner spilled to the heap (unbounded under default config)"
                    );
                    self.counters.intern_fallback.increment(1);
                    MetaString::from(s.as_ref())
                })
            })
    }

    /// Creates a new tag set from the given tags.
    ///
    /// This will intern the tags, and then return a shared tag set. If the interner is full, and heap allocations are
    /// not allowed, then this will return `None`.
    ///
    /// If heap allocations are allowed, then this will return a shared tag set, and the tag set will be cached.
    pub fn create_tag_set<I, T>(&mut self, tags: I) -> Option<SharedTagSet>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<str> + CheapMetaString,
    {
        let mut tag_set = TagSet::default();
        for tag in tags {
            let tag = self.intern(tag)?;
            tag_set.insert_tag(tag);
        }

        self.counters.resolved_new_tagset.increment(1);

        Some(tag_set.into_shared())
    }

    /// Resolves the origin tags for the given origin.
    ///
    /// This will return the origin tags for the given origin, or an empty tag set if no origin tags resolver is set.
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

    /// Adds what the resolver and its cache counted since they last published to their telemetry, if they were built
    /// with [accumulated telemetry][TagsResolverBuilder::with_accumulated_telemetry]; otherwise, does nothing.
    pub fn publish_telemetry(&self) {
        if let Some(accumulated) = &self.accumulated {
            accumulated.publish();
        }
        self.tagset_cache.publish_telemetry();
    }
}

impl Clone for TagsResolver {
    fn clone(&self) -> Self {
        Self {
            telemetry: self.telemetry.clone(),
            counters: self.counters.clone(),
            accumulated: self.accumulated.clone(),
            _interner_capacity: self._interner_capacity.clone(),
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
    use crate::data_model::tags::Tag;

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
    fn host_affects_identity_but_not_visible_tags() {
        let mut resolver = ContextResolverBuilder::for_tests().build();

        let context1 = resolver
            .resolve_with_host("metric_name", "host-a", &[] as &[&str], None)
            .expect("should not fail to resolve");
        let context2 = resolver
            .resolve_with_host("metric_name", "host-b", &[] as &[&str], None)
            .expect("should not fail to resolve");
        let context1_redo = resolver
            .resolve_with_host("metric_name", "host-a", &[] as &[&str], None)
            .expect("should not fail to resolve");

        assert_ne!(context1, context2);
        assert_eq!(context1, context1_redo);
        assert!(context1.ptr_eq(&context1_redo));
        assert_eq!(context1.host(), Some("host-a"));
        assert_eq!(context2.host(), Some("host-b"));
        assert!(context1.tags().is_empty());
        assert!(context2.tags().is_empty());

        let mut uncached_resolver = ContextResolverBuilder::for_tests().without_caching().build();
        let uncached1 = uncached_resolver
            .resolve_with_host("metric_name", "host-a", &[] as &[&str], None)
            .expect("should not fail to resolve");
        let uncached2 = uncached_resolver
            .resolve_with_host("metric_name", "host-b", &[] as &[&str], None)
            .expect("should not fail to resolve");

        assert_ne!(uncached1, uncached2);
        assert!(!uncached1.ptr_eq(&uncached2));
    }

    #[test]
    fn host_survives_rewrites() {
        let mut resolver = ContextResolverBuilder::for_tests().build();

        let context1 = resolver
            .resolve_with_host("metric_name", "host-a", &["env:prod"][..], None)
            .expect("should not fail to resolve");
        let context2 = resolver
            .resolve_with_host("metric_name", "host-b", &["env:prod"][..], None)
            .expect("should not fail to resolve");

        assert_ne!(context1, context2);
        let service_tag_set = TagSet::from_iter([Tag::from("service:api")]);
        assert_ne!(context1.with_name("renamed"), context2.with_name("renamed"));
        assert_ne!(
            context1.with_tags(service_tag_set.clone()),
            context2.with_tags(service_tag_set)
        );

        let mut context1_filtered = context1.clone();
        let mut context2_filtered = context2.clone();
        let mut state1 = crate::data_model::event::metric::context::TagSetMutViewState::new();
        let mut state2 = crate::data_model::event::metric::context::TagSetMutViewState::new();
        {
            let mut view = context1_filtered.tags_mut_view(&mut state1);
            view.retain_tags(|_| false);
            view.finish();
        }
        {
            let mut view = context2_filtered.tags_mut_view(&mut state2);
            view.retain_tags(|_| false);
            view.finish();
        }

        assert_ne!(context1_filtered, context2_filtered);
        assert!(context1_filtered.tags().is_empty());
        assert!(context2_filtered.tags().is_empty());
        assert_eq!(context1_filtered.host(), Some("host-a"));
        assert_eq!(context2_filtered.host(), Some("host-b"));
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

        // Now drop the context, and observe the active context count is negative one, representing the context we dropped:
        drop(context);
        let metrics_after = snapshotter.snapshot().into_vec();
        let active_contexts = get_gauge_value(&metrics_after, Telemetry::active_contexts_name());
        assert_eq!(active_contexts, -1.0);
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
        origin1.set_local_data("container1");
        let mut origin2 = RawOrigin::default();
        origin2.set_local_data("container2");

        let context1 = resolver
            .resolve(name, &tags[..], Some(origin1.clone()))
            .expect("should not fail to resolve");
        let context2 = resolver
            .resolve(name, &tags[..], Some(origin2.clone()))
            .expect("should not fail to resolve");

        assert_eq!(context1, context2);

        let tags_resolver = TagsResolverBuilder::for_tests()
            .with_origin_tags_resolver(Some(Arc::new(DummyOriginTagsResolver)))
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
            .with_origin_tags_resolver(Some(Arc::new(DummyOriginTagsResolver)))
            .build();
        let mut resolver = ContextResolverBuilder::for_tests()
            .without_caching()
            .with_tags_resolver(Some(tags_resolver))
            .build();

        let name = "metric_name";
        let tags = ["tag1"];
        let mut origin1 = RawOrigin::default();
        origin1.set_local_data("container1");

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

    /// Returns every counter of `recorder` whose label value starts with `prefix`, with that prefix removed from it.
    fn counters_of(recorder: &saluki_metrics::test::TestRecorder, prefix: &str) -> Vec<(String, String, u64)> {
        let mut counters = recorder
            .counters()
            .into_iter()
            .filter_map(|(key, value)| {
                let label = key.labels().find(|label| label.value().starts_with(prefix))?;
                let rest = label.value()[prefix.len()..].to_string();
                Some((key.name().to_string(), rest, value))
            })
            .collect::<Vec<_>>();
        counters.sort();
        counters
    }

    /// Returns a builder of a caching resolver named `name` whose cache holds `limit` contexts and supports front
    /// caches, without background tasks.
    fn caching_resolver_builder(name: &str, limit: usize) -> ContextResolverBuilder {
        ContextResolverBuilder::from_name(name)
            .unwrap()
            .with_cached_contexts_limit(limit)
            .with_heap_allocations(true)
            .with_tags_resolver(Some(TagsResolverBuilder::for_tests().build()))
            .with_front_caches()
            .without_telemetry()
    }

    /// Builds a caching resolver named `name` whose cache holds `limit` contexts and supports front caches, without
    /// background tasks.
    fn caching_resolver(name: &str, limit: usize) -> ContextResolver {
        caching_resolver_builder(name, limit).build()
    }

    /// Returns the gauge `name` of the resolver `resolver_id` in `recorder`.
    fn resolver_gauge(recorder: &saluki_metrics::test::TestRecorder, name: &str, resolver_id: &str) -> Option<f64> {
        recorder.gauge(metrics::Key::from_parts(
            format!("context_resolver_{}", name),
            vec![metrics::Label::new("resolver_id", resolver_id.to_string())],
        ))
    }

    #[tokio::test(start_paused = true)]
    async fn the_interner_gauges_of_summed_resolvers_of_one_name_sum_and_each_withdraws_its_share_with_its_last_clone()
    {
        let recorder = saluki_metrics::test::TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);
        let build = |capacity: usize| {
            ContextResolverBuilder::from_name("summed")
                .unwrap()
                .with_interner_capacity_bytes(NonZeroUsize::new(capacity).unwrap())
                .with_gauges(GaugeMode::Summed)
                .build()
        };
        let (mut first, mut second) = (build(4_096), build(8_192));
        let capacity = |name| resolver_gauge(&recorder, "interner_capacity_bytes", name);
        assert_eq!(capacity("summed"), Some(12_288.0), "the capacities sum at build");
        assert_eq!(capacity("summed/tags"), Some(12_288.0), "and the tags resolvers' too");
        first
            .resolve("a.long.enough.name.to.be.interned", &["env:prod"][..], None)
            .unwrap();
        second
            .resolve("another.long.enough.name.to.be.interned", &["env:prod"][..], None)
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        let entries = |recorder: &saluki_metrics::test::TestRecorder| {
            resolver_gauge(recorder, "interner_entries", "summed").expect("the entries are reported")
        };
        let both = entries(&recorder);
        assert_eq!(
            both,
            (first.interner.len() + second.interner.len()) as f64,
            "the entries sum"
        );
        let first_entries = first.interner.len() as f64;
        assert!(first_entries > 0.0);
        let contexts = |recorder: &saluki_metrics::test::TestRecorder| {
            recorder.gauge(metrics::Key::from_parts(
                "cache_current_items",
                vec![metrics::Label::new("cache_id", "summed/contexts")],
            ))
        };
        assert_eq!(contexts(&recorder), Some(2.0), "the context caches' items sum");

        // A clone keeps the resolver's share, and its telemetry task running; the last one dropped withdraws the
        // share, and stops the task.
        let clone = first.clone();
        drop(first);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(entries(&recorder), both);
        drop(clone);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(entries(&recorder), both - first_entries);
        assert_eq!(capacity("summed"), Some(8_192.0));
        assert_eq!(capacity("summed/tags"), Some(8_192.0));
        assert_eq!(contexts(&recorder), Some(1.0));
        drop(second);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(entries(&recorder), 0.0);
        assert_eq!(capacity("summed"), Some(0.0));
        assert_eq!(capacity("summed/tags"), Some(0.0));
        assert_eq!(contexts(&recorder), Some(0.0));

        // A resolver whose gauges are set reports its own values, and its telemetry task stops with its last clone too,
        // rather than holding the interner it reads for the life of the process.
        let resolver = ContextResolverBuilder::from_name("set").unwrap().build();
        let capacity_bytes = resolver.interner.capacity_bytes() as f64;
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert_eq!(capacity("set"), Some(capacity_bytes));
        drop(resolver);
        tokio::time::sleep(Duration::from_millis(10)).await;
        metrics::gauge!("context_resolver_interner_capacity_bytes", "resolver_id" => "set").set(-1.0);
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(
            capacity("set"),
            Some(-1.0),
            "no task sets the gauge once the resolver is gone"
        );
    }

    #[test]
    fn a_front_cache_answers_with_the_shared_contexts_and_counts_what_the_shared_cache_would() {
        let recorder = saluki_metrics::test::TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);
        let mut plain = caching_resolver("plain", 1_000);
        let mut front = caching_resolver("front", 1_000).with_front_cache(NonZeroUsize::new(64).unwrap());

        for index in [0, 1, 0, 0, 2, 1, 0, 3, 3, 3, 0, 4, 1] {
            let name = format!("metric.{}", index);
            let expected = plain.resolve(&name, ["env:prod"], None).unwrap();
            let context = front.resolve(&name, ["env:prod"], None).unwrap();
            assert_eq!(context, expected);
            let shared = front
                .context_cache
                .peek(&context.key())
                .expect("the shared cache holds it");
            assert!(
                context.ptr_eq(&shared),
                "a front cache answers with the shared cache's context"
            );
        }

        // Every resolution counts once, as a hit or a new context, in the same series once the front cache publishes.
        front.maintain_front_cache();
        let counted = counters_of(&recorder, "front");
        assert_eq!(counted, counters_of(&recorder, "plain"));
        let hits = counted
            .iter()
            .find(|(name, rest, _)| name == "cache_hits_total" && rest == "/contexts")
            .map(|(_, _, value)| *value);
        assert_eq!(hits, Some(8), "13 resolutions of five contexts");

        // A clone starts with an empty front cache of its own; what is left unpublished is published on drop.
        let mut clone = front.clone();
        assert_eq!(clone.front.as_ref().map(FrontCache::occupied), Some(0));
        clone.resolve("metric.0", ["env:prod"], None).unwrap();
        clone.resolve("metric.0", ["env:prod"], None).unwrap();
        plain.resolve("metric.0", ["env:prod"], None).unwrap();
        plain.resolve("metric.0", ["env:prod"], None).unwrap();
        drop(clone);
        assert_eq!(counters_of(&recorder, "front"), counters_of(&recorder, "plain"));
    }

    #[test]
    fn an_accumulating_resolver_with_a_front_cache_counts_what_one_without_counts_once_the_last_clone_drops() {
        let recorder = saluki_metrics::test::TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);
        let mut plain = caching_resolver_builder("plain", 1_000)
            .with_accumulated_telemetry()
            .build();
        let mut front = caching_resolver_builder("front", 1_000)
            .with_accumulated_telemetry()
            .build()
            .with_front_cache(NonZeroUsize::new(64).unwrap());

        // Five resolutions of one context, then a clone's two: one new context, then hits, most answered by a front
        // cache and counted there.
        for _ in 0..5 {
            plain.resolve("metric.one", ["env:prod"], None).unwrap();
            front.resolve("metric.one", ["env:prod"], None).unwrap();
        }
        let mut plain_clone = plain.clone();
        let mut front_clone = front.clone();
        for _ in 0..2 {
            plain_clone.resolve("metric.one", ["env:prod"], None).unwrap();
            front_clone.resolve("metric.one", ["env:prod"], None).unwrap();
        }
        drop((plain_clone, front_clone));

        // Nothing is published until the last clone drops; then everything is, the front caches' hits included.
        drop((plain, front));
        let counted = counters_of(&recorder, "front");
        assert_eq!(counted, counters_of(&recorder, "plain"));
        let existing = counted
            .iter()
            .find(|(name, _, _)| name == "context_resolver_resolved_existing_context_total")
            .map(|(_, _, value)| *value);
        assert_eq!(existing, Some(6), "seven resolutions of one context");
    }

    #[test]
    fn a_resolver_without_caching_or_built_without_front_caches_ignores_a_front_cache() {
        let resolver = ContextResolverBuilder::for_tests()
            .without_caching()
            .with_front_caches()
            .build()
            .with_front_cache(NonZeroUsize::new(64).unwrap());
        assert!(resolver.front.is_none());
        assert!(resolver.context_cache.change_stamps().is_none(), "nothing to support");

        // Without front caches, the context cache keeps no change stamps, and a front cache has nothing to rely on.
        let resolver = ContextResolverBuilder::for_tests()
            .build()
            .with_front_cache(NonZeroUsize::new(64).unwrap());
        assert!(resolver.front.is_none());
        assert!(resolver.context_cache.change_stamps().is_none());
        let resolver = ContextResolverBuilder::for_tests().with_front_caches().build();
        assert!(resolver.context_cache.change_stamps().is_some());
    }

    #[test]
    fn change_stamps_take_16_kib() {
        assert_eq!(ContextResolver::change_stamps_bytes(), 16 * 1024);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn a_front_cache_of_4096_entries_takes_96_kib() {
        assert_eq!(
            ContextResolver::front_cache_bytes(NonZeroUsize::new(4_096).unwrap()),
            96 * 1024
        );
    }

    #[test]
    fn a_context_the_shared_cache_let_go_of_is_resolved_anew_and_dropped_from_the_front_cache() {
        let mut front = caching_resolver("removing", 1_000).with_front_cache(NonZeroUsize::new(4_096).unwrap());
        let first = front.resolve("metric.first", ["a:b"], None).unwrap();
        let again = front.resolve("metric.first", ["a:b"], None).unwrap();
        assert!(again.ptr_eq(&first));
        // Another context, in another slot of the front cache (keys are hashed with a seed of the process's).
        let slot = |key: ContextKey| key.as_u64() & 4_095;
        let no_tags: [&str; 0] = [];
        let kept_name = (0..)
            .map(|index| format!("metric.kept.{}", index))
            .find(|name| slot(super::super::hash::hash_context(name, ["a:b"], no_tags).0) != slot(first.key()))
            .unwrap();
        let kept = front.resolve(&kept_name, ["a:b"], None).unwrap();
        front.resolve(&kept_name, ["a:b"], None).unwrap();
        assert_eq!(front.front.as_ref().map(FrontCache::occupied), Some(2));

        // The shared cache lets the context go (as an expiry or an eviction does, which advance the same stamps). The
        // front cache keeps no copy past its next maintenance, and does not answer with it.
        front.context_cache.remove(&first.key());
        front.maintain_front_cache();
        let copies = front.front.as_ref().map(FrontCache::copies).unwrap();
        assert_eq!(copies.len(), 1);
        assert!(copies[0].1.ptr_eq(&kept), "the copies of other contexts stay");
        let anew = front.resolve("metric.first", ["a:b"], None).unwrap();
        assert!(
            !anew.ptr_eq(&first),
            "a context the shared cache let go of is resolved anew, as without a front cache"
        );
    }

    /// One step of a sequence of resolutions through a resolver with a front cache and a clone of it without one.
    #[derive(Clone, Debug)]
    enum Step {
        /// The resolver with a front cache resolves context `n`.
        Front(u8),
        /// The clone without a front cache resolves context `n`, which can evict others from the shared cache.
        Other(u8),
        /// The resolver with a front cache maintains it.
        Maintain,
    }

    fn arb_step() -> impl proptest::strategy::Strategy<Value = Step> {
        use proptest::prelude::*;
        prop_oneof![
            6 => (0..24u8).prop_map(Step::Front),
            3 => (0..24u8).prop_map(Step::Other),
            1 => Just(Step::Maintain),
        ]
    }

    proptest::proptest! {
        #[test]
        fn property_test_a_front_cache_answers_only_with_what_the_shared_cache_holds(
            limit in 2usize..12,
            entries in 1usize..32,
            steps in proptest::collection::vec(arb_step(), 1..200),
        ) {
            let mut front = caching_resolver("property", limit).with_front_cache(NonZeroUsize::new(entries).unwrap());
            let mut other = front.clone();
            other.front = None;
            for step in steps {
                match step {
                    Step::Front(index) => {
                        let answered_before = front.front.as_ref().map(FrontCache::unpublished_hits);
                        let context = front.resolve(format!("metric.{}", index), ["a:b"], None).unwrap();
                        let answered = front.front.as_ref().map(FrontCache::unpublished_hits) != answered_before;
                        let shared = front.context_cache.peek(&context.key());
                        if answered {
                            proptest::prop_assert!(
                                shared.is_some_and(|shared| shared.ptr_eq(&context)),
                                "a front cache answered with a context the shared cache does not hold"
                            );
                        }
                    }
                    Step::Other(index) => {
                        other.resolve(format!("metric.{}", index), ["a:b"], None).unwrap();
                    }
                    Step::Maintain => {
                        front.maintain_front_cache();
                        // After maintenance, every copy it keeps is what the shared cache holds.
                        let front_cache = front.front.as_ref().unwrap();
                        for (key, context) in front_cache.copies() {
                            let shared = front.context_cache.peek(&key);
                            proptest::prop_assert!(shared.is_some_and(|shared| shared.ptr_eq(&context)));
                        }
                    }
                }
            }
        }
    }
}
