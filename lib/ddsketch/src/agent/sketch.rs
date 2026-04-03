//! Agent-specific DDSketch implementation.

use std::{cmp::Ordering, mem};

use datadog_protos::metrics::Dogsketch;
use ordered_float::OrderedFloat;
use smallvec::SmallVec;

use super::bin::Bin;
use super::bucket::Bucket;
use super::config::{
    Config, DDSKETCH_CONF_BIN_LIMIT, DDSKETCH_CONF_GAMMA_LN, DDSKETCH_CONF_GAMMA_V, DDSKETCH_CONF_NORM_BIAS,
    DDSKETCH_CONF_NORM_MIN,
};
use crate::common::float_eq;

static SKETCH_CONFIG: Config = Config::new(
    DDSKETCH_CONF_BIN_LIMIT,
    DDSKETCH_CONF_GAMMA_V,
    DDSKETCH_CONF_GAMMA_LN,
    DDSKETCH_CONF_NORM_MIN,
    DDSKETCH_CONF_NORM_BIAS,
);
const MAX_BIN_WIDTH: u32 = u32::MAX;

/// [DDSketch][ddsketch] implementation based on the [Datadog Agent][ddagent].
///
/// This implementation is subtly different from the open-source implementations of `DDSketch`, as Datadog made some
/// slight tweaks to configuration values and in-memory layout to optimize it for insertion performance within the
/// agent.
///
/// We've mimicked the agent version of `DDSketch` here in order to support a future where we can take sketches shipped
/// by the agent, handle them internally, merge them, and so on, without any loss of accuracy, eventually forwarding
/// them to Datadog ourselves.
///
/// As such, this implementation is constrained in the same ways: the configuration parameters cannot be changed, the
/// collapsing strategy is fixed, and we support a limited number of methods for inserting into the sketch.
///
/// Importantly, we have a special function, again taken from the agent version, to allow us to interpolate histograms,
/// specifically our own aggregated histograms, into a sketch so that we can emit useful default quantiles, rather than
/// having to ship the buckets -- upper bound and count -- to a downstream system that might have no native way to do
/// the same thing, basically providing no value as they have no way to render useful data from them.
///
/// # Features
///
/// This crate exposes a single feature, `serde`, which enables serialization and deserialization of `DDSketch` with
/// `serde`. This feature is not enabled by default, as it can be slightly risky to use. This is primarily due to the
/// fact that the format of `DDSketch` is not promised to be stable over time. If you enable this feature, you should
/// take care to avoid storing serialized `DDSketch` data for long periods of time, as deserializing it in the future
/// may work but could lead to incorrect/unexpected behavior or issues with correctness.
///
/// [ddsketch]: https://www.vldb.org/pvldb/vol12/p2195-masson.pdf
/// [ddagent]: https://github.com/DataDog/datadog-agent
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct DDSketch {
    /// The bins within the sketch.
    bins: SmallVec<[Bin; 4]>,

    /// The number of observations within the sketch.
    count: u64,

    /// The minimum value of all observations within the sketch.
    min: f64,

    /// The maximum value of all observations within the sketch.
    max: f64,

    /// The sum of all observations within the sketch.
    sum: f64,

    /// The average value of all observations within the sketch.
    avg: f64,
}

impl DDSketch {
    /// Returns the number of bins in the sketch.
    pub fn bin_count(&self) -> usize {
        self.bins.len()
    }

    /// Whether or not this sketch is empty.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Number of samples currently represented by this sketch.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Minimum value seen by this sketch.
    ///
    /// Returns `None` if the sketch is empty.
    pub fn min(&self) -> Option<f64> {
        if self.is_empty() {
            None
        } else {
            Some(self.min)
        }
    }

    /// Maximum value seen by this sketch.
    ///
    /// Returns `None` if the sketch is empty.
    pub fn max(&self) -> Option<f64> {
        if self.is_empty() {
            None
        } else {
            Some(self.max)
        }
    }

