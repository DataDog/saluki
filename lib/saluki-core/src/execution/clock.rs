//! Admission clocks.

// The `loom` models below run this module's own `SourceClock`: under the feature, its lock and its epoch mirror are
// `loom`'s, which only a `loom::model` can run.
#[cfg(not(all(test, feature = "loom")))]
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, MutexGuard,
};
use std::{
    fmt,
    sync::{Arc, PoisonError},
    time::{Duration, SystemTime},
};

#[cfg(all(test, feature = "loom"))]
use loom::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, MutexGuard,
};
use tokio::sync::watch;

use super::header::AdmissionTime;

/// A source of wall-clock time for admission clocks.
///
/// Admission clocks read the wall clock through this trait so that tests and replays can control what they read. The
/// wall clock may move backward; [`AdmissionClock`] decides what a backward move means.
pub trait WallClock: fmt::Debug + Send + Sync {
    /// Returns the current wall-clock time.
    fn now(&self) -> AdmissionTime;
}

/// The operating system's wall clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemWallClock;

impl WallClock for SystemWallClock {
    fn now(&self) -> AdmissionTime {
        // A wall clock set before the Unix epoch reads as the epoch, and one beyond the year 2554 saturates.
        let since_epoch = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        AdmissionTime::UNIX_EPOCH.saturating_add(since_epoch)
    }
}

/// A wall clock that moves only when told to.
///
/// Clones share the same time, so a test can hand one clone to an admission point and keep another to move time.
#[cfg(any(test, feature = "test-util"))]
#[derive(Clone, Debug, Default)]
pub struct ManualWallClock {
    nanos: Arc<std::sync::atomic::AtomicU64>,
}

#[cfg(any(test, feature = "test-util"))]
impl ManualWallClock {
    /// Creates a clock that reads `start` until it is moved.
    pub fn new(start: AdmissionTime) -> Self {
        Self {
            nanos: Arc::new(std::sync::atomic::AtomicU64::new(start.as_unix_nanos())),
        }
    }

    /// Sets the time the clock reads, which may be earlier than the current reading.
    pub fn set(&self, time: AdmissionTime) {
        self.nanos
            .store(time.as_unix_nanos(), std::sync::atomic::Ordering::SeqCst);
    }

    /// Moves the clock forward by `duration`, saturating at the largest representable time.
    pub fn advance(&self, duration: Duration) {
        self.set(self.now().saturating_add(duration));
    }
}

#[cfg(any(test, feature = "test-util"))]
impl WallClock for ManualWallClock {
    fn now(&self) -> AdmissionTime {
        AdmissionTime::from_unix_nanos(self.nanos.load(std::sync::atomic::Ordering::SeqCst))
    }
}

/// One reading of an admission clock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockStamp {
    /// The admission time `t_a`.
    pub admitted_at: AdmissionTime,

    /// The clock's epoch after this reading.
    pub epoch: u32,

    /// Whether this reading reset the clock, which started the epoch it carries.
    pub reset: bool,

    /// The latest admission time the clock had assigned before this reading, if it had assigned any: read in the
    /// same critical section as the reading, so that with a shared clock it is never another point's later stamp.
    pub previous: Option<AdmissionTime>,
}

/// The clock of one admission point.
///
/// The clock assigns every batch admitted at its point an admission time `t_a = max(previous t_a, wall clock)`, so
/// admission times never decrease. A wall clock that moves backward by at most the **reset threshold** is absorbed:
/// the clock holds its latest admission time until the wall clock catches up. A backward move larger than the
/// threshold, measured from the latest admission time, is a **clock reset**: the clock restarts from the wall clock
/// and increments its **epoch**, so that downstream consumers can tell a reset from a fault and close what the
/// previous epoch fed. Without resets, admission times would freeze for the length of a large jump and put all of that
/// time's input at one instant.
///
/// Epochs start at 0 and wrap after `u32::MAX` resets, so compare them for equality rather than order.
#[derive(Debug)]
pub struct AdmissionClock {
    wall: Arc<dyn WallClock>,
    reset_threshold: Duration,
    latest: Option<AdmissionTime>,
    epoch: u32,
}

