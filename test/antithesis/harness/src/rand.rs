//! Randomness utilities.

use rand::distr::Distribution;
use rand::seq::IndexedRandom as _;
use rand::{Rng, RngExt};
use rand_distr::LogNormal;

/// Boundary values for the u64 field.
const BOUNDARIES: &[u64] = &[
    0,
    1,
    i8::MAX as u64 - 1,
    i8::MAX as u64,
    i8::MAX as u64 + 1,
    u8::MAX as u64 - 1,
    u8::MAX as u64,
    u8::MAX as u64 + 1,
    i16::MAX as u64 - 1,
    i16::MAX as u64,
    i16::MAX as u64 + 1,
    u16::MAX as u64 - 1,
    u16::MAX as u64,
    u16::MAX as u64 + 1,
    i32::MAX as u64 - 1,
    i32::MAX as u64,
    i32::MAX as u64 + 1,
    u32::MAX as u64 - 1,
    u32::MAX as u64,
    u32::MAX as u64 + 1,
    i64::MAX as u64 - 1,
    i64::MAX as u64,
    i64::MAX as u64 + 1,
    u64::MAX - 1,
    u64::MAX,
];

/// Produces `u64` values that are generally 'normal' and with some being
/// boundary values.
#[derive(Debug, Clone, Copy)]
pub struct Probe;

impl Distribution<u64> for Probe {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> u64 {
        if rng.random_ratio(1, 8) {
            BOUNDARIES.choose(rng).copied().unwrap_or(0)
        } else {
            typical(rng)
        }
    }
}

/// Typical draw: a [`LogNormal`] (median `1024`, sigma `4`) — a hump near the
/// median with a heavy tail spanning many orders of magnitude. The float draw is
/// rounded to the nearest integer (standard library); a value beyond `u64`
/// saturates to `u64::MAX`. [`num_traits::cast`] is range-checked, so the
/// conversion is not a lossy `as`.
///
/// Approximate probability of a typical draw landing in each range:
///
/// | Value range            | Probability |
/// |------------------------|-------------|
/// | `<= 16`                | ~15%        |
/// | `16 ..= 256`           | ~21%        |
/// | `256 ..= 1_024`        | ~14%        |
/// | `1_024 ..= 4_096`      | ~14%        |
/// | `4_096 ..= 65_536`     | ~22%        |
/// | `65_536 ..= 1_048_576` | ~11%        |
/// | `> 1_048_576`          | ~4%         |
fn typical<R: Rng + ?Sized>(rng: &mut R) -> u64 {
    let dist = LogNormal::new(1024.0_f64.ln(), 4.0).expect("median > 0 and sigma >= 0");
    num_traits::cast::<f64, u64>(dist.sample(rng).round()).unwrap_or(u64::MAX)
}