    /// Sum of all values seen by this sketch.
    ///
    /// Returns `None` if the sketch is empty.
    pub fn sum(&self) -> Option<f64> {
        if self.is_empty() {
            None
        } else {
            Some(self.sum)
        }
    }

    /// Average value seen by this sketch.
    ///
    /// Returns `None` if the sketch is empty.
    pub fn avg(&self) -> Option<f64> {
        if self.is_empty() {
            None
        } else {
            Some(self.avg)
        }
    }

    /// Returns the current bins of this sketch.
    pub fn bins(&self) -> &[Bin] {
        &self.bins
    }

    /// Clears the sketch, removing all bins and resetting all statistics.
    pub fn clear(&mut self) {
        self.count = 0;
        self.min = f64::MAX;
        self.max = f64::MIN;
        self.avg = 0.0;
        self.sum = 0.0;
        self.bins.clear();
    }

    fn adjust_basic_stats(&mut self, v: f64, n: u64) {
        if v < self.min {
            self.min = v;
        }

        if v > self.max {
            self.max = v;
        }

        self.count += n;
        self.sum += v * n as f64;

        if n == 1 {
            self.avg += (v - self.avg) / self.count as f64;
        } else {
            // TODO: From the Agent source code, this method apparently loses precision when the
            // two averages -- v and self.avg -- are close.  Is there a better approach?
            self.avg = self.avg + (v - self.avg) * n as f64 / self.count as f64;
        }
    }

    fn insert_key_counts(&mut self, counts: &[(i16, u64)]) {
        let mut temp = SmallVec::<[Bin; 4]>::new();

        let mut bins_idx = 0;
        let mut key_idx = 0;
        let bins_len = self.bins.len();
        let counts_len = counts.len();

        // PERF TODO: there's probably a fast path to be had where could check if all if the counts have existing bins
        // that aren't yet full, and we just update them directly, although we'd still be doing a linear scan to find
        // them since keys aren't 1:1 with their position in `self.bins` but using this method just to update one or two
        // bins is clearly suboptimal and we wouldn't really want to scan them all just to have to back out and actually
        // do the non-fast path.. maybe a first pass could be checking if the first/last key falls within our known
        // min/max key, and if it doesn't, then we know we have to go through the non-fast path, and if it passes, we do
        // the scan to see if we can just update bins directly?
        while bins_idx < bins_len && key_idx < counts_len {
            let bin = self.bins[bins_idx];
            let vk = counts[key_idx].0;
            let kn = counts[key_idx].1;

            match bin.k.cmp(&vk) {
                Ordering::Greater => {
                    generate_bins(&mut temp, vk, kn);
                    key_idx += 1;
                }
                Ordering::Less => {
                    temp.push(bin);
                    bins_idx += 1;
                }
                Ordering::Equal => {
                    generate_bins(&mut temp, bin.k, u64::from(bin.n) + kn);
                    bins_idx += 1;
                    key_idx += 1;
                }
            }
        }

        temp.extend_from_slice(&self.bins[bins_idx..]);

        while key_idx < counts_len {
            let vk = counts[key_idx].0;
            let kn = counts[key_idx].1;
            generate_bins(&mut temp, vk, kn);
            key_idx += 1;
        }

        trim_left(&mut temp, SKETCH_CONFIG.bin_limit);

        // PERF TODO: This is where we might do a mem::swap instead so that we could shove the bin vector into an object
        // pool but I'm not sure this actually matters at the moment.
        self.bins = temp;
    }