impl AdmissionClock {
    /// Creates a clock that reads the operating system's wall clock.
    ///
    /// `reset_threshold` is the largest backward wall-clock jump the clock absorbs.
    pub fn new(reset_threshold: Duration) -> Self {
        Self::with_wall_clock(reset_threshold, Arc::new(SystemWallClock))
    }

    /// Creates a clock that reads the given wall clock.
    ///
    /// `reset_threshold` is the largest backward wall-clock jump the clock absorbs.
    pub fn with_wall_clock(reset_threshold: Duration, wall: Arc<dyn WallClock>) -> Self {
        Self {
            wall,
            reset_threshold,
            latest: None,
            epoch: 0,
        }
    }

    /// Returns the largest backward wall-clock jump this clock absorbs.
    pub fn reset_threshold(&self) -> Duration {
        self.reset_threshold
    }

    /// Returns the latest admission time this clock assigned, if it has assigned any.
    pub fn latest(&self) -> Option<AdmissionTime> {
        self.latest
    }

    /// Returns the clock's current epoch.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Reads the wall clock this clock stamps from, without stamping anything.
    pub(crate) fn wall_now(&self) -> AdmissionTime {
        self.wall.now()
    }

    /// Reads the wall clock and assigns the next admission time.
    pub fn stamp(&mut self) -> ClockStamp {
        let wall = self.wall.now();
        let (admitted_at, reset) = match self.latest {
            None => (wall, false),
            Some(latest) if wall >= latest => (wall, false),
            Some(latest) => {
                let jump = latest.as_unix_nanos() - wall.as_unix_nanos();
                let threshold = u64::try_from(self.reset_threshold.as_nanos()).unwrap_or(u64::MAX);
                if jump > threshold {
                    (wall, true)
                } else {
                    (latest, false)
                }
            }
        };

        if reset {
            self.epoch = self.epoch.wrapping_add(1);
        }
        let previous = self.latest.replace(admitted_at);

        ClockStamp {
            admitted_at,
            epoch: self.epoch,
            reset,
            previous,
        }
    }
}

/// One admission clock that every admission point of one source stamps from.
///
/// A source whose output is split into lanes, each lane an admission point stamped by a task of its own, shares one
/// clock among the lanes' points ([`AdmissionPointConfig::with_source_clock`]): one latest admission time, one epoch,
/// and one reset decision for the source. Clocks of their own would each absorb or reset a backward jump separately, so
/// that one lane could reset while another, a few milliseconds behind, absorbs the jump and never moves to the new
/// epoch.
///
/// Stamping takes one short critical section. The epoch is mirrored in an atomic, which a lane reads after every batch
/// without the lock to see a reset another lane detected, and in a [`watch`] channel, which an idle lane waits on.
///
/// With the `measurement` feature, every stamp records how long it waited for the lock and how long it held it, in the
/// measurement probes (the crate's `probes` module) `source_clock_lock_wait/<source>` and
/// `source_clock_lock_hold/<source>`, which show whether the lanes of a source contend on its clock. `<source>` is the
/// name the clock was created with ([`named`][Self::named]), `source` by default.
///
/// [`AdmissionPointConfig::with_source_clock`]: super::AdmissionPointConfig::with_source_clock
#[derive(Debug)]
pub struct SourceClock {
    clock: Mutex<AdmissionClock>,
    epoch: AtomicU32,
    resets: watch::Sender<u32>,
    #[cfg(feature = "measurement")]
    probes: ClockProbes,
}

/// The measurement probes of a source clock's lock.
#[cfg(feature = "measurement")]
#[derive(Debug)]
struct ClockProbes {
    lock_wait: Arc<crate::probes::Histogram>,
    lock_hold: Arc<crate::probes::Histogram>,
}

impl SourceClock {
    /// Creates a source clock from the clock every point of the source stamps from.
    pub fn new(clock: AdmissionClock) -> Arc<Self> {
        Self::named(clock, "source")
    }

