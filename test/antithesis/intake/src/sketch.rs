//! Sketch bucket merge and the seven scalar series a sketch context projects to.

use crate::capture::SketchValue;

#[cfg(test)]
mod tests;

/// The Agent's default relative accuracy, `defaultEps` in `pkg/util/quantile/config.go:15`.
const EPS: f64 = 1.0 / 128.0;
/// `gamma.v = 1 + 2 * eps`, `config.go:138`.
const GAMMA: f64 = 1.0 + 2.0 * EPS;
/// The smallest representable value, `defaultMin` at `config.go:16`.
const MIN_VALUE: f64 = 1e-9;

/// `norm.bias = -floor(log_gamma(defaultMin)) + 1`, `config.go:151-154`. A key maps to a value by
/// `gamma^(k - bias)`, so the bias is what anchors the grid.
fn bias() -> i32 {
    // log_gamma(1e-9) is about -1335 at the Agent's gamma, so the floor fits an i32 with room to spare.
    #[allow(clippy::cast_possible_truncation)]
    let emin = (MIN_VALUE.ln() / GAMMA.ln()).floor() as i32;
    -emin + 1
}

/// The terminal key. Everything the grid cannot represent lands here and the Agent maps it to an infinity.
/// The projection clamps to the sketch's own extremes, so a terminal bin reads as `min` or `max` rather
/// than escaping as a non-finite value.
const MAX_KEY: i32 = 32_767;

/// The value a bin key stands for, or `None` for a key outside the Agent's grid, which no lane should
/// ever put on the wire.
///
/// The harmonic mean of the bin's true bounds, which is what the backend uses. The backend computes the
/// percentile in production, since the Agent ships bins rather than quantiles.
///
/// Two lanes only agree on this if they built their sketches on the same grid, which is why the
/// quantization check runs first.
pub(crate) fn key_to_value(k: i32) -> Option<f64> {
    match k {
        0 => Some(0.0),
        // `unsigned_abs`, since `i32::MIN.abs()` overflows and the release profile checks overflow, so
        // the guard meant to reject an out-of-domain key would abort on one instead.
        k if k.unsigned_abs() > MAX_KEY.unsigned_abs() => None,
        k if k.unsigned_abs() == MAX_KEY.unsigned_abs() => Some(f64::INFINITY.copysign(f64::from(k))),
        k if k < 0 => key_to_value(-k).map(|value| -value),
        k => {
            let exponent = f64::from(k - bias() + 1) - 0.5;
            Some(2.0 * GAMMA.powf(exponent) / (1.0 + GAMMA))
        }
    }
}

/// Merge a bucket's sketches. Counts and sums add, min and max take their extremes, and bins add
/// per key. Exact, so a merged bucket is what one sketch over the same points would have been.
pub(crate) fn merge(values: &[SketchValue]) -> Option<SketchValue> {
    values.first()?;
    // A zero-count sketch carries no samples, so folding its summary in fabricates them. An empty
    // accumulator adopts the next input, after which only a positive-count input contributes. Bins merge
    // either way, since a zero-count sketch can still carry retained bins.
    let mut summary: Option<SketchValue> = None;
    for value in values {
        match &mut summary {
            None => summary = Some(value.clone()),
            Some(acc) if acc.count == 0 => *acc = value.clone(),
            Some(acc) if value.count > 0 => {
                // Saturating, since the release profile checks overflow and a malformed lane must be
                // reported rather than abort the oracle and poison the capture lock.
                acc.count = acc.count.saturating_add(value.count);
                acc.sum += value.sum;
                acc.min = acc.min.min(value.min);
                acc.max = acc.max.max(value.max);
            }
            Some(_) => {}
        }
    }
    let seed = summary?;
    let mut merged = SketchValue {
        count: seed.count,
        sum: seed.sum,
        min: seed.min,
        max: seed.max,
        bins: Vec::new(),
    };
    // Bins arrive key-sorted per sketch, so a k-way merge by key keeps the result sorted.
    let mut keys: Vec<(i32, u32)> = values.iter().flat_map(|v| v.bins.iter().copied()).collect();
    keys.sort_unstable_by_key(|(k, _)| *k);
    for (k, n) in keys {
        // A saturating add would discard the excess silently and shrink the distribution. `quantile_key`
        // accumulates in `u64` and walks bins in key order, so carrying the remainder in a second entry
        // for the same key reads back as the full count.
        match merged.bins.last_mut() {
            Some((last_k, last_n)) if *last_k == k => {
                if let Some(sum) = last_n.checked_add(n) {
                    *last_n = sum;
                } else {
                    let carried = n - (u32::MAX - *last_n);
                    *last_n = u32::MAX;
                    merged.bins.push((k, carried));
                }
            }
            _ => merged.bins.push((k, n)),
        }
    }
    Some(merged)
}

