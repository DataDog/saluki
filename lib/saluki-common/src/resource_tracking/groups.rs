use std::{
    cell::RefCell,
    collections::HashMap,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    ptr::NonNull,
    sync::{Mutex, OnceLock},
    task::{Context, Poll},
};

use pin_project::pin_project;

use super::stats::{measured_cpu_time_start, sampled_cpu_time_start, CpuTimeStart, ResourceStats};

static REGISTRY: OnceLock<ResourceGroupRegistry> = OnceLock::new();
static ROOT_GROUP: ResourceStats = ResourceStats::new();

thread_local! {
    pub(super) static CURRENT_GROUP: RefCell<NonNull<ResourceStats>> = RefCell::new(NonNull::from(&ROOT_GROUP));
}

/// A token associated with a specific resource group.
///
/// Used to attribute allocations and deallocations to a specific group with a scope guard [`ResourceTrackingGuard`], or
/// through helpers provided by the [`Track`] trait.
#[derive(Clone, Copy)]
pub struct ResourceGroupToken {
    group_ptr: NonNull<ResourceStats>,
}

impl ResourceGroupToken {
    fn new(group_ptr: NonNull<ResourceStats>) -> Self {
        Self { group_ptr }
    }

    /// Returns an `ResourceGroupToken` for the current resource group.
    pub fn current() -> Self {
        CURRENT_GROUP.with(|current_group| {
            let group_ptr = current_group.borrow();
            Self::new(*group_ptr)
        })
    }

    #[cfg(test)]
    fn ptr_eq(&self, other: &Self) -> bool {
        self.group_ptr == other.group_ptr
    }

    /// Returns the token for the root resource group.
    pub fn root() -> Self {
        Self::new(NonNull::from(&ROOT_GROUP))
    }

    /// Enters this resource group, returning a guard that will exit the resource group when dropped.
    ///
    /// Allocations are attributed to the group while it is entered. The CPU time the thread uses until the guard is
    /// dropped is attributed to it only if the entry is sampled: with the default sampling rate of `1`, every entry
    /// is; see [`set_cpu_time_sampling`][super::set_cpu_time_sampling].
    pub fn enter(&self) -> ResourceTrackingGuard<'_> {
        self.enter_with(sampled_cpu_time_start())
    }

    /// Enters this resource group, as [`enter`][Self::enter] does, and always measures the CPU time the thread uses
    /// until the guard is dropped, whatever the sampling rate.
    ///
    /// Use this for entries known to be long and rare, such as building a component or draining accumulated state,
    /// whose cost sampling would report as nothing, or as many times what they cost, in any interval short enough to
    /// see few of them.
    pub fn enter_measured(&self) -> ResourceTrackingGuard<'_> {
        self.enter_with(measured_cpu_time_start(1))
    }

    fn enter_with(&self, cpu_time_start: Option<CpuTimeStart>) -> ResourceTrackingGuard<'_> {
        // Swap the current group to the one we're tracking.
        CURRENT_GROUP.with(|current_group| {
            let mut group_ptr = current_group.borrow_mut();
            let previous_group_ptr = *group_ptr;
            *group_ptr = self.group_ptr;

            ResourceTrackingGuard {
                previous_group_ptr,
                cpu_time_start,
                _token: PhantomData,
            }
        })
    }
}

// SAFETY: There's nothing inherently thread-specific about the token.
unsafe impl Send for ResourceGroupToken {}

// SAFETY: There's nothing unsafe about sharing the token between threads, as it's safe to enter the same token on
// multiple threads at the same time, and the token itself has no internal state or interior mutability.
unsafe impl Sync for ResourceGroupToken {}