    /// Creates a source clock for the source `source` from the clock every point of the source stamps from.
    ///
    /// The name only labels the clock's measurement probes (see the [type documentation][Self]).
    pub fn named(clock: AdmissionClock, source: &str) -> Arc<Self> {
        #[cfg(not(feature = "measurement"))]
        let _ = source;
        let epoch = clock.epoch();
        let (resets, _) = watch::channel(epoch);
        Arc::new(Self {
            clock: Mutex::new(clock),
            epoch: AtomicU32::new(epoch),
            resets,
            #[cfg(feature = "measurement")]
            probes: ClockProbes {
                lock_wait: crate::probes::histogram(&format!("source_clock_lock_wait/{}", source)),
                lock_hold: crate::probes::histogram(&format!("source_clock_lock_hold/{}", source)),
            },
        })
    }

    /// Reads the wall clock and assigns the next admission time, as [`AdmissionClock::stamp`] does.
    ///
    /// A stamp that resets the clock updates the epoch mirror and notifies every subscriber before the lock is
    /// released, so the mirror never shows an epoch the clock has not reached.
    pub fn stamp(&self) -> ClockStamp {
        #[cfg(feature = "measurement")]
        let lock_wait = crate::probes::Span::start(&self.probes.lock_wait);
        let mut clock = self.lock();
        #[cfg(feature = "measurement")]
        drop(lock_wait);
        // Declared after the guard, so that it ends just before the lock is released.
        #[cfg(feature = "measurement")]
        let _lock_hold = crate::probes::Span::start(&self.probes.lock_hold);
        let stamp = clock.stamp();
        if stamp.reset {
            self.epoch.store(stamp.epoch, Ordering::Release);
            self.resets.send_replace(stamp.epoch);
        }
        stamp
    }

    /// Returns the latest admission time the clock assigned, at any point of the source, if it has assigned any.
    pub fn latest(&self) -> Option<AdmissionTime> {
        self.lock().latest()
    }

    /// Returns the clock's current epoch, from its mirror, without taking the lock.
    pub fn epoch(&self) -> u32 {
        self.epoch.load(Ordering::Acquire)
    }

    /// Reads the wall clock the clock stamps from, without stamping anything.
    pub fn wall_now(&self) -> AdmissionTime {
        self.lock().wall_now()
    }

    /// Returns the largest backward wall-clock jump the clock absorbs.
    pub fn reset_threshold(&self) -> Duration {
        self.lock().reset_threshold()
    }

    /// Returns a receiver of the clock's epoch, which changes at every reset, for a lane that waits on it while its
    /// input is idle.
    pub fn subscribe(&self) -> watch::Receiver<u32> {
        self.resets.subscribe()
    }

    fn lock(&self) -> MutexGuard<'_, AdmissionClock> {
        // The clock is consistent after every statement, so a panic while it is held leaves nothing half-written.
        self.clock.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use proptest::{collection::vec, prelude::*};

    use super::*;

    const THRESHOLD: Duration = Duration::from_secs(600);

    fn secs(secs: u64) -> AdmissionTime {
        AdmissionTime::from_unix_secs(secs)
    }

    /// Feeds `readings` to a fresh clock with a 600 second threshold and returns each stamp as `(t_a, epoch, reset)`.
    fn stamps(readings: &[AdmissionTime]) -> Vec<(AdmissionTime, u32, bool)> {
        let wall = ManualWallClock::new(AdmissionTime::UNIX_EPOCH);
        let mut clock = AdmissionClock::with_wall_clock(THRESHOLD, Arc::new(wall.clone()));
        readings
            .iter()
            .map(|reading| {
                wall.set(*reading);
                let stamp = clock.stamp();
                (stamp.admitted_at, stamp.epoch, stamp.reset)
            })
            .collect()
    }

    #[test]
    fn the_clock_latches_the_largest_wall_clock_reading() {
        // Forward moves are taken as they are, including repeated readings.
        assert_eq!(
            stamps(&[secs(100), secs(100), secs(101), secs(150)]),
            vec![
                (secs(100), 0, false),
                (secs(100), 0, false),
                (secs(101), 0, false),
                (secs(150), 0, false)
            ]
        );
    }