    fn insert_keys(&mut self, mut keys: Vec<i16>) {
        // Updating more than 4 billion keys would be very very weird and likely indicative of something horribly
        // broken.
        //
        // TODO: I don't actually understand why I wrote this assertion in this way. Either the code can handle
        // collapsing values in order to maintain the relative error bounds, or we have to cap it to the maximum allowed
        // number of bins. Gotta think about this some more.
        assert!(keys.len() <= u32::MAX.try_into().expect("we don't support 16-bit systems"));

        keys.sort_unstable();

        let mut temp = SmallVec::<[Bin; 4]>::new();

        let mut bins_idx = 0;
        let mut key_idx = 0;
        let bins_len = self.bins.len();
        let keys_len = keys.len();

        // PERF TODO: there's probably a fast path to be had where could check if all if the counts have existing bins
        // that aren't yet full, and we just update them directly, although we'd still be doing a linear scan to find
        // them since keys aren't 1:1 with their position in `self.bins` but using this method just to update one or two
        // bins is clearly suboptimal and we wouldn't really want to scan them all just to have to back out and actually
        // do the non-fast path.. maybe a first pass could be checking if the first/last key falls within our known
        // min/max key, and if it doesn't, then we know we have to go through the non-fast path, and if it passes, we do
        // the scan to see if we can just update bins directly?
        while bins_idx < bins_len && key_idx < keys_len {
            let bin = self.bins[bins_idx];
            let vk = keys[key_idx];

            match bin.k.cmp(&vk) {
                Ordering::Greater => {
                    let kn = buf_count_leading_equal(&keys, key_idx);
                    generate_bins(&mut temp, vk, kn);
                    key_idx += kn as usize;
                }
                Ordering::Less => {
                    temp.push(bin);
                    bins_idx += 1;
                }
                Ordering::Equal => {
                    let kn = buf_count_leading_equal(&keys, key_idx);
                    generate_bins(&mut temp, bin.k, u64::from(bin.n) + kn);
                    bins_idx += 1;
                    key_idx += kn as usize;
                }
            }
        }

        temp.extend_from_slice(&self.bins[bins_idx..]);

        while key_idx < keys_len {
            let vk = keys[key_idx];
            let kn = buf_count_leading_equal(&keys, key_idx);
            generate_bins(&mut temp, vk, kn);
            key_idx += kn as usize;
        }

        trim_left(&mut temp, SKETCH_CONFIG.bin_limit);

        // PERF TODO: This is where we might do a mem::swap instead so that we could shove the bin vector into an object
        // pool but I'm not sure this actually matters at the moment.
        self.bins = temp;
    }

    /// Inserts a single value into the sketch.
    pub fn insert(&mut self, v: f64) {
        // TODO: This should return a result that makes sure we have enough room to actually add 1 more sample without
        // hitting `self.config.max_count()`
        self.adjust_basic_stats(v, 1);

        let key = SKETCH_CONFIG.key(v);

        let mut insert_at = None;

        for (bin_idx, b) in self.bins.iter_mut().enumerate() {
            if b.k == key {
                if b.n < MAX_BIN_WIDTH {
                    // Fast path for adding to an existing bin without overflow.
                    b.n += 1;
                    return;
                } else {
                    insert_at = Some(bin_idx);
                    break;
                }
            }
            if b.k > key {
                insert_at = Some(bin_idx);
                break;
            }
        }

        if let Some(bin_idx) = insert_at {
            self.bins.insert(bin_idx, Bin { k: key, n: 1 });
        } else {
            self.bins.push(Bin { k: key, n: 1 });
        }
        trim_left(&mut self.bins, SKETCH_CONFIG.bin_limit);
    }

    /// Inserts many values into the sketch.
    pub fn insert_many(&mut self, vs: &[f64]) {
        // TODO: This should return a result that makes sure we have enough room to actually add N more samples without
        // hitting `self.config.bin_limit`.
        let mut keys = Vec::with_capacity(vs.len());
        for v in vs {
            self.adjust_basic_stats(*v, 1);
            keys.push(SKETCH_CONFIG.key(*v));
        }
        self.insert_keys(keys);
    }