/// The value at quantile `q`, walking the cumulative distribution the way the backend does.
///
/// The rank is `q * (total - 1) + 1` truncated, over the sum of the bin counts, and the answer is the first
/// bin whose cumulative count reaches it. The backend does not interpolate inside the selected bin, for
/// backward compatibility, so neither does this.
///
/// The result is clamped to the sketch's own extremes, which is what folds a terminal bin's infinity back
/// to `min` or `max`.
///
/// `None` for a sketch whose bins hold nothing.
pub(crate) fn quantile(sketch: &SketchValue, q: f64) -> Option<f64> {
    let total: u64 = sketch.bins.iter().map(|(_, n)| u64::from(*n)).sum();
    if total == 0 {
        return None;
    }
    // Truncating, as the backend does. `ceil(total * p / 100)` sat one rank high, which made p99 the top
    // populated bin for any bucket under a hundred samples, so p99 copied the max series.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let want = (q.clamp(0.0, 1.0) * ((total - 1) as f64) + 1.0) as u64;
    let mut seen = 0u64;
    let mut selected = None;
    for (k, n) in &sketch.bins {
        seen += u64::from(*n);
        if seen >= want {
            selected = Some(*k);
            break;
        }
    }
    let key = selected.or_else(|| sketch.bins.last().map(|(k, _)| *k))?;
    Some(key_to_value(key)?.clamp(sketch.min, sketch.max))
}

/// Whether two lanes built their sketches on the same grid.
///
/// Gamma is never on the wire, so it is inferred from the highest populated bin key under equal `max`. A
/// disagreement there means the lanes quantize differently, which makes every quantile comparison
/// meaningless and is a failure of equivalence in its own right.
///
/// The lowest populated key says nothing about the grid. Both lanes trim bins from the left past their bin
/// limit, folding everything below the cutoff into it while `min` keeps the true minimum, and they trim at
/// different points. Reading that as a grid mismatch reds with the wrong diagnosis.
pub(crate) fn same_quantization(a: &SketchValue, b: &SketchValue) -> bool {
    // Only comparable when the raw extremes agree. Different data legitimately lands on different
    // keys. The comparison is exact because the extremes travel the wire as the same f64 on both lanes.
    #[allow(clippy::float_cmp)]
    let extremes_differ = a.min != b.min || a.max != b.max;
    if extremes_differ {
        return true;
    }
    let highest = |s: &SketchValue| s.bins.last().map(|(k, _)| *k);
    highest(a) == highest(b)
}

/// The seven series a sketch context compares as. `None` where the sketch carries no bins, which the
/// caller reports as a skip rather than folding to zero.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Projection {
    pub(crate) count: f64,
    pub(crate) sum: f64,
    pub(crate) min: f64,
    pub(crate) max: f64,
    pub(crate) p75: Option<f64>,
    pub(crate) p95: Option<f64>,
    pub(crate) p99: Option<f64>,
}

/// Project a merged sketch onto its seven scalar series.
pub(crate) fn project(sketch: &SketchValue) -> Projection {
    let at = |q: f64| quantile(sketch, q);
    #[allow(clippy::cast_precision_loss)]
    Projection {
        count: sketch.count as f64,
        sum: sketch.sum,
        min: sketch.min,
        max: sketch.max,
        p75: at(0.75),
        p95: at(0.95),
        p99: at(0.99),
    }
}