    #[test]
    fn small_backward_jumps_are_absorbed() {
        // The specification's "small clock jump back" trace: the admission clock is at 103 and the wall clock reads 97,
        // so `t_a` stays at 103 until the wall clock passes it again.
        assert_eq!(
            stamps(&[secs(103), secs(97), secs(102), secs(104)]),
            vec![
                (secs(103), 0, false),
                (secs(103), 0, false),
                (secs(103), 0, false),
                (secs(104), 0, false)
            ]
        );
    }

    #[test]
    fn a_jump_of_exactly_the_threshold_is_absorbed() {
        assert_eq!(
            stamps(&[secs(1_000), secs(400)]),
            vec![(secs(1_000), 0, false), (secs(1_000), 0, false)]
        );
    }

    #[test]
    fn a_large_backward_jump_resets_the_clock_and_starts_a_new_epoch() {
        // The specification's "large clock jump back" trace: the admission clock is at 7,203 and the wall clock jumps
        // back one hour. The clock restarts from the wall clock in epoch 1, then latches as usual within that epoch.
        assert_eq!(
            stamps(&[secs(7_203), secs(3_603), secs(3_604), secs(3_600)]),
            vec![
                (secs(7_203), 0, false),
                (secs(3_603), 1, true),
                (secs(3_604), 1, false),
                (secs(3_604), 1, false)
            ]
        );
    }

    #[test]
    fn backward_jumps_are_measured_from_the_latched_time() {
        // Each reading is only 400 seconds behind the previous reading, but the third is 800 seconds behind the latched
        // admission time, so it resets the clock.
        assert_eq!(
            stamps(&[secs(2_000), secs(1_600), secs(1_200)]),
            vec![(secs(2_000), 0, false), (secs(2_000), 0, false), (secs(1_200), 1, true)]
        );
    }

    #[test]
    fn every_reset_increments_the_epoch() {
        assert_eq!(
            stamps(&[secs(5_000), secs(1_000), secs(9_000), secs(10)]),
            vec![
                (secs(5_000), 0, false),
                (secs(1_000), 1, true),
                (secs(9_000), 1, false),
                (secs(10), 2, true)
            ]
        );
    }

    #[test]
    fn the_clock_reports_its_latest_time_and_epoch() {
        let wall = ManualWallClock::new(secs(50));
        let mut clock = AdmissionClock::with_wall_clock(THRESHOLD, Arc::new(wall.clone()));
        assert_eq!(clock.latest(), None);
        assert_eq!(clock.epoch(), 0);
        assert_eq!(clock.reset_threshold(), THRESHOLD);

        clock.stamp();
        wall.advance(Duration::from_millis(1_500));
        let stamp = clock.stamp();
        assert_eq!(stamp.admitted_at, AdmissionTime::from_unix_nanos(51_500_000_000));
        assert_eq!(clock.latest(), Some(stamp.admitted_at));
    }

    #[test]
    fn a_source_clock_resets_once_for_every_point_and_mirrors_its_epoch() {
        let wall = ManualWallClock::new(secs(7_203));
        let clock = SourceClock::new(AdmissionClock::with_wall_clock(THRESHOLD, Arc::new(wall.clone())));
        let mut resets = clock.subscribe();
        let stamp = |admitted_at, epoch, reset, previous| ClockStamp {
            admitted_at,
            epoch,
            reset,
            previous,
        };
        assert_eq!(clock.stamp(), stamp(secs(7_203), 0, false, None));

        // One stamp detects the jump, and a stamp of another point right after it is already in the new epoch. Each
        // stamp carries the latest admission time before it, at any point, the reset's included.
        wall.set(secs(3_603));
        assert_eq!(clock.stamp(), stamp(secs(3_603), 1, true, Some(secs(7_203))));
        assert_eq!(clock.epoch(), 1);
        assert!(resets.has_changed().unwrap());
        assert_eq!(*resets.borrow_and_update(), 1);
        wall.set(secs(3_602));
        assert_eq!(clock.stamp(), stamp(secs(3_603), 1, false, Some(secs(3_603))));
        assert!(!resets.has_changed().unwrap());
        assert_eq!(clock.latest(), Some(secs(3_603)));
        assert_eq!(clock.wall_now(), secs(3_602));
        assert_eq!(clock.reset_threshold(), THRESHOLD);
    }

