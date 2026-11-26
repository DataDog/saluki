//! Provides opt-in byte admission based on observed memory usage and estimated allocation growth.
//!
//! # Design
//!
//! A topology can enable a budget when it has a memory limit. Without a budget, callers do not reserve bytes or start
//! a sampler. The sampler reads cgroup usage when available, or process RSS otherwise.
//! It waits 25 ms between samples to keep observation off the admission fast path at the cost of feedback delay.
//!
//! The admission ceiling reserves one eighth of the limit, up to 128 MiB, for work that does not reserve bytes.
//! This includes receive buffers, allocator rounding, and encoder workspaces. A budget reduces accepted growth under
//! pressure but cannot guarantee a process or cgroup memory cap. The estimates and unmetered work depend on the workload.
//!
//! Reservations use a shared atomic total, so concurrent callers cannot spend the same headroom.
//! Each sample replaces the last observed usage and reconciles reservations that completed before the sample began.
//! Reservations still in progress stay charged. This can count bytes twice for one sample, but it avoids releasing
//! headroom while an allocation is still in progress.
//!
//! On pressure, a caller can reclaim idle memory, sample again, and retry before refusing the reservation.
//! Concurrent refusals share a reclaim pass. A 250 ms cooldown limits repeated cache scans at the cost of delaying
//! another pass when more idle memory becomes available.
//!
use std::{
    sync::{
        atomic::{
            AtomicUsize,
            Ordering::{Acquire, Relaxed, Release},
        },
        Arc, Mutex, Weak,
    },
    thread,
    time::{Duration, Instant},
};

const MIB: usize = 1024 * 1024;
const INGEST_CREDIT_BYTES: usize = 16 * 1024;
const SAMPLE_INTERVAL: Duration = Duration::from_millis(25);
// Bound the cost of scanning big caches while the process is already shedding load.
const RECLAIM_INTERVAL: Duration = Duration::from_millis(250);

type Usage = Box<dyn FnMut() -> Option<usize> + Send>;
type Reclaimer = dyn Fn() + Send + Sync;

/// A shared limit for allocation growth, on top of already resident memory.
///
/// Reserve growth before allocating. Hold the reservation until the operation finishes.
/// The estimate stays charged until a successful observation can replace it with measured usage.
#[derive(Clone)]
pub struct ByteBudget(Arc<State>);

struct State {
    ceiling: usize,
    used: AtomicUsize,
    completed: AtomicUsize,
    observation: Mutex<Observation>,
    pressure: Mutex<Pressure>,
}

struct Observation {
    bytes: usize,
    usage: Option<Usage>,
}

#[derive(Default)]
struct Pressure {
    last_reclaim: Option<Instant>,
    reclaimers: Vec<Weak<Reclaimer>>,
}

impl ByteBudget {
    /// Creates a budget with a fixed observation and no sampler.
    ///
    /// Without a sampler, completed reservations remain charged against the fixed ceiling.
    pub fn new(ceiling: usize, observed_bytes: usize) -> Self {
        Self::with_usage(ceiling, observed_bytes, None)
    }

    /// Starts observing the process working set when no cgroup memory limit is available.
    ///
    /// Returns `None` if the initial observation or sampler startup fails.
    pub fn for_process_memory_limit(limit: usize) -> Option<Self> {
        let mut querier = process_memory::Querier::default();
        Self::for_memory_limit(limit, move || querier.resident_set_size())
    }

    fn with_usage(ceiling: usize, bytes: usize, usage: Option<Usage>) -> Self {
        Self(Arc::new(State {
            ceiling,
            used: AtomicUsize::new(bytes),
            completed: AtomicUsize::new(0),
            observation: Mutex::new(Observation { bytes, usage }),
            pressure: Mutex::new(Pressure::default()),
        }))
    }

    /// Starts observing resident bytes against a memory limit, holding back headroom for unmetered work.
    ///
    /// The caller supplies the usage observation, such as `memory.current` for a cgroup v2 limit.
    /// The ceiling leaves one eighth of the limit, up to 128 MiB, outside admission accounting.
    ///
    /// Returns `None` if the initial observation or sampler startup fails.
    pub fn for_memory_limit(limit: usize, mut usage: impl FnMut() -> Option<usize> + Send + 'static) -> Option<Self> {
        let initial = usage()?;
        let headroom = (limit / 8).min(128 * MIB);
        let budget = Self::with_usage(limit - headroom, initial, Some(Box::new(usage)));
        let weak = Arc::downgrade(&budget.0);
        thread::Builder::new()
            .name("byte-budget-sampler".into())
            .spawn(move || loop {
                thread::sleep(SAMPLE_INTERVAL);
                let Some(state) = weak.upgrade() else { break };
                state.refresh_usage();
            })
            .ok()?;
        Some(budget)
    }

