//! Shared helpers for Antithesis test commands.

use std::time::Duration;

pub mod config;
pub mod context;
pub mod dogstatsd;
#[cfg(unix)]
pub mod driver;
pub mod payload;
pub mod rand;

/// How long a context may take to appear on both lanes before it counts as a
/// divergence.
pub const ACCEPTABLE_FLUSH_DELAY: Duration = Duration::from_secs(30);

/// Provisional Frechet-oracle constants, baked to launch fast. Full calibration is deferred; these
/// are the launch defaults, not calibrated values.
///
/// Sakoe-Chiba band half-width, in buckets: the time-slide Band B the aligner tolerates.
pub const SAKOE_CHIBA_BAND: usize = 2;

/// The clipped signed CUSUM firing threshold `h`.
pub const CUSUM_H: f64 = 1.5;

/// The CUSUM base slack `k_base` for the sketch mean/quantile channels.
pub const CUSUM_K_BASE_SKETCH: f64 = 0.5;

/// The CUSUM base slack `k_base` for the scalar count/rate/gauge channels.
pub const CUSUM_K_BASE_SCALAR: f64 = 0.1;

/// Settle horizon before a column is compared or a presence mismatch trips: band `B` plus one flush.
pub const SETTLE_HORIZON: usize = SAKOE_CHIBA_BAND + 1;

/// The provisional per-quantile sketch merge slack `k_merge`, a small constant below `alpha`.
pub const K_MERGE: f64 = 0.05;

/// The provisional trend-aware CUSUM slack coefficient `phi_max`: how much a fast-moving reference
/// widens the per-column slack so a legitimate level transition does not accumulate a false trip.
pub const CUSUM_PHI_MAX: f64 = 1.0;