    #[cfg(feature = "measurement")]
    #[test]
    fn source_clock_probes_record_how_long_each_stamp_waits_for_the_lock_and_holds_it() {
        use crate::probes;

        let _probes = probes::tests::PROBES_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let wall = ManualWallClock::new(secs(100));
        let clock = SourceClock::named(
            AdmissionClock::with_wall_clock(THRESHOLD, Arc::new(wall)),
            "clock_probe_test",
        );
        let probe = |name: &str| probes::snapshot()[&format!("{}/clock_probe_test", name)].clone();
        let (wait_before, hold_before) = (probe("source_clock_lock_wait"), probe("source_clock_lock_hold"));

        // While another point holds the lock, a stamp waits for it.
        probes::set_timing(true);
        let held = clock.lock();
        let stamping = std::thread::spawn({
            let clock = Arc::clone(&clock);
            move || clock.stamp()
        });
        std::thread::sleep(Duration::from_millis(5));
        drop(held);
        stamping.join().expect("the stamp completes");
        clock.stamp();
        probes::set_timing(false);

        let (wait, hold) = (
            probe("source_clock_lock_wait").since(&wait_before),
            probe("source_clock_lock_hold").since(&hold_before),
        );
        assert_eq!((wait.count, hold.count), (2, 2), "every stamp records both");
        assert!(
            wait.max >= 5_000_000,
            "the stamp that met a held lock waited: {:?}",
            wait
        );
    }

    #[test]
    fn the_system_wall_clock_reads_the_current_time() {
        let before = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap();
        let reading = SystemWallClock.now();
        let after = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap();
        assert!(u128::from(reading.as_unix_nanos()) >= before.as_nanos());
        assert!(u128::from(reading.as_unix_nanos()) <= after.as_nanos());
    }

    /// How the wall clock moves before one stamp, measured from the latest admission time the source clock assigned.
    #[derive(Clone, Copy, Debug)]
    enum Move {
        /// Forward by this many milliseconds, or not at all: several points stamping at one reading.
        Forward(u64),

        /// Back by the threshold less this many nanoseconds: at most the threshold, so the clock absorbs it.
        Absorbed(u64),

        /// Back by the threshold and this many nanoseconds more: beyond it, so the clock resets.
        Reset(u64),
    }

    const THRESHOLD_NANOS: u64 = 600_000_000_000;