    /// Registers a reclamation callback while the returned handle remains live.
    ///
    /// Reclamation runs synchronously under the pressure lock. Do not reenter this budget from the callback.
    pub fn register_reclaimer(&self, reclaim: impl Fn() + Send + Sync + 'static) -> BudgetReclaimer {
        let reclaim = Arc::new(reclaim) as Arc<Reclaimer>;
        let mut pressure = self.0.pressure.lock().unwrap();
        pressure.reclaimers.push(Arc::downgrade(&reclaim));
        pressure.last_reclaim = None;
        BudgetReclaimer { _reclaim: reclaim }
    }

    /// Creates a local allowance for synchronous operations to avoid a shared atomic update per line.
    ///
    /// The batch requests credits in chunks of at least 16 KiB when space allows. Unused credits can temporarily
    /// reduce available headroom for other callers.
    pub fn batch(&self) -> ByteBudgetBatch {
        ByteBudgetBatch {
            budget: self.clone(),
            remaining: 0,
            spent: 0,
        }
    }

    /// Reserves an upper estimate of bytes to be allocated by the operation.
    ///
    /// Returns `None` if the reservation still cannot fit after any eligible idle-memory reclaim.
    /// A zero-byte reservation succeeds without reclamation or an atomic charge.
    #[inline]
    pub fn try_reserve(&self, bytes: usize) -> Option<ByteReservation<'_>> {
        (self.charge(bytes) || self.reclaim_and_charge(bytes)).then(|| ByteReservation { state: &self.0, bytes })
    }

    #[inline]
    fn charge(&self, bytes: usize) -> bool {
        bytes == 0
            || self
                .0
                .used
                .fetch_update(Relaxed, Relaxed, |used| {
                    used.checked_add(bytes).filter(|next| *next <= self.0.ceiling)
                })
                .is_ok()
    }

    #[cold]
    fn reclaim_and_charge(&self, bytes: usize) -> bool {
        let mut pressure = self.0.pressure.lock().unwrap();
        if self.charge(bytes) {
            return true;
        }
        if pressure
            .last_reclaim
            .is_none_or(|last| last.elapsed() >= RECLAIM_INTERVAL)
        {
            pressure.reclaimers.retain(|reclaim| {
                let Some(reclaim) = reclaim.upgrade() else { return false };
                reclaim();
                true
            });
            self.0.refresh_usage();
            pressure.last_reclaim = Some(Instant::now());
        }
        self.charge(bytes)
    }
}

impl State {
    fn refresh_usage(&self) {
        let mut observation = self.observation.lock().unwrap();
        let Some(usage) = &mut observation.usage else { return };
        // Only reconcile operations that finish before the sample starts. Keep later completions for the next sample,
        // even if the current observation already includes some of their bytes.
        let completed = self.completed.swap(0, Acquire);
        let Some(next) = usage() else {
            self.completed.fetch_add(completed, Relaxed);
            return;
        };
        // Apply the change in the baseline without overwriting concurrent reservations in the shared total.
        let previous = observation.bytes + completed;
        if next >= previous {
            self.used.fetch_add(next - previous, Relaxed);
        } else {
            self.used.fetch_sub(previous - next, Relaxed);
        }
        observation.bytes = next;
    }
}

/// Keeps one idle-memory reclamation callback registered.
#[derive(Clone)]
#[must_use]
pub struct BudgetReclaimer {
    _reclaim: Arc<Reclaimer>,
}

/// Local credits reserved against a shared [`ByteBudget`].
///
/// Finish each operation synchronously before spending more credits or dropping this batch.
/// Refilling or dropping the batch returns unused credits and marks spent credits complete for reconciliation.
pub struct ByteBudgetBatch {
    budget: ByteBudget,
    remaining: usize,
    spent: usize,
}

impl ByteBudgetBatch {
    /// Returns `true` if the synchronous operation can spend the requested credits.
    #[inline]
    pub fn try_spend(&mut self, bytes: usize) -> bool {
        if bytes > self.remaining && !self.refill(bytes) {
            return false;
        }
        self.remaining -= bytes;
        self.spent += bytes;
        true
    }