    /// Inserts a single value into the sketch `n` times.
    pub fn insert_n(&mut self, v: f64, n: u64) {
        // TODO: This should return a result that makes sure we have enough room to actually add N more samples without
        // hitting `self.config.max_count()`.
        if n == 1 {
            self.insert(v);
        } else {
            self.adjust_basic_stats(v, n);

            let key = SKETCH_CONFIG.key(v);
            self.insert_key_counts(&[(key, n)]);
        }
    }

    fn insert_interpolate_bucket(&mut self, lower: f64, upper: f64, count: u64) {
        // Find the keys for the bins where the lower bound and upper bound would end up, and collect all of the keys in
        // between, inclusive.
        let lower_key = SKETCH_CONFIG.key(lower);
        let upper_key = SKETCH_CONFIG.key(upper);
        let keys = (lower_key..=upper_key).collect::<Vec<_>>();

        let mut key_counts = Vec::new();
        let mut remaining_count = count;
        let distance = upper - lower;
        let mut start_idx = 0;
        let mut end_idx = 1;
        let mut lower_bound = SKETCH_CONFIG.bin_lower_bound(keys[start_idx]);
        let mut remainder = 0.0;

        while end_idx < keys.len() && remaining_count > 0 {
            // For each key, map the total distance between the input lower/upper bound against the sketch lower/upper
            // bound for the current sketch bin, which tells us how much of the input count to apply to the current
            // sketch bin.
            let upper_bound = SKETCH_CONFIG.bin_lower_bound(keys[end_idx]);
            let fkn = ((upper_bound - lower_bound) / distance) * count as f64;
            if fkn > 1.0 {
                remainder += fkn - fkn.trunc();
            }

            // SAFETY: This integer cast is intentional: we want to get the non-fractional part, as we've captured the
            // fractional part in the above conditional.
            #[allow(clippy::cast_possible_truncation)]
            let mut kn = fkn as u64;
            if remainder > 1.0 {
                kn += 1;
                remainder -= 1.0;
            }

            if kn > 0 {
                if kn > remaining_count {
                    kn = remaining_count;
                }

                self.adjust_basic_stats(lower_bound, kn);
                key_counts.push((keys[start_idx], kn));

                remaining_count -= kn;
                start_idx = end_idx;
                lower_bound = upper_bound;
            }

            end_idx += 1;
        }

        if remaining_count > 0 {
            let last_key = keys[start_idx];
            lower_bound = SKETCH_CONFIG.bin_lower_bound(last_key);
            self.adjust_basic_stats(lower_bound, remaining_count);
            key_counts.push((last_key, remaining_count));
        }

        // Sort the key counts first, as that's required by `insert_key_counts`.
        key_counts.sort_unstable_by(|(k1, _), (k2, _)| k1.cmp(k2));

        self.insert_key_counts(&key_counts);
    }

    /// Inserts histogram buckets into the sketch via linear interpolation.
    ///
    /// ## Errors
    ///
    /// Returns an error if a bucket size is greater that `u32::MAX`.
    pub fn insert_interpolate_buckets(&mut self, mut buckets: Vec<Bucket>) -> Result<(), &'static str> {
        // Buckets need to be sorted from lowest to highest so that we can properly calculate the rolling lower/upper
        // bounds.
        buckets.sort_by(|a, b| {
            let oa = OrderedFloat(a.upper_limit);
            let ob = OrderedFloat(b.upper_limit);

            oa.cmp(&ob)
        });

        let mut lower = f64::NEG_INFINITY;

        if buckets.iter().any(|bucket| bucket.count > u64::from(u32::MAX)) {
            return Err("bucket size greater than u32::MAX");
        }

        for bucket in buckets {
            let mut upper = bucket.upper_limit;
            if upper.is_sign_positive() && upper.is_infinite() {
                upper = lower;
            } else if lower.is_sign_negative() && lower.is_infinite() {
                lower = upper;
            }

            self.insert_interpolate_bucket(lower, upper, bucket.count);
            lower = bucket.upper_limit;
        }

