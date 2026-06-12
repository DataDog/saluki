//! Time-related functions.

use std::{
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Once, OnceLock,
    },
    thread,
    time::{Duration, SystemTime},
};

static COARSE_TIME_INITIALIZED: Once = Once::new();
static COARSE_TIME: AtomicU64 = AtomicU64::new(0);
const COARSE_TIME_UPDATE_INTERVAL: Duration = Duration::from_millis(250);

/// Anchors a tokio-driven unix clock the first time it is needed inside a tokio runtime.
///
/// The pair is `(wall_unix_secs_at_anchor, tokio_instant_at_anchor)`. From there, the unix
/// timestamp is computed as `wall_unix_secs_at_anchor + (tokio::time::Instant::now() - tokio_instant_at_anchor)`,
/// which has two useful properties:
///
/// 1. **Monotonic in production.** `tokio::time::Instant` is monotonic, so the derived unix
///    timestamp can never jump backward across NTP adjustments — a desirable property for any
///    bucket-expiration or rate-limit logic that calls into this module.
/// 2. **Pauses with the runtime in tests.** When the runtime is built with
///    `tokio::runtime::Builder::start_paused(true)`, `tokio::time::Instant` follows simulated
///    time, so the unix timestamp returned from inside the runtime advances in lockstep with
///    `tokio::time` auto-advance. Time-sensitive logic such as aggregation bucket expiration
///    therefore stays aligned with simulated time without any test-side wiring.
static TOKIO_UNIX_ANCHOR: OnceLock<(u64, tokio::time::Instant)> = OnceLock::new();

/// Get the current Unix timestamp, in seconds.
///
/// When called from inside a tokio runtime, the timestamp is derived from
/// [`tokio::time::Instant`] anchored on the wall clock at first use. This makes the result
/// monotonic in production and aligned with simulated time when the runtime is paused (see
/// [`tokio::runtime::Builder::start_paused`]).
///
/// When called outside a tokio runtime (for example, from `std::thread::spawn`-ed background
/// threads or process startup before the runtime is built), the timestamp falls back to
/// [`SystemTime::now`].
///
/// In scenarios where this function is called very frequently, consider
/// [`get_coarse_unix_timestamp`], which caches the value and updates it periodically.
pub fn get_unix_timestamp() -> u64 {
    if tokio::runtime::Handle::try_current().is_ok() {
        let (wall_anchor_secs, tokio_anchor) = *TOKIO_UNIX_ANCHOR.get_or_init(|| {
            let wall_secs = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            (wall_secs, tokio::time::Instant::now())
        });
        return wall_anchor_secs + tokio::time::Instant::now().duration_since(tokio_anchor).as_secs();
    }

    // Fallback for callers without a tokio runtime context (process startup, std::thread
    // background workers, plain `#[test]` units, etc.).
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Get the current coarse Unix timestamp, in seconds.
///
/// In scenarios where the current Unix timestamp is needed frequently, this function provides a cached value that is
/// updated periodically. As the precision of the timestamp is one second, this function allows trading off accuracy
/// (see below) for reduced overhead. The resulting value is considered "coarse", because it might be off by significant
/// percentage of the overall precision.
///
/// # Time source
///
/// The cache is refreshed by a `std::thread::spawn`-ed background loop, which has no tokio
/// runtime context. It therefore always reads [`SystemTime::now`] (the fallback path of
/// [`get_unix_timestamp`]), even when callers from inside a runtime would otherwise see a
/// tokio-anchored value. The two sources agree to within a few milliseconds in production; the
/// only place this matters is integration tests using `tokio::time::pause()`, where the coarse
/// value will track real wall clock instead of simulated time.
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