/// A guard representing an resource group which has been entered.
///
/// When the guard is dropped, the resource group will be exited and the previously entered resource group will be
/// restored.
///
/// This is returned by the [`ResourceGroupToken::enter`] and [`ResourceGroupToken::enter_measured`] methods.
pub struct ResourceTrackingGuard<'a> {
    previous_group_ptr: NonNull<ResourceStats>,
    /// The thread's CPU time when the group was entered, if this entry measures it.
    cpu_time_start: Option<CpuTimeStart>,
    _token: PhantomData<&'a ResourceGroupToken>,
}

impl Drop for ResourceTrackingGuard<'_> {
    fn drop(&mut self) {
        // Grab our current total CPU usage for the thread, and calculate the delta, scaled by the sampling rate the
        // entry was drawn at.
        let cpu_usage_delta = self.cpu_time_start.map_or(0, CpuTimeStart::attributable_since);

        // Reset the current group to the one that existed before we entered.
        CURRENT_GROUP.with(|current_group| {
            let mut group_ptr = current_group.borrow_mut();

            // Now track the delta in CPU usage, if available, before resetting the group.
            if cpu_usage_delta != 0 {
                // SAFETY: We only construct the pointer to `ResourceStats` from a leaked heap allocation, and we never
                // deallocate it, so it's always non-null/aligned/valid-for-`T`, etc.
                unsafe { group_ptr.as_ref().track_cpu_time(cpu_usage_delta) }
            }

            *group_ptr = self.previous_group_ptr;
        });
    }
}

/// An object wrapper that tracks allocations and attributes them to a specific group.
///
/// Provides methods and implementations to help ensure that operations against/using the wrapped object have all
/// allocations properly tracked and attributed to a given group.
///
/// Implements [`Future`] when the wrapped object itself implements [`Future`].
//
// TODO: A more complete example of this sort of thing is `tracing::Instrumented`, where they also have some fancy code
// to trace execution even in the drop logic of the wrapped future. I'm not sure we need that here, because we don't
// care about what components an object is deallocated in, and I don't think we expect to have any futures where the
// drop logic actually _allocates_, and certainly not in a way where we want to attribute it to that future's attached
// component... but for posterity, I'm mentioning it here since we _might_ consider doing it. Might.
#[pin_project]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Tracked<Inner> {
    token: ResourceGroupToken,

    #[pin]
    inner: Inner,
}

impl<Inner> Tracked<Inner> {
    /// Consumes this object and returns the inner object and tracking token.
    pub fn into_parts(self) -> (ResourceGroupToken, Inner) {
        (self.token, self.inner)
    }
}

impl<Inner> Future for Tracked<Inner>
where
    Inner: Future,
{
    type Output = Inner::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let _enter = this.token.enter();

        this.inner.poll(cx)
    }
}

/// Attaches resource groups to a [`Future`].
pub trait Track: Sized {
    /// Instruments this type by attaching the given resource group token, returning a `Tracked` wrapper.
    ///
    /// The resource group will be entered every time the wrapped future is polled.
    ///
    /// # Examples
    ///
    /// <!-- vale off -->
    /// ```rust
    /// use saluki_common::resource_tracking::{ResourceGroupRegistry, ResourceGroupToken, Track as _};
    ///
    /// # async fn doc() {
    /// let future = async {
    ///     // All allocations in this future will be attached to the resource group
    ///     // represented by `token`...
    /// };
    ///
    /// let token = ResourceGroupRegistry::global().register_resource_group("my-group");
    /// future
    ///     .track_resources(token)
    ///     .await
    /// # }
    /// ```
    /// <!-- vale on -->
    fn track_resources(self, token: ResourceGroupToken) -> Tracked<Self> {
        Tracked { token, inner: self }
    }