        Ok(())
    }

    /// Adds a bin directly into the sketch.
    ///
    /// Used only for unit testing so that we can create a sketch with an exact layout, which allows testing around the
    /// resulting bins when feeding in specific values, as well as generating explicitly bad layouts for testing.
    #[allow(dead_code)]
    pub(crate) fn insert_raw_bin(&mut self, k: i16, n: u32) {
        let v = SKETCH_CONFIG.bin_lower_bound(k);
        self.adjust_basic_stats(v, u64::from(n));
        self.bins.push(Bin { k, n });
    }

    /// Gets the value at a given quantile.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        if self.count == 0 {
            return None;
        }

        if q <= 0.0 {
            return Some(self.min);
        }

        if q >= 1.0 {
            return Some(self.max);
        }

        let mut n = 0.0;
        let mut estimated = None;
        let wanted_rank = rank(self.count, q);

        for (i, bin) in self.bins.iter().enumerate() {
            n += f64::from(bin.n);
            if n <= wanted_rank {
                continue;
            }

            let weight = (n - wanted_rank) / f64::from(bin.n);
            let mut v_low = SKETCH_CONFIG.bin_lower_bound(bin.k);
            let mut v_high = v_low * SKETCH_CONFIG.gamma_v;

            if i == self.bins.len() {
                v_high = self.max;
            } else if i == 0 {
                v_low = self.min;
            }

            estimated = Some(v_low * weight + v_high * (1.0 - weight));
            break;
        }

        estimated.map(|v| v.clamp(self.min, self.max)).or(Some(f64::NAN))
    }

    /// Merges another sketch into this sketch, without a loss of accuracy.
    ///
    /// All samples present in the other sketch will be correctly represented in this sketch, and summary statistics
    /// such as the sum, average, count, min, and max, will represent the sum of samples from both sketches.
    pub fn merge(&mut self, other: &DDSketch) {
        // Merge the basic statistics together.
        self.count += other.count;
        if other.max > self.max {
            self.max = other.max;
        }
        if other.min < self.min {
            self.min = other.min;
        }
        self.sum += other.sum;
        self.avg = self.avg + (other.avg - self.avg) * other.count as f64 / self.count as f64;

        // Now merge the bins.
        let mut temp = SmallVec::<[Bin; 4]>::new();

        let mut bins_idx = 0;
        for other_bin in &other.bins {
            let start = bins_idx;
            while bins_idx < self.bins.len() && self.bins[bins_idx].k < other_bin.k {
                bins_idx += 1;
            }

            temp.extend_from_slice(&self.bins[start..bins_idx]);

            if bins_idx >= self.bins.len() || self.bins[bins_idx].k > other_bin.k {
                temp.push(*other_bin);
            } else if self.bins[bins_idx].k == other_bin.k {
                generate_bins(
                    &mut temp,
                    other_bin.k,
                    u64::from(other_bin.n) + u64::from(self.bins[bins_idx].n),
                );
                bins_idx += 1;
            }
        }

        temp.extend_from_slice(&self.bins[bins_idx..]);
        trim_left(&mut temp, SKETCH_CONFIG.bin_limit);

        self.bins = temp;
    }

    /// Merges this sketch into the `Dogsketch` Protocol Buffers representation.
    pub fn merge_to_dogsketch(&self, dogsketch: &mut Dogsketch) {
        dogsketch.set_cnt(i64::try_from(self.count).unwrap_or(i64::MAX));
        dogsketch.set_min(self.min);
        dogsketch.set_max(self.max);
        dogsketch.set_avg(self.avg);
        dogsketch.set_sum(self.sum);

        let mut k = Vec::new();
        let mut n = Vec::new();

        for bin in &self.bins {
            k.push(i32::from(bin.k));
            n.push(bin.n);
        }

        dogsketch.set_k(k);
        dogsketch.set_n(n);
    }
}

