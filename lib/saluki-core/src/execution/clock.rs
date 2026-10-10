//! Admission clocks.

use std::{
    fmt,
    sync::Arc,
    time::{Duration, SystemTime},
};

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
        self.latest = Some(admitted_at);

        ClockStamp {
            admitted_at,
            epoch: self.epoch,
            reset,
        }
    }
}

#[cfg(test)]
mod tests {
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
    fn the_system_wall_clock_reads_the_current_time() {
        let before = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap();
        let reading = SystemWallClock.now();
        let after = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap();
        assert!(u128::from(reading.as_unix_nanos()) >= before.as_nanos());
        assert!(u128::from(reading.as_unix_nanos()) <= after.as_nanos());
    }
}