    fn arb_move() -> impl Strategy<Value = Move> {
        // Jumps near the threshold, on either side of it, are the ones per-point clocks would decide differently.
        prop_oneof![
            4 => (0u64..2_000).prop_map(Move::Forward),
            1 => (0u64..1_000).prop_map(Move::Absorbed),
            1 => (0u64..=THRESHOLD_NANOS).prop_map(Move::Absorbed),
            1 => (1u64..1_000).prop_map(Move::Reset),
            1 => (1u64..2_000_000_000).prop_map(Move::Reset),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        // Several points stamp one source clock, in any order, while the wall clock jumps back, near the threshold and
        // beyond it. Whichever point's stamp meets a jump beyond the threshold resets the clock once, every later stamp
        // of every point is in the new epoch, and the mirror and the watch follow at once. The expected stamps come
        // from one latest admission time and one epoch for the whole source, which is what one clock per source means.
        #[test]
        fn property_test_source_clock_resets_once_per_jump(
            points in 1usize..=4,
            steps in vec((0usize..4, arb_move()), 1..64),
        ) {
            const START: u64 = 1_000_000_000_000_000;
            let wall = ManualWallClock::new(AdmissionTime::from_unix_nanos(START));
            let clock = SourceClock::new(AdmissionClock::with_wall_clock(THRESHOLD, Arc::new(wall.clone())));
            let mut resets = clock.subscribe();
            let mut latest: Option<u64> = None;
            let mut epoch = 0u32;
            let mut jumps = 0u32;
            let mut point_epochs = vec![None; points];

            for (point, step) in steps {
                let point = point % points;
                let (reading, admitted_at, reset) = match (latest, step) {
                    (None, _) => (START, START, false),
                    (Some(latest), Move::Forward(millis)) => {
                        let reading = latest + millis * 1_000_000;
                        (reading, reading, false)
                    }
                    (Some(latest), Move::Absorbed(less)) => (latest - (THRESHOLD_NANOS - less), latest, false),
                    (Some(latest), Move::Reset(more)) => {
                        let reading = latest - (THRESHOLD_NANOS + more);
                        (reading, reading, true)
                    }
                };
                if reset {
                    jumps += 1;
                    epoch += 1;
                }
                let previous = latest.replace(admitted_at).map(AdmissionTime::from_unix_nanos);

                wall.set(AdmissionTime::from_unix_nanos(reading));
                let stamp = clock.stamp();
                prop_assert_eq!(
                    stamp,
                    ClockStamp { admitted_at: AdmissionTime::from_unix_nanos(admitted_at), epoch, reset, previous },
                    "point {} stamping at {}", point, reading
                );
                prop_assert_eq!(clock.epoch(), epoch);
                prop_assert_eq!(clock.latest(), Some(stamp.admitted_at));
                prop_assert_eq!(resets.has_changed().unwrap(), reset);
                if reset {
                    prop_assert_eq!(*resets.borrow_and_update(), epoch);
                }
                // No point ever stamps an epoch older than one it already stamped in.
                prop_assert!(point_epochs[point].is_none_or(|previous| previous <= stamp.epoch));
                point_epochs[point] = Some(stamp.epoch);
            }
            prop_assert_eq!(clock.epoch(), jumps, "one epoch per jump beyond the threshold");
        }
    }
}

// Loom models of the source clock: several points, each stamped by a task of its own, stamp one clock across a backward
// jump of the wall clock beyond the reset threshold, and read its epoch mirror as their lanes do.
//
// The models run this module's own `SourceClock` code, whose lock and mirror are `loom`'s under the feature (the watch
// channel is tokio's, which loom does not model; it is sent under the same lock as the mirror). Two variants show what
// the models catch: per-point clocks, which reset once per point, and a clock that publishes its new epoch after it
// releases its lock, whose mirror can lag a stamp another point already took in the new epoch.
#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use loom::{sync::atomic::AtomicU64, thread};

    use super::*;

    const THRESHOLD: Duration = Duration::from_secs(300);

    /// The wall clock before the jump, and after it: an hour back.
    const BEFORE: u64 = 7_203_000_000_000;
    const AFTER: u64 = 3_603_000_000_000;

    /// A wall clock that reads one nanosecond later every time it is read, so that the order of the readings, which
    /// `loom` explores with the order of the stamps that take them, shows in the admission times.
    #[derive(Debug)]
    struct TickingWall {
        nanos: AtomicU64,
    }

    impl WallClock for TickingWall {
        fn now(&self) -> AdmissionTime {
            AdmissionTime::from_unix_nanos(self.nanos.fetch_add(1, Ordering::SeqCst))
        }
    }

    /// How the points stamp in a model: the source clock as built, or one of the variants the models reject.
    #[derive(Clone, Copy)]
    enum Stamping {
        /// Every point stamps the one source clock.
        Shared,

        /// Every point stamps the source clock, which publishes a new epoch only after it releases its lock.
        PublishingAfterUnlock,

        /// Every point stamps a clock of its own, all reading the same wall clock.
        PerPoint,
    }

    /// Stamps `clock` as `SourceClock::stamp` does, but publishes a new epoch to the mirror and the watch only after
    /// the lock is released.
    fn stamp_publishing_after_unlock(clock: &SourceClock) -> ClockStamp {
        let stamp = clock.lock().stamp();
        if stamp.reset {
            clock.epoch.store(stamp.epoch, Ordering::Release);
            clock.resets.send_replace(stamp.epoch);
        }
        stamp
    }

