//! Randomness utilities.

use std::marker::PhantomData;

use rand::distr::Distribution;
use rand::{Rng, RngExt};

// ===========================================================================
// Probe — a range-bounded, boundary-biased u64 sampler.
// ===========================================================================

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

/// A range-bounded, boundary-biased `u64` sampler: about one draw in four a
/// range edge, the rest log-uniform over the non-zero interior, so `0` appears
/// only as an edge.
#[derive(Debug, Clone, Copy)]
pub struct Probe {
    min: u64,
    max: u64,
}

impl Probe {
    /// Creates a sampler over `[min, max]`.
    ///
    /// # Panics
    ///
    /// Panics if `min` exceeds `max`.
    #[must_use]
    pub const fn new(min: u64, max: u64) -> Self {
        assert!(min <= max, "Probe min must not exceed max");
        Self { min, max }
    }
}

impl Distribution<u64> for Probe {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> u64 {
        if rng.random_ratio(1, 4) {
            if rng.random_ratio(1, 2) {
                self.min
            } else {
                self.max
            }
        } else {
            // Log-uniform so the interior is covered evenly across magnitudes.
            let lo = num_traits::cast::<u64, f64>(self.min.max(1)).unwrap_or(1.0).ln();
            let hi = num_traits::cast::<u64, f64>(self.max.max(1)).unwrap_or(1.0).ln();
            let exponent = lo + rng.random::<f64>() * (hi - lo);
            num_traits::cast::<f64, u64>(exponent.exp().round())
                .unwrap_or(self.max)
                .clamp(self.min, self.max)
        }
    }
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
// half-range midpoint ±1, the same idea as the boundary tables above but for one type.
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
