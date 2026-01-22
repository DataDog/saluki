//! Agent-specific DDSketch configuration.

#[allow(dead_code)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/agent_config.rs"));
}

pub(crate) use generated::*;

const MAX_KEY: i16 = i16::MAX;

#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct Config {
    // Maximum number of bins per sketch.
    pub(crate) bin_limit: u16,

    // gamma_ln is the natural log of gamma_v, used to speed up calculating log base gamma.
    pub(crate) gamma_v: f64,
    pub(crate) gamma_ln: f64,

    // Minimum and maximum values representable by a sketch with these params.
    //
    // key(x) =
    //    0 : -min > x < min
    //    1 : x == min
    //   -1 : x == -min
    // +Inf : x > max
    // -Inf : x < -max.
    pub(crate) norm_min: f64,

    // Bias of the exponent, used to ensure key(x) >= 1.
    pub(crate) norm_bias: i32,
}

impl Config {
    pub(crate) const fn new(bin_limit: u16, gamma_v: f64, gamma_ln: f64, norm_min: f64, norm_bias: i32) -> Self {
        Self {
            bin_limit,
            gamma_v,
            gamma_ln,
            norm_min,
            norm_bias,
        }
    }

    /// Gets the value lower bound of the bin at the given key.
    #[inline]
    pub(crate) fn bin_lower_bound(&self, k: i16) -> f64 {
        if k < 0 {
            return -self.bin_lower_bound(-k);
        }

        if k == MAX_KEY {
            return f64::INFINITY;
        }

        if k == 0 {
            return 0.0;
        }

        self.gamma_v.powf(f64::from(i32::from(k) - self.norm_bias))
    }

    /// Gets the key for the given value.
    ///
    /// The key corresponds to the bin where this value would be represented. The value returned here is such that: γ^k
    /// <= v < γ^(k+1).
    #[allow(clippy::cast_possible_truncation)]
    #[inline]
    pub(crate) fn key(&self, v: f64) -> i16 {
        if v < 0.0 {
            return -self.key(-v);
        }

        if v == 0.0 || (v > 0.0 && v < self.norm_min) || (v < 0.0 && v > -self.norm_min) {
            return 0;
        }

        // SAFETY: `rounded` is intentionally meant to be a whole integer, and additionally, based on our target gamma
        // ln, we expect `log_gamma` to return a value between -2^16 and 2^16, so it will always fit in an i32.
        let rounded = self.log_gamma(v).round_ties_even() as i32;
        let key = rounded.wrapping_add(self.norm_bias);

        // SAFETY: Our upper bound of POS_INF_KEY is i16, and our lower bound is simply one, so there is no risk of
        // truncation via conversion.
        key.clamp(1, i32::from(MAX_KEY)) as i16
    }

    #[inline]
    pub(crate) fn log_gamma(&self, v: f64) -> f64 {
        v.ln() / self.gamma_ln
    }
}