    /// Instruments this type by attaching the current resource group, returning a `Tracked` wrapper.
    ///
    /// The resource group will be entered every time the wrapped future is polled.
    ///
    /// This can be used to propagate the current resource group when spawning a new future.
    ///
    /// # Examples
    ///
    /// <!-- vale off -->
    /// ```rust
    /// use saluki_common::resource_tracking::{ResourceGroupRegistry, ResourceGroupToken, Track as _};
    ///
    /// # mod tokio {
    /// #     pub(super) fn spawn(_: impl std::future::Future) {}
    /// # }
    /// # async fn doc() {
    /// let token = ResourceGroupRegistry::global().register_resource_group("my-group");
    /// let _enter = token.enter();
    ///
    /// // ...
    ///
    /// let future = async {
    ///     // All allocations in this future will be attached to the resource group
    ///     // represented by `token`...
    /// };
    /// tokio::spawn(future.in_current_resource_group());
    /// # }
    /// ```
    /// <!-- vale on -->
    fn in_current_resource_group(self) -> Tracked<Self> {
        Tracked {
            token: ResourceGroupToken::current(),
            inner: self,
        }
    }
}

impl<T: Sized> Track for T {}

/// A registry of resource groups and the statistics for each of them.
///
/// Resource groups are user-defined groups which can then be associated with memory and CPU usage in distinct code
/// regions. This mechanism allows for granular resource accounting at the level which makes sense to the application,
/// such as per-thread, per-async task, and so on.
///
/// # Token guard
///
/// When an resource group is registered, an `ResourceGroupToken` is returned. This token can be used to "enter" the
/// group, which causes memory and CPU usage on the current thread to be attributed to that group. Entering the group
/// returns a drop guard that restores the previously entered group when dropped.
///
/// This allows for arbitrarily nested resource groups.
///
/// Additionally, [`Tracked`] can be used to wrap a [`Future`], attaching it to a specific resource group token. This
/// causes the future to track all memory and CPU usage during polls such that the usage is properly attributed to the
/// resource group.
///
/// # Resources tracked
///
/// ## Memory usage
///
/// In order for memory usage to be tracked, [`TrackingAllocator`][super::TrackingAllocator] must be installed
/// as the global allocator for the process.
///
/// ## CPU usage
///
/// CPU usage is automatically tracked if platform support is detected.
///
/// Currently, only Linux is supported for CPU usage tracking.
pub struct ResourceGroupRegistry {
    resource_groups: Mutex<HashMap<String, Box<ResourceStats>>>,
}

impl ResourceGroupRegistry {
    fn new() -> Self {
        in_root_resource_group(|| Self {
            resource_groups: Mutex::new(HashMap::with_capacity(4)),
        })
    }

    /// Gets a reference to the global resource group registry.
    pub fn global() -> &'static Self {
        REGISTRY.get_or_init(Self::new)
    }

    /// Returns `true` if `TrackingAllocator` is installed as the global allocator.
    pub fn allocator_installed() -> bool {
        // Essentially, when we load the group registry, and it gets created for the first time, it will specifically
        // allocate its internal data structures while entered into the root resource group.
        //
        // This means that if the allocator is installed, we should always have some allocations in the root group by
        // the time we call `ResourceStats::has_allocated`.
        ROOT_GROUP.has_allocated()
    }

    /// Registers a new resource group with the given name.
    ///
    /// Returns an `ResourceGroupToken` that can be used to attribute CPU and memory usage to the
    /// newly created resource group.
    pub fn register_resource_group<S>(&self, name: S) -> ResourceGroupToken
    where
        S: AsRef<str>,
    {
        in_root_resource_group(|| {
            let mut resource_groups = self.resource_groups.lock().unwrap();
            match resource_groups.get(name.as_ref()) {
                Some(stats) => ResourceGroupToken::new(NonNull::from(&**stats)),
                None => {
                    let resource_group_stats = Box::new(ResourceStats::new());
                    let token = ResourceGroupToken::new(NonNull::from(&*resource_group_stats));

                    resource_groups.insert(name.as_ref().to_string(), resource_group_stats);

                    token
                }
            }
        })
    }

    /// Visits all resource groups in the registry and calls the given closure with their names and statistics.
    pub fn visit_resource_groups<F>(&self, mut f: F)
    where
        F: FnMut(&str, &ResourceStats),
    {
        in_root_resource_group(|| {
            f("root", &ROOT_GROUP);

            let resource_groups = self.resource_groups.lock().unwrap();
            for (name, stats) in resource_groups.iter() {
                f(name, stats);
            }
        });
    }
}

