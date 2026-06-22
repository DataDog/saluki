//! Randomness utilities.

use std::marker::PhantomData;

use rand::distr::Distribution;
use rand::{Rng, RngExt};
use rand_distr::LogNormal;

// ===========================================================================
// Probe — a boundary-biased magnitude sampler. ~1/8 of draws are a boundary
// value, the rest a typical log-normal magnitude.
// ===========================================================================

/// `u64` boundary values: 0, 1, and each fixed-width max ±1.
const BOUNDARIES_U64: &[u64] = &[
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

/// `i64` boundary values: 0, ±1, and each signed-width min/max.
const BOUNDARIES_I64: &[i64] = &[
    i64::MIN,
    i64::MIN + 1,
    i32::MIN as i64,
    i16::MIN as i64,
    i8::MIN as i64,
    -1,
    0,
    1,
    i8::MAX as i64,
    i16::MAX as i64,
    i32::MAX as i64,
    i64::MAX - 1,
    i64::MAX,
];

/// `f64` boundary values (no NaN/inf — those break frame parsing and belong to a
/// dedicated malformed-input driver).
const BOUNDARIES_F64: &[f64] = &[
    0.0,
    1.0,
    -1.0,
    f64::MIN_POSITIVE,
    -f64::MIN_POSITIVE,
    f64::MAX,
    f64::MIN,
];

/// A boundary-biased distribution: ~1/8 of draws are a boundary value, the rest a
/// "typical" log-normal magnitude. Generic over the numeric output type so a draw
/// site reads `let v: i64 = Probe.sample(rng)` and gets type-appropriate
/// boundaries. `i64`/`f64` draws carry a random sign.
#[derive(Debug, Clone, Copy)]
pub struct Probe;

impl Distribution<u64> for Probe {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> u64 {
        if rng.random_ratio(1, 8) {
            BOUNDARIES_U64[rng.random_range(0..BOUNDARIES_U64.len())]
        } else {
            typical(rng)
        }
    }
}

impl Distribution<i64> for Probe {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> i64 {
        if rng.random_ratio(1, 8) {
            BOUNDARIES_I64[rng.random_range(0..BOUNDARIES_I64.len())]
        } else {
            let magnitude = num_traits::cast::<u64, i64>(typical(rng)).unwrap_or(i64::MAX);
            if rng.random_ratio(1, 2) {
                -magnitude
            } else {
                magnitude
            }
        }
    }
}

impl Distribution<f64> for Probe {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> f64 {
        if rng.random_ratio(1, 8) {
            BOUNDARIES_F64[rng.random_range(0..BOUNDARIES_F64.len())]
        } else {
            let magnitude = num_traits::cast::<u64, f64>(typical(rng)).unwrap_or(f64::MAX);
            if rng.random_ratio(1, 2) {
                -magnitude
            } else {
                magnitude
            }
        }
    }
}

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

// ===========================================================================
// Wide — a log-uniform magnitude sampler with a random sign and a boundary tail.
// Draws spread evenly across orders of magnitude so values land in distinct
// buckets, and ~1/8 of draws are a type boundary so the extremes still appear.
// The `f64` body spans 1e-30..1e30, the `i64` body spans 1..1e18, and the tail
// reaches f64::MAX / i64::MAX, which is what overflows a sketch sum or count.
// ===========================================================================

/// A log-uniform, random-sign magnitude sampler with a boundary tail. See the
/// section note above.
#[derive(Debug, Clone, Copy)]
pub struct Wide;

impl Distribution<f64> for Wide {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> f64 {
        if rng.random_ratio(1, 8) {
            return BOUNDARIES_F64[rng.random_range(0..BOUNDARIES_F64.len())];
        }
        let mag = 10f64.powf(rng.random_range(-30.0..30.0));
        if rng.random_range(0..2u8) == 0 {
            -mag
        } else {
            mag
        }
    }
}

impl Distribution<i64> for Wide {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> i64 {
        if rng.random_ratio(1, 8) {
            return BOUNDARIES_I64[rng.random_range(0..BOUNDARIES_I64.len())];
        }
        let mag = num_traits::cast::<f64, i64>(10f64.powf(rng.random_range(0.0..18.0))).unwrap_or(i64::MAX);
        if rng.random_range(0..2u8) == 0 {
            -mag
        } else {
            mag
        }
    }
}

// ===========================================================================
// Boundary<T> — a finite type-boundary sampler: each fixed-width max ±1 and the
// half-range midpoint ±1, the same idea as Probe's arrays but for one type.
// ===========================================================================

/// A boundary-value sampler for `T`: each fixed-width max ±1 and the half-range
/// midpoint ±1. `Boundary::<T>::new().sample(rng)` returns one.
#[derive(Clone, Copy, Debug, Default)]
pub struct Boundary<T>(PhantomData<T>);

impl<T> Boundary<T> {
    /// A boundary sampler for `T`.
    #[must_use]
    pub const fn new() -> Self {
        Boundary(PhantomData)
    }
}

const BOUNDARY_U8: &[u8] = &[0, 1, 2, 126, 127, 128, 129, 254, 255];

impl Distribution<u8> for Boundary<u8> {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> u8 {
        BOUNDARY_U8[rng.random_range(0..BOUNDARY_U8.len())]
    }
}

const BOUNDARY_U64: &[u64] = &[
    0,
    1,
    2,
    u8::MAX as u64 - 1,
    u8::MAX as u64,
    u8::MAX as u64 + 1,
    u16::MAX as u64 - 1,
    u16::MAX as u64,
    u16::MAX as u64 + 1,
    u32::MAX as u64 - 1,
    u32::MAX as u64,
    u32::MAX as u64 + 1,
    u64::MAX / 2 - 1,
    u64::MAX / 2,
    u64::MAX / 2 + 1,
    u64::MAX - 1,
    u64::MAX,
];

impl Distribution<u64> for Boundary<u64> {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> u64 {
        BOUNDARY_U64[rng.random_range(0..BOUNDARY_U64.len())]
    }
}

const BOUNDARY_I64: &[i64] = &[
    i64::MIN,
    i64::MIN + 1,
    i64::MIN / 2 - 1,
    i64::MIN / 2,
    i64::MIN / 2 + 1,
    i32::MIN as i64,
    i16::MIN as i64,
    i8::MIN as i64,
    -1,
    0,
    1,
    i8::MAX as i64,
    i16::MAX as i64,
    i32::MAX as i64,
    i64::MAX / 2 - 1,
    i64::MAX / 2,
    i64::MAX / 2 + 1,
    i64::MAX - 1,
    i64::MAX,
];

impl Distribution<i64> for Boundary<i64> {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> i64 {
        BOUNDARY_I64[rng.random_range(0..BOUNDARY_I64.len())]
    }
}