    #[cold]
    fn refill(&mut self, bytes: usize) -> bool {
        self.publish();
        let chunk = bytes.max(INGEST_CREDIT_BYTES);
        if self.budget.charge(chunk) {
            self.remaining = chunk;
        } else if (chunk != bytes && self.budget.charge(bytes)) || self.budget.reclaim_and_charge(bytes) {
            self.remaining = bytes;
        } else {
            return false;
        }
        true
    }

    fn publish(&mut self) {
        let remaining = std::mem::take(&mut self.remaining);
        let spent = std::mem::take(&mut self.spent);
        if remaining != 0 {
            self.budget.0.used.fetch_sub(remaining, Relaxed);
        }
        if spent != 0 {
            self.budget.0.completed.fetch_add(spent, Release);
        }
    }
}

impl Drop for ByteBudgetBatch {
    fn drop(&mut self) {
        self.publish();
    }
}

/// Keeps an operation's estimated growth charged until that operation finishes.
///
/// Dropping the reservation marks it complete. A later observation reconciles it with actual usage.
#[must_use]
pub struct ByteReservation<'a> {
    state: &'a State,
    bytes: usize,
}

impl Drop for ByteReservation<'_> {
    #[inline]
    fn drop(&mut self) {
        if self.bytes != 0 {
            self.state.completed.fetch_add(self.bytes, Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;

    #[test]
    fn concurrent_reservations_do_not_overspend() {
        let budget = ByteBudget::new(100, 4);
        let admitted = AtomicUsize::new(0);
        thread::scope(|scope| {
            for _ in 0..32 {
                scope.spawn(|| {
                    if budget.try_reserve(8).is_some() {
                        admitted.fetch_add(1, Relaxed);
                    }
                });
            }
        });
        assert_eq!(admitted.load(Relaxed), 12);
        assert!(budget.try_reserve(1).is_none());
        assert!(budget.try_reserve(usize::MAX).is_none());
        assert!(budget.try_reserve(0).is_some());
    }

    #[test]
    fn batched_ingest_shares_the_limit_and_returns_unused_credit() {
        let budget = ByteBudget::new(INGEST_CREDIT_BYTES + 10, 0);
        let mut batch = budget.batch();
        assert!(batch.try_spend(6));
        assert!(batch.try_spend(4));
        assert!(budget.try_reserve(11).is_none());
        drop(batch);
        assert!(budget.try_reserve(INGEST_CREDIT_BYTES).is_some());
        assert!(budget.try_reserve(1).is_none());
        assert_eq!(budget.0.completed.load(Relaxed), INGEST_CREDIT_BYTES + 10);
    }

    #[test]
    fn observation_reconciles_only_completed_growth() {
        let usage = Arc::new(AtomicUsize::new(40));
        let sample = Arc::clone(&usage);
        let budget = ByteBudget::with_usage(100, 40, Some(Box::new(move || Some(sample.load(Relaxed)))));
        let in_flight = budget.try_reserve(40).unwrap();
        drop(budget.try_reserve(20).unwrap());

        usage.store(60, Relaxed);
        budget.0.refresh_usage();
        assert!(budget.try_reserve(1).is_none());
        drop(in_flight);

        usage.store(75, Relaxed);
        budget.0.refresh_usage();
        assert!(budget.try_reserve(26).is_none());
        assert!(budget.try_reserve(25).is_some());
    }

    #[test]
    fn failed_sample_keeps_reservations() {
        let budget = ByteBudget::with_usage(10, 0, Some(Box::new(|| None)));
        drop(budget.try_reserve(10).unwrap());
        budget.0.refresh_usage();
        assert!(budget.try_reserve(1).is_none());
        assert_eq!(budget.0.completed.load(Relaxed), 10);
    }

    #[test]
    fn pressure_reclaims_before_retry_or_refusal() {
        let reclaimed = Arc::new(AtomicBool::new(false));
        let sample = Arc::clone(&reclaimed);
        let budget = ByteBudget::with_usage(
            100,
            95,
            Some(Box::new(move || Some(if sample.load(Relaxed) { 80 } else { 95 }))),
        );
        let flag = Arc::clone(&reclaimed);
        let _reclaimer = budget.register_reclaimer(move || flag.store(true, Relaxed));
        assert!(budget.try_reserve(10).is_some());
        assert!(reclaimed.load(Relaxed));
        assert!(budget.try_reserve(11).is_none());
    }
}