fn in_root_resource_group<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    let token = ResourceGroupToken::root();
    let _enter = token.enter();
    f()
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        future::Future,
        num::NonZeroU32,
        pin::Pin,
        rc::Rc,
        task::{Context, Poll, Waker},
    };

    use super::{ResourceGroupRegistry, ResourceGroupToken, Track};
    use crate::resource_tracking::{cpu_time_sampling, set_cpu_time_sampling, testing::FakeCpuClock};

    /// Polls a future to completion on the current thread using a no-op `waker`.
    fn poll_to_completion<F: Future>(future: F) -> F::Output {
        let mut future = Box::pin(future);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
                return output;
            }
        }
    }

    /// A future that records, each time it is polled, whether the currently entered resource group is `expected`.
    struct RecordCurrentGroup {
        expected: ResourceGroupToken,
        matched: Rc<Cell<bool>>,
    }

    impl Future for RecordCurrentGroup {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.matched.set(ResourceGroupToken::current().ptr_eq(&self.expected));
            Poll::Ready(())
        }
    }

    // Real CPU time is only read on platforms where per-thread CPU time is available (Linux), so the helper that burns
    // CPU is gated to avoid dead-code warnings elsewhere.
    #[cfg(target_os = "linux")]
    fn burn_cpu() {
        let mut sum = 0u64;
        for i in 0..20_000_000u64 {
            sum = sum.wrapping_add(i);
        }
        std::hint::black_box(sum);
    }

    /// Returns the CPU time attributed to the group named `target` so far.
    fn cpu_time_of(registry: &ResourceGroupRegistry, target: &str) -> u64 {
        use crate::resource_tracking::ResourceStatsSnapshot;

        let mut cpu_time_nanos = 0;
        registry.visit_resource_groups(|name, stats| {
            if name == target {
                cpu_time_nanos = stats.snapshot_delta(&ResourceStatsSnapshot::empty()).cpu_time_nanos;
            }
        });
        cpu_time_nanos
    }

    /// Enters `group` `entries` times, with the fake clock advancing `cost` nanoseconds inside each entry.
    fn enter_repeatedly(clock: &FakeCpuClock, group: &ResourceGroupToken, entries: u64, cost: u64) {
        for _ in 0..entries {
            let _guard = group.enter();
            clock.advance(cost);
        }
    }

    #[test]
    fn every_entry_measures_its_cpu_time_at_the_default_sampling_rate() {
        let registry = ResourceGroupRegistry::new();
        let group = registry.register_resource_group("every-entry");
        let clock = FakeCpuClock::install(NonZeroU32::MIN, 1);

        enter_repeatedly(&clock, &group, 100, 30);

        // Two reads per entry, and exactly the CPU time used inside them, as without sampling.
        assert_eq!(clock.reads(), 200);
        assert_eq!(cpu_time_of(&registry, "every-entry"), 3_000);
        // Time used outside every entry is attributed to no group.
        clock.advance(1_000);
        assert_eq!(cpu_time_of(&registry, "every-entry"), 3_000);
    }

    #[test]
    fn sampled_entries_estimate_the_cpu_time_without_bias() {
        const ENTRIES: u64 = 64_000;
        const COST: u64 = 1_000;
        let registry = ResourceGroupRegistry::new();
        let group = registry.register_resource_group("sampled");
        let clock = FakeCpuClock::install(NonZeroU32::new(16).unwrap(), 42);

        enter_repeatedly(&clock, &group, ENTRIES, COST);

        // One entry in 16 reads the clock, twice, and counts what it measured 16 times.
        let sampled = clock.reads() / 2;
        assert_eq!(cpu_time_of(&registry, "sampled"), sampled * COST * 16);
        // The number of sampled entries is binomial, with a mean of 4,000 and a standard deviation of about 61, so the
        // estimate is within 10% of the CPU time used, by more than six deviations.
        let used = (ENTRIES * COST) as f64;
        let estimate = cpu_time_of(&registry, "sampled") as f64;
        assert!(
            (estimate - used).abs() < used * 0.1,
            "estimated {} for {} used",
            estimate,
            used
        );
    }

    #[test]
    fn sampling_is_not_aliased_by_a_regular_pattern_of_entries() {
        // Two groups entered in turn, at very different costs: a sampler that took every 16th entry would only ever
        // see one of them. A random one estimates both.
        const ROUNDS: u64 = 64_000;
        let registry = ResourceGroupRegistry::new();
        let long = registry.register_resource_group("long");
        let short = registry.register_resource_group("short");
        let clock = FakeCpuClock::install(NonZeroU32::new(16).unwrap(), 3);

        for _ in 0..ROUNDS {
            enter_repeatedly(&clock, &long, 1, 1_000);
            enter_repeatedly(&clock, &short, 1, 10);
        }

        for (name, cost) in [("long", 1_000), ("short", 10)] {
            let used = (ROUNDS * cost) as f64;
            let estimate = cpu_time_of(&registry, name) as f64;
            assert!(
                (estimate - used).abs() < used * 0.1,
                "estimated {} for {} used in {}",
                estimate,
                used,
                name
            );
        }
    }

    #[test]
    fn a_measured_entry_always_measures_its_cpu_time() {
        let registry = ResourceGroupRegistry::new();
        let group = registry.register_resource_group("measured");
        let clock = FakeCpuClock::install(NonZeroU32::new(1_000_000).unwrap(), 5);

        for _ in 0..10 {
            let _guard = group.enter_measured();
            clock.advance(70);
        }

        // Every entry read the clock, and counted exactly what it measured, never scaled.
        assert_eq!(clock.reads(), 20);
        assert_eq!(cpu_time_of(&registry, "measured"), 700);
    }

    #[test]
    fn nested_entries_count_their_cpu_time_in_both_groups() {
        let registry = ResourceGroupRegistry::new();
        let outer = registry.register_resource_group("nesting-outer");
        let inner = registry.register_resource_group("nesting-inner");
        let clock = FakeCpuClock::install(NonZeroU32::MIN, 1);

        {
            let _outer = outer.enter();
            clock.advance(5);
            {
                let _inner = inner.enter();
                clock.advance(20);
            }
            clock.advance(5);
        }

        assert_eq!(cpu_time_of(&registry, "nesting-inner"), 20);
        assert_eq!(cpu_time_of(&registry, "nesting-outer"), 30);
    }

    #[test]
    fn the_sampling_rate_is_set_process_wide() {
        // Under the fake clock, this thread's entries use the clock's rate, so the process-wide one only reads back.
        assert_eq!(cpu_time_sampling(), NonZeroU32::MIN);
        let sixteen = NonZeroU32::new(16).unwrap();
        set_cpu_time_sampling(sixteen);
        assert_eq!(cpu_time_sampling(), sixteen);
        set_cpu_time_sampling(NonZeroU32::MIN);
        assert_eq!(cpu_time_sampling(), NonZeroU32::MIN);
    }

    #[test]
    fn existing_group() {
        let registry = ResourceGroupRegistry::new();
        let token = registry.register_resource_group("test");
        let token2 = registry.register_resource_group("test");
        let token3 = registry.register_resource_group("test2");

        assert!(token.ptr_eq(&token2));
        assert!(!token.ptr_eq(&token3));
    }

    #[test]
    fn visit_resource_groups() {
        let registry = ResourceGroupRegistry::new();
        let _token = registry.register_resource_group("my-group");

        let mut visited = Vec::new();
        registry.visit_resource_groups(|name, _stats| {
            visited.push(name.to_string());
        });

        assert_eq!(visited.len(), 2);
        assert_eq!(visited[0], "root");
        assert_eq!(visited[1], "my-group");
    }

    #[test]
    fn enter_swaps_current_group_and_restores_previous_on_drop() {
        let registry = ResourceGroupRegistry::new();
        let group = registry.register_resource_group("group-a");
        let previous = ResourceGroupToken::current();

        {
            let _guard = group.enter();
            assert!(
                ResourceGroupToken::current().ptr_eq(&group),
                "entering a group should make it the current group"
            );
        }

        assert!(
            ResourceGroupToken::current().ptr_eq(&previous),
            "dropping the guard should restore the previously-entered group"
        );
    }

    #[test]
    fn nested_groups_restore_in_lifo_order() {
        let registry = ResourceGroupRegistry::new();
        let outer = registry.register_resource_group("outer");
        let inner = registry.register_resource_group("inner");
        let root = ResourceGroupToken::current();

        let outer_guard = outer.enter();
        assert!(ResourceGroupToken::current().ptr_eq(&outer));

        {
            let _inner_guard = inner.enter();
            assert!(ResourceGroupToken::current().ptr_eq(&inner));
        }

        // Exiting the inner group restores the outer group, not the root.
        assert!(ResourceGroupToken::current().ptr_eq(&outer));

        drop(outer_guard);
        assert!(ResourceGroupToken::current().ptr_eq(&root));
    }

    #[test]
    fn tracked_future_enters_attached_group_during_poll() {
        let registry = ResourceGroupRegistry::new();
        let group = registry.register_resource_group("tracked");
        let previous = ResourceGroupToken::current();

        let matched = Rc::new(Cell::new(false));
        let future = RecordCurrentGroup {
            expected: group,
            matched: Rc::clone(&matched),
        }
        .track_resources(group);

        poll_to_completion(future);

        assert!(
            matched.get(),
            "the attached group should be the current group while the future is polled"
        );
        assert!(
            ResourceGroupToken::current().ptr_eq(&previous),
            "the previous group should be restored once the poll returns"
        );
    }

    #[test]
    fn in_current_resource_group_captures_group_at_attach_time() {
        let registry = ResourceGroupRegistry::new();
        let group = registry.register_resource_group("captured");

        let matched = Rc::new(Cell::new(false));
        let future = {
            // Attach while `group` is entered; the wrapper should remember it.
            let _guard = group.enter();
            RecordCurrentGroup {
                expected: group,
                matched: Rc::clone(&matched),
            }
            .in_current_resource_group()
        };

        // The guard has been dropped, so `group` is no longer current...
        assert!(!ResourceGroupToken::current().ptr_eq(&group));

        // ...yet polling the wrapper still re-enters the group captured at attach time.
        poll_to_completion(future);
        assert!(matched.get());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cpu_time_is_attributed_to_the_entered_group() {
        let registry = ResourceGroupRegistry::new();
        let busy = registry.register_resource_group("busy");
        let _idle = registry.register_resource_group("idle");

        {
            let _guard = busy.enter();
            burn_cpu();
        }

        // CPU time consumed while `busy` was entered is attributed to it; `idle` was never entered.
        assert!(
            cpu_time_of(&registry, "busy") > 0,
            "the entered group should accrue the CPU time spent inside the guard"
        );
        assert_eq!(
            cpu_time_of(&registry, "idle"),
            0,
            "a group that was never entered should accrue no CPU time"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tracked_future_attributes_poll_cpu_time_to_its_group() {
        let registry = ResourceGroupRegistry::new();
        let group = registry.register_resource_group("worker");

        struct BurnCpu;

        impl Future for BurnCpu {
            type Output = ();

            fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
                burn_cpu();
                Poll::Ready(())
            }
        }

        poll_to_completion(BurnCpu.track_resources(group));

        assert!(
            cpu_time_of(&registry, "worker") > 0,
            "CPU time spent polling a tracked future should be attributed to its group"
        );
    }
}