impl PartialEq for DDSketch {
    fn eq(&self, other: &Self) -> bool {
        // We skip checking the configuration because we don't allow creating configurations by hand, and it's always
        // locked to the constants used by the Datadog Agent.  We only check the configuration equality manually in
        // `DDSketch::merge`, to protect ourselves in the future if different configurations become allowed.
        //
        // Additionally, we also use floating-point-specific relative comparisons for sum/avg because they can be
        // minimally different between sketches purely due to floating-point behavior, despite being fed the same exact
        // data in terms of recorded samples.
        self.count == other.count
            && float_eq(self.min, other.min)
            && float_eq(self.max, other.max)
            && float_eq(self.sum, other.sum)
            && float_eq(self.avg, other.avg)
            && self.bins == other.bins
    }
}

impl Default for DDSketch {
    fn default() -> Self {
        Self {
            bins: SmallVec::new(),
            count: 0,
            min: f64::MAX,
            max: f64::MIN,
            sum: 0.0,
            avg: 0.0,
        }
    }
}

impl Eq for DDSketch {}

impl TryFrom<Dogsketch> for DDSketch {
    type Error = &'static str;

    fn try_from(value: Dogsketch) -> Result<Self, Self::Error> {
        let mut sketch = DDSketch {
            count: u64::try_from(value.cnt).map_err(|_| "sketch count overflows u64 or is negative")?,
            min: value.min,
            max: value.max,
            avg: value.avg,
            sum: value.sum,
            ..Default::default()
        };

        let k = value.k;
        let n = value.n;

        if k.len() != n.len() {
            return Err("k and n bin vectors have differing lengths");
        }

        for (k, n) in k.into_iter().zip(n.into_iter()) {
            let k = i16::try_from(k).map_err(|_| "bin key overflows i16")?;

            sketch.bins.push(Bin { k, n });
        }

        Ok(sketch)
    }
}

fn rank(count: u64, q: f64) -> f64 {
    let rank = q * (count - 1) as f64;
    rank.round_ties_even()
}

#[allow(clippy::cast_possible_truncation)]
fn buf_count_leading_equal(keys: &[i16], start_idx: usize) -> u64 {
    if start_idx == keys.len() - 1 {
        return 1;
    }

    let mut idx = start_idx;
    while idx < keys.len() && keys[idx] == keys[start_idx] {
        idx += 1;
    }

    // SAFETY: We limit the size of the vector (used to provide the slice given to us here) to be no larger than 2^32,
    // so we can't exceed u64 here.
    (idx - start_idx) as u64
}

fn trim_left(bins: &mut SmallVec<[Bin; 4]>, bin_limit: u16) {
    // We won't ever support Vector running on anything other than a 32-bit platform and above, I imagine, so this
    // should always be safe.
    let bin_limit = bin_limit as usize;
    if bin_limit == 0 || bins.len() <= bin_limit {
        return;
    }

    let num_to_remove = bins.len() - bin_limit;
    let mut missing: u64 = 0;

    // Sum all mass from the bins being removed. Per CollapsingLowestDenseStore in sketches-go,
    // all removed mass collapses into the first kept bin (the new minimum index). We accumulate
    // here without creating intermediate bins so that the overflow key is correct below.
    for bin in bins.iter().take(num_to_remove) {
        missing += u64::from(bin.n);
    }

    // Fold the accumulated mass into the first kept bin, matching Go's `bins[newMinIndex] += n`.
    let bin_remove = &mut bins[num_to_remove];
    let first_kept_k = bin_remove.k;
    missing = bin_remove.increment(missing);

    // Any mass that overflows the first kept bin's u32 counter generates additional bins at
    // first_kept_k — the same key as the first kept bin, not the keys of the removed bins.
    let mut overflow = SmallVec::<[Bin; 4]>::new();
    if missing > 0 {
        generate_bins(&mut overflow, first_kept_k, missing);
    }

    let (_, bins_end) = bins.split_at(num_to_remove);
    overflow.reserve(bins_end.len());
    overflow.extend_from_slice(bins_end);

    // Cap at `bin_limit` by dropping from the front so we keep the suffix (higher keys in `bins_end`).
    //
    // This can make `sum(bin.n)` smaller than the sketch's logical `count` (inserts still update `count`, min/max,
    // sum, avg). `quantile` ranks against `count` while walking bin masses, so results are approximate when those
    // diverge — the same class of issue as any hard cap that drops bins without rewriting aggregate stats.
    //
    // Even though we fold all removed mass into first_kept_k above, `generate_bins` may require more than
    // `bin_limit` bins for a single key when total weight is huge (u32 per bin), so "preserve all mass in-bounds"
    // is not always achievable; a plain prefix drop keeps the cap and favors retaining the tail keys.
    //
    // As of April 2026, this is an intentional divergence from the Datadog Agent implementation,
    // which does not truncate bins to stay under a limit.
    if overflow.len() > bin_limit {
        let drop_len = overflow.len() - bin_limit;
        overflow.drain(0..drop_len);
    }

    mem::swap(bins, &mut overflow);
}

