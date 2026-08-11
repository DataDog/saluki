//! Time-related functions.

use std::{
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Once,
    },
    thread,
    time::{Duration, SystemTime},
};

static COARSE_TIME_INITIALIZED: Once = Once::new();
static COARSE_TIME: AtomicU64 = AtomicU64::new(0);
const COARSE_TIME_UPDATE_INTERVAL: Duration = Duration::from_millis(250);

/// Get the current Unix timestamp, in seconds.
///
/// This function is accurate, as it always retrieves the current time for each call. In scenarios where this function
/// is being called frequently, it may pose an unacceptable performance overhead. In such cases, consider using
/// `get_coarse_unix_timestamp`, which provides a cached value that's updated periodically.
pub fn get_unix_timestamp() -> u64 {
    let since_unix_epoch = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    since_unix_epoch.as_secs()
}

/// Get the current coarse Unix timestamp, in seconds.
///
/// In scenarios where the current Unix timestamp is needed frequently, this function provides a cached value that's
/// updated periodically. As the precision of the timestamp is one second, this function allows trading off accuracy
/// (see below) for reduced overhead. The resulting value is considered "coarse," because it might be off by significant
/// percentage of the overall precision.
///
/// # Accuracy
///
/// The underlying coarse time is updated roughly every 250 milliseconds, so the value returned by this function may be
/// behind by up to 250 milliseconds. This means that if calling the function at true time `t` (where `t` is in
/// seconds), the value returned may be `t-1` _or_ `t`, but will never be behind by more than 250 milliseconds, and
/// never _ahead_ of `t`.
pub fn get_coarse_unix_timestamp() -> u64 {
    // Initialize a background thread to update the coarse time if it hasn't been initialized yet.
    COARSE_TIME_INITIALIZED.call_once(|| {
        // Initialize the coarse time with the current Unix timestamp.
        COARSE_TIME.store(get_unix_timestamp(), Relaxed);

        thread::spawn(|| {
            loop {
                // Sleep for 250 milliseconds.
                thread::sleep(COARSE_TIME_UPDATE_INTERVAL);

                // Update the coarse time with the current Unix timestamp.
                COARSE_TIME.store(get_unix_timestamp(), Relaxed);
            }
        });
    });

    COARSE_TIME.load(Relaxed)
}

#[cfg(test)]
mod tests {
    use std::{thread::sleep, time::Duration};

    use super::*;

    #[test]
    fn coarse_timestamp_never_exceeds_accurate_timestamp() {
        // The documented contract is that the coarse timestamp is never _ahead_ of the true time:
        // it is only ever equal to it or lagging behind it, because it is a cached snapshot of a
        // previous `get_unix_timestamp()` call. Sample repeatedly to make an accidental "ahead"
        // read unlikely to slip through.
        for _ in 0..1_000 {
            let coarse = get_coarse_unix_timestamp();
            let accurate = get_unix_timestamp();
            assert!(
                coarse <= accurate,
                "coarse timestamp {coarse} must never be ahead of accurate timestamp {accurate}"
            );
        }
    }

    #[test]
    fn coarse_timestamp_is_monotonically_non_decreasing() {
        // The coarse timestamp is only ever replaced with a newer `get_unix_timestamp()` value, so
        // successive reads must never go backwards, including across an update-interval boundary.
        let first = get_coarse_unix_timestamp();

        let mut previous = first;
        for _ in 0..1_000 {
            let current = get_coarse_unix_timestamp();
            assert!(
                current >= previous,
                "coarse timestamp went backwards: {previous} -> {current}"
            );
            previous = current;
        }

        // Sleep past a couple of update intervals so the background updater runs, then confirm the
        // value has not regressed.
        sleep(COARSE_TIME_UPDATE_INTERVAL * 3);
        let after_sleep = get_coarse_unix_timestamp();
        assert!(
            after_sleep >= previous,
            "coarse timestamp regressed after update: {previous} -> {after_sleep}"
        );
    }

    #[test]
    fn coarse_timestamp_tracks_accurate_within_staleness_bound() {
        // The documented staleness bound is that the coarse value may lag true time by at most
        // ~250ms, i.e. in whole seconds it is either `t` or `t-1`. Once the background updater is
        // running, the difference against the accurate timestamp should therefore be no more than
        // one second. Retry across a short window to absorb one-off scheduling jitter on busy CI.
        get_coarse_unix_timestamp();
        sleep(COARSE_TIME_UPDATE_INTERVAL * 2);

        let mut within_bound = false;
        for _ in 0..20 {
            let accurate = get_unix_timestamp();
            let coarse = get_coarse_unix_timestamp();
            if accurate - coarse <= 1 {
                within_bound = true;
                break;
            }
            sleep(Duration::from_millis(100));
        }

        assert!(
            within_bound,
            "coarse timestamp never came within one second of the accurate timestamp"
        );
    }
}