    /// Runs three points, each stamping twice, after the wall clock jumped back an hour from where the source clock
    /// last stamped, at every interleaving `loom` explores at a preemption bound of three.
    ///
    /// Each point checks, after each stamp, that the mirror shows at least that stamp's epoch (it never lags a stamp
    /// taken), and before its second stamp, that the stamp is in at least the epoch the mirror showed (it never shows
    /// an epoch the clock has not reached). At the end: exactly one reset, every stamp in the new epoch, the reset the
    /// earliest of them, and admission times that never decrease within the epoch.
    fn model(stamping: Stamping) {
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(3);
        builder.check(move || {
            let wall = Arc::new(TickingWall {
                nanos: AtomicU64::new(BEFORE),
            });
            let source = SourceClock::new(AdmissionClock::with_wall_clock(
                THRESHOLD,
                Arc::clone(&wall) as Arc<dyn WallClock>,
            ));
            source.stamp();
            let owns = (0..3)
                .map(|_| {
                    let mut own = AdmissionClock::with_wall_clock(THRESHOLD, Arc::clone(&wall) as Arc<dyn WallClock>);
                    own.stamp();
                    own
                })
                .collect::<Vec<_>>();
            wall.nanos.store(AFTER, Ordering::SeqCst);

            let points = owns
                .into_iter()
                .map(|mut own| {
                    let source = Arc::clone(&source);
                    thread::spawn(move || {
                        let mut stamps = Vec::new();
                        for _ in 0..2 {
                            let mirror = source.epoch();
                            let stamp = match stamping {
                                Stamping::Shared => source.stamp(),
                                Stamping::PublishingAfterUnlock => stamp_publishing_after_unlock(&source),
                                Stamping::PerPoint => own.stamp(),
                            };
                            assert!(
                                stamp.epoch >= mirror,
                                "violated: the mirror showed epoch {} before a stamp in epoch {}",
                                mirror,
                                stamp.epoch
                            );
                            if !matches!(stamping, Stamping::PerPoint) {
                                assert!(
                                    source.epoch() >= stamp.epoch,
                                    "violated: the mirror lags a stamp in epoch {}",
                                    stamp.epoch
                                );
                            }
                            stamps.push(stamp);
                        }
                        stamps
                    })
                })
                .collect::<Vec<_>>();

            let per_point = points
                .into_iter()
                .map(|point| point.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)))
                .collect::<Vec<_>>();
            for stamps in &per_point {
                assert!(
                    stamps[0].admitted_at < stamps[1].admitted_at,
                    "violated: a point's admission times decreased within an epoch"
                );
            }
            let stamps = per_point.into_iter().flatten().collect::<Vec<_>>();
            let resets = stamps.iter().filter(|stamp| stamp.reset).collect::<Vec<_>>();
            assert_eq!(resets.len(), 1, "violated: {} resets for one jump", resets.len());
            assert!(
                stamps.iter().all(|stamp| stamp.epoch == 1),
                "violated: a stamp after the jump is not in the new epoch"
            );
            assert!(
                stamps.iter().all(|stamp| stamp.admitted_at >= resets[0].admitted_at),
                "violated: a stamp in the new epoch is earlier than the reset that started it"
            );
            assert_eq!(source.epoch(), 1);
            assert_eq!(*source.subscribe().borrow(), 1);
        });
    }

    #[test]
    fn a_source_clock_resets_once_for_every_point_and_its_mirror_follows_every_stamp() {
        model(Stamping::Shared);
    }

    // Publishing after the lock is released lets another point stamp in the new epoch, under the lock, before the
    // mirror moves: loom finds the interleaving where that point reads a mirror behind its own stamp.
    #[test]
    #[should_panic(expected = "violated: ")]
    fn publishing_the_epoch_after_the_lock_lets_the_mirror_lag_a_stamp() {
        model(Stamping::PublishingAfterUnlock);
    }

    // Clocks of their own each meet the jump from their own latest time: every point resets.
    #[test]
    #[should_panic(expected = "violated: ")]
    fn per_point_clocks_reset_once_per_point() {
        model(Stamping::PerPoint);
    }
}
