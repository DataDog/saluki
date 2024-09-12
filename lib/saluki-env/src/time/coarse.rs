use std::{
	sync::atomic::{AtomicBool, AtomicU64, Ordering::{AcqRel, Relaxed}}, thread, time::{Duration, SystemTime}
};

use super::TimeProvider;

static CACHED_TIME: AtomicU64 = AtomicU64::new(0);
static CACHED_TIME_THREAD_SPAWNED: AtomicBool = AtomicBool::new(false);

/// A time provider based on coarse timestamps that are cached in-process.
///
/// In many cases, while we may require second or millisecond granularity, having those timestamps be accurate to the
/// millisecond, or nanosecond, is not required. We can trade-off a decrease in accuracy by a few hundred milliseconds
/// (so long as this is bounded and known) for a significant reduction in overhead for querying the time itself.
///
/// This provider takes "coarse" timestamps -- accurate to around 100ms of the system clock -- and caches them through a
/// shared atomic, which allows for ultra-low overhead reads of the current time, on the order of a few cycle.
pub struct CachedCoarseTimeProvider {
	_init: (),
}

impl CachedCoarseTimeProvider {
	/// Creates a new `CachedCoarseTimeProvider`.
	///
	/// If this is the first time this method is called, a background thread will be spawned that updates the 
	pub fn new() -> Self {
		// We force callers to create this time provider through this method, which we use as a spot to ensure the
		// background thread that updates the coarse time is spawned.
		if !CACHED_TIME_THREAD_SPAWNED.swap(true, AcqRel) {
			// We won the swap, which means we are responsible for spawning the background thread.
			let _ = thread::Builder::new()
				.name("saluki-coarse-time-updater".to_string())
				.spawn(|| {
					loop {
						// Get the current system time and store it.
						let current_time = SystemTime::now()
							.duration_since(SystemTime::UNIX_EPOCH)
							.unwrap_or_default()
							.as_secs();
						CACHED_TIME.store(current_time, Relaxed);

						// Sleep for 100ms. Our granularity is a single second, so we _could_ just do this once a
						// second, but our sleep interval also becomes the upper bound for how stale the cached time is,
						// so we don't want to be stale by a second, but updating it every 1ms would be wasteful... so
						// this is a compromise.
						thread::sleep(Duration::from_millis(100));
					}
				});
		}

		Self { _init: () }
	}
}

impl TimeProvider for CachedCoarseTimeProvider {
	fn get_unix_timestamp(&self) -> u64 {
		get_cached_unix_timestamp()
	}
}

#[inline(always)]
fn get_cached_unix_timestamp() -> u64 {
	CACHED_TIME.load(Relaxed)
}