#[allow(clippy::cast_possible_truncation)]
fn generate_bins(bins: &mut SmallVec<[Bin; 4]>, k: i16, n: u64) {
    if n < u64::from(MAX_BIN_WIDTH) {
        // SAFETY: Cannot truncate `n`, as it's less than a u32 value.
        bins.push(Bin { k, n: n as u32 });
    } else {
        let overflow = n % u64::from(MAX_BIN_WIDTH);
        if overflow != 0 {
            bins.push(Bin {
                k,
                // SAFETY: Cannot truncate `overflow`, as it's modulo'd by a u32 value.
                n: overflow as u32,
            });
        }

        for _ in 0..(n / u64::from(MAX_BIN_WIDTH)) {
            bins.push(Bin { k, n: MAX_BIN_WIDTH });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: build a SmallVec<[Bin; 4]> from (k, n) pairs. Assumes input is already sorted by k.
    fn make_bins(pairs: &[(i16, u32)]) -> SmallVec<[Bin; 4]> {
        pairs.iter().map(|&(k, n)| Bin { k, n }).collect()
    }

    // Helper: extract (k, n) pairs from a SmallVec for easy assertion.
    fn to_pairs(bins: &SmallVec<[Bin; 4]>) -> Vec<(i16, u32)> {
        bins.iter().map(|b| (b.k, b.n)).collect()
    }

    /// Basic collapse: when bins exceed the limit, the mass from removed bins is merged
    /// into the first kept bin (lowest surviving key). This mirrors the CollapsingLowestDenseStore
    /// semantics from sketches-go: all bins with index < (maxIndex - limit + 1) collapse into
    /// the bin at (maxIndex - limit + 1).
    ///
    /// Input:  [(0,2), (1,3), (2,4), (3,5)]  limit=2  →  remove 2 bins
    /// missing = n[0] + n[1] = 2 + 3 = 5
    /// first kept bin: k=2, n=4 → increment by 5 → n=9
    /// Result: [(2,9), (3,5)]
    #[test]
    fn trim_left_collapses_removed_mass_into_first_kept_bin() {
        let mut bins = make_bins(&[(0, 2), (1, 3), (2, 4), (3, 5)]);
        trim_left(&mut bins, 2);
        assert_eq!(to_pairs(&bins), vec![(2, 9), (3, 5)]);
    }

    /// Total count is preserved exactly when the collapse fits within a single u32 bin.
    ///
    /// Input:  [(10,5), (20,3), (30,7)]  limit=2  →  remove 1 bin
    /// missing = 5; first kept bin k=20, n=3 → n=8
    /// Result: [(20,8), (30,7)], total=15 == 5+3+7
    #[test]
    fn trim_left_preserves_total_count_when_no_overflow() {
        let mut bins = make_bins(&[(10, 5), (20, 3), (30, 7)]);
        let total_before: u64 = bins.iter().map(|b| u64::from(b.n)).sum();
        trim_left(&mut bins, 2);
        let total_after: u64 = bins.iter().map(|b| u64::from(b.n)).sum();
        assert_eq!(to_pairs(&bins), vec![(20, 8), (30, 7)]);
        assert_eq!(total_before, total_after);
    }

    /// With u32 bin counts, collapsed mass from multiple removed bins fits in a single bin
    /// without saturation for typical weights, fully preserving the total count.
    ///
    /// Input:  [(0,50000), (1,50000), (2,1)]  limit=1  →  remove 2 bins
    /// missing = 100000; bins[2].increment(100000): 100001 < u32::MAX → n=100001, returns 0
    /// No overflow bins generated. Final: [(2,100001)], all mass preserved.
    ///
    /// With the old u16 layout, this same input would have saturated at 65535 and discarded
    /// 34466 observations. u32 eliminates that loss for any per-bin count below ~4.3 billion.
    #[test]
    fn trim_left_preserves_exact_count_with_u32_bins() {
        let mut bins = make_bins(&[(0, 50000), (1, 50000), (2, 1)]);
        let total_before: u64 = bins.iter().map(|b| u64::from(b.n)).sum();
        trim_left(&mut bins, 1);
        let total_after: u64 = bins.iter().map(|b| u64::from(b.n)).sum();
        assert_eq!(bins.len(), 1);
        assert_eq!(bins[0].k, 2);
        assert_eq!(bins[0].n, 100001);
        assert_eq!(total_before, total_after, "all mass must be preserved with u32 bins");
    }

    /// When already at or under the limit, trim_left is a no-op.
    #[test]
    fn trim_left_no_op_when_within_limit() {
        let original = make_bins(&[(5, 10), (6, 20)]);
        let mut bins = original.clone();
        trim_left(&mut bins, 2);
        assert_eq!(to_pairs(&bins), to_pairs(&original));
        trim_left(&mut bins, 3);
        assert_eq!(to_pairs(&bins), to_pairs(&original));
    }

    /// Regression test for trim_left bin count with large per-sample weights.
    ///
    /// With the old u16 layout, a sample weight of ~260M would generate ceil(260M / 65535) = 3969
    /// bins per key, causing bin count explosion and an encoder panic. With u32, the same weight
    /// fits in a single bin (260M < u32::MAX ≈ 4.3B), so one insert_n call produces exactly one
    /// bin per key and the bin limit is trivially respected.
    ///
    /// This test inserts several values with a weight representative of what ADP receives when
    /// clamping an incoming sample rate of 3e-9 to its minimum of 3.845e-9 (~260M per sample),
    /// then asserts the bin count never exceeds DDSKETCH_CONF_BIN_LIMIT.
    #[test]
    fn trim_left_respects_bin_limit_with_large_weights() {
        // Weight corresponding to ADP's minimum safe sample rate (1 / 3.845e-9 ≈ 260_078_024).
        // With u32 bins, this fits in a single bin per key (260_078_024 < u32::MAX).
        let weight: u64 = 260_078_024;
        let bin_limit = usize::from(DDSKETCH_CONF_BIN_LIMIT);

        let mut sketch = DDSketch::default();

        // Insert enough distinct values to repeatedly trigger trim_left.  Ten values is more
        // than sufficient; two already exceed the bin limit without the fix.
        for i in 1..=10_i32 {
            sketch.insert_n(f64::from(i), weight);
            assert!(
                sketch.bins().len() <= bin_limit,
                "bin count {} exceeded limit {} after inserting {} value(s) at weight {}",
                sketch.bins().len(),
                bin_limit,
                i,
                weight,
            );
        }
    }
}
