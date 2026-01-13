//! A DDSketch implementation based on the Datadog Agent's DDSketch implementation.
#![deny(warnings)]
#![deny(missing_docs)]
use std::collections::HashMap;
use std::{cmp::Ordering, mem};

use datadog_protos::metrics::Dogsketch;
use datadog_protos::sketches::{index_mapping::Interpolation, DDSketch as ProtoDDSketch, IndexMapping, Store};
use float_cmp::ApproxEqRatio as _;
use ordered_float::OrderedFloat;
use smallvec::SmallVec;

use crate::{AgentSketchParameters, AsBinCount as _, AsBinKey as _, SketchParameters};

/// A sketch bin.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(
    feature = "serde",
    serde(bound(
        serialize = "P::BinKey: serde::Serialize, P::BinCount: serde::Serialize",
        deserialize = "P::BinKey: serde::Deserialize<'de>, P::BinCount: serde::Deserialize<'de>"
    ))
)]
pub struct Bin<P: SketchParameters> {
    /// The bin index.
    k: P::BinKey,

    /// The number of observations within the bin.
    n: P::BinCount,
}

impl<P: SketchParameters> Bin<P> {
    /// Creates a bin for the given key with a count of one.
    pub fn single(key: P::BinKey) -> Self {
        Bin {
            k: key,
            n: P::BinCount::ONE,
        }
    }

    /// Returns the key of the bin.
    pub fn key(&self) -> P::BinKey {
        self.k
    }

    /// Returns the number of observations within the bin.
    pub fn count(&self) -> P::BinCount {
        self.n
    }

    fn increment(&mut self, n: u64) -> u64 {
        // This adds `n` to `self.n` while handling any overflow of the numerical type backing `self.n`.
        let (next, overflow) = self.n.add_with_overflow(n);
        self.n = next;
        overflow
    }
}

/// A histogram bucket.
///
/// This type can be used to represent a classic histogram bucket, such as those defined by Prometheus or OpenTelemetry,
/// in a way that is then able to be inserted into the DDSketch via linear interpolation.
pub struct Bucket {
    /// The upper limit (inclusive) of values in the bucket.
    pub upper_limit: f64,

    /// The number of values in the bucket.
    pub count: u64,
}

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
#[cfg_attr(
    feature = "serde",
    serde(bound(
        serialize = "P::BinKey: serde::Serialize, P::BinCount: serde::Serialize",
        deserialize = "P::BinKey: serde::Deserialize<'de>, P::BinCount: serde::Deserialize<'de>"
    ))
)]
pub struct DDSketch<P = AgentSketchParameters>
where
    P: SketchParameters,
{
    /// The bins within the sketch.
    bins: SmallVec<[Bin<P>; 4]>,

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

impl<P: SketchParameters> DDSketch<P> {
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
    pub fn bins(&self) -> &[Bin<P>] {
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

    fn insert_key_counts(&mut self, counts: &[(P::BinKey, u64)]) {
        let mut temp = SmallVec::<[Bin<P>; 4]>::new();

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
                    generate_bins(&mut temp, bin.k, bin.n.to_u64() + kn);
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

        trim_left(&mut temp, P::BIN_LIMIT);

        // PERF TODO: This is where we might do a mem::swap instead so that we could shove the bin vector into an object
        // pool but I'm not sure this actually matters at the moment.
        self.bins = temp;
    }

    fn insert_keys(&mut self, mut keys: Vec<P::BinKey>) {
        // Updating more than 4 billion keys would be very very weird and likely indicative of something horribly
        // broken.
        //
        // TODO: I don't actually understand why I wrote this assertion in this way. Either the code can handle
        // collapsing values in order to maintain the relative error bounds, or we have to cap it to the maximum allowed
        // number of bins. Gotta think about this some more.
        assert!(keys.len() <= u32::MAX.try_into().expect("we don't support 16-bit systems"));

        keys.sort_unstable();

        let mut temp = SmallVec::<[Bin<P>; 4]>::new();

        let mut bins_idx = 0;
        let mut key_idx = 0;
        let bins_len = self.bins.len();
        let keys_len = keys.len();

        // PERF TODO: See `insert_key_counts` for ideas on fast path optimization.
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
                    generate_bins(&mut temp, bin.k, bin.n.to_u64() + kn);
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

        trim_left(&mut temp, P::BIN_LIMIT);

        // PERF TODO: See `insert_key_counts` for ideas on buffer reuse.
        self.bins = temp;
    }

    /// Inserts a single value into the sketch.
    pub fn insert(&mut self, v: f64) {
        // TODO: This should return a result that makes sure we have enough room to actually add 1 more sample without
        // hitting `self.config.max_count()`
        self.adjust_basic_stats(v, 1);

        let key = P::key(v);

        let mut insert_at = None;

        for (bin_idx, b) in self.bins.iter_mut().enumerate() {
            if b.k == key {
                if b.n < P::MAX_BIN_COUNT {
                    // Fast path for adding to an existing bin without overflow.
                    b.n = b.n + P::BinCount::ONE;
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

        let bin = Bin::single(key);
        if let Some(bin_idx) = insert_at {
            self.bins.insert(bin_idx, bin);
        } else {
            self.bins.push(bin);
        }
        trim_left(&mut self.bins, P::BIN_LIMIT);
    }

    /// Inserts many values into the sketch.
    pub fn insert_many(&mut self, vs: &[f64]) {
        // TODO: This should return a result that makes sure we have enough room to actually add N more samples without
        // hitting `self.config.bin_limit`.
        let mut keys = Vec::with_capacity(vs.len());
        for v in vs {
            self.adjust_basic_stats(*v, 1);
            keys.push(P::key(*v));
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

            let key = P::key(v);
            self.insert_key_counts(&[(key, n)]);
        }
    }

    fn insert_interpolate_bucket(&mut self, lower: f64, upper: f64, count: u64) {
        // Find the keys for the bins where the lower bound and upper bound would end up, and collect all of the keys in
        // between, inclusive.
        let lower_key = P::key(lower);
        let upper_key = P::key(upper);
        let keys = lower_key.keys_between(upper_key);

        let mut key_counts = Vec::new();
        let mut remaining_count = count;
        let distance = upper - lower;
        let mut start_idx = 0;
        let mut end_idx = 1;
        let mut lower_bound = P::bin_lower_bound(keys[start_idx]);
        let mut remainder = 0.0;

        while end_idx < keys.len() && remaining_count > 0 {
            // For each key, map the total distance between the input lower/upper bound against the sketch lower/upper
            // bound for the current sketch bin, which tells us how much of the input count to apply to the current
            // sketch bin.
            let upper_bound = P::bin_lower_bound(keys[end_idx]);
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
            lower_bound = P::bin_lower_bound(last_key);
            self.adjust_basic_stats(lower_bound, remaining_count);
            key_counts.push((last_key, remaining_count));
        }

        // Sort the key counts first, as that's required by `insert_key_counts`.
        key_counts.sort_unstable_by(|(k1, _), (k2, _)| k1.cmp(k2));

        self.insert_key_counts(&key_counts);
    }

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
    #[cfg(test)]
    pub(crate) fn insert_raw_bin(&mut self, k: P::BinKey, n: P::BinCount) {
        let v = P::bin_lower_bound(k);
        self.adjust_basic_stats(v, n.to_u64());
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
            let nf = bin.n.to_f64();
            n += nf;
            if n <= wanted_rank {
                continue;
            }

            let weight = (n - wanted_rank) / nf;
            let mut v_low = P::bin_lower_bound(bin.k);
            let mut v_high = v_low * P::GAMMA_V;

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
    pub fn merge(&mut self, other: &DDSketch<P>) {
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
        let mut temp = SmallVec::<[Bin<P>; 4]>::new();

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
                    other_bin.n.to_u64() + self.bins[bins_idx].n.to_u64(),
                );
                bins_idx += 1;
            }
        }

        temp.extend_from_slice(&self.bins[bins_idx..]);
        trim_left(&mut temp, P::BIN_LIMIT);

        self.bins = temp;
    }

    /// Converts this sketch to canonical Protocol Buffers representation.
    pub fn to_proto(&self) -> ProtoDDSketch {
        let mut proto = ProtoDDSketch::new();
        let mut mapping = IndexMapping::new();
        mapping.set_gamma(P::GAMMA_V);
        mapping.set_indexOffset(f64::from(P::NORM_BIAS));
        mapping.set_interpolation(Interpolation::NONE);
        proto.set_mapping(mapping);

        let mut positive_counts = HashMap::new();
        let mut negative_counts = HashMap::new();
        let mut zero_count: f64 = 0.0;

        for bin in &self.bins {
            let count = bin.n.to_f64();
            match bin.k.cmp(&P::BinKey::ZERO) {
                Ordering::Greater => {
                    *positive_counts.entry(bin.k.into_i32()).or_insert(0.0) += count;
                }
                Ordering::Less => {
                    *negative_counts.entry(-bin.k.into_i32()).or_insert(0.0) += count;
                }
                Ordering::Equal => {
                    zero_count += count;
                }
            }
        }

        if !positive_counts.is_empty() {
            let mut pos_store = Store::new();
            pos_store.set_binCounts(positive_counts);
            proto.set_positiveValues(pos_store);
        }

        if !negative_counts.is_empty() {
            let mut neg_store = Store::new();
            neg_store.set_binCounts(negative_counts);
            proto.set_negativeValues(neg_store);
        }

        proto.set_zeroCount(zero_count);

        proto
    }
}

impl DDSketch<AgentSketchParameters> {
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
            n.push(u32::from(bin.n));
        }

        dogsketch.set_k(k);
        dogsketch.set_n(n);
    }
}

impl<P: SketchParameters + PartialEq> PartialEq for DDSketch<P> {
    fn eq(&self, other: &Self) -> bool {
        // We also use floating-point-specific relative comparisons for sum/avg because they can be minimally different
        // between sketches purely due to floating-point behavior, despite being fed the same exact data in terms of
        // recorded samples.
        self.count == other.count
            && float_eq(self.min, other.min)
            && float_eq(self.max, other.max)
            && float_eq(self.sum, other.sum)
            && float_eq(self.avg, other.avg)
            && self.bins == other.bins
    }
}

impl<P: SketchParameters> Default for DDSketch<P> {
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

impl<P: SketchParameters + Eq> Eq for DDSketch<P> {}

impl TryFrom<Dogsketch> for DDSketch<AgentSketchParameters> {
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
            let n = u16::try_from(n).map_err(|_| "bin count overflows u16")?;

            sketch.bins.push(Bin { k, n });
        }

        Ok(sketch)
    }
}

fn float_eq(l_value: f64, r_value: f64) -> bool {
    // When comparing two values, the smaller value cannot deviate by more than 0.0000001% of the larger value.
    const RATIO_ERROR: f64 = 0.00000001;

    (l_value.is_nan() && r_value.is_nan()) || l_value.approx_eq_ratio(&r_value, RATIO_ERROR)
}

fn rank(count: u64, q: f64) -> f64 {
    let rank = q * (count - 1) as f64;
    rank.round_ties_even()
}

#[allow(clippy::cast_possible_truncation)]
fn buf_count_leading_equal<K: Eq>(keys: &[K], start_idx: usize) -> u64 {
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

fn trim_left<P: SketchParameters>(bins: &mut SmallVec<[Bin<P>; 4]>, bin_limit: usize) {
    if bin_limit == 0 || bins.len() < bin_limit {
        return;
    }

    // Figure out how many bins we need to remove by seeing how far past the configured bin limit we are.
    let num_to_remove = bins.len() - bin_limit;
    let mut remainder = 0;
    let mut overflow_bins = SmallVec::<[Bin<P>; 4]>::new();

    // Working from the left (or "low collapsing"), go through the number of bins we need to remove and aggregate
    // their count together. When our aggregated count exceeds the width of a bin, we create an "overflow" bin (based
    // on the key of the bin we're currently iterating over) with the count set to the max bin width, and then we
    // remove that amount from our aggregated count.
    //
    // This ends up looking something like this:
    //
    //  Original bins, with a bin width of 100 and `num_to_remove` as `5`:
    // ┌───────────┐ ┌───────────┐ ┌───────────┐ ┌───────────┐ ┌───────────┐ ┌───────────┐
    // │  (0, 50)  │ │  (1, 25)  │ │  (2, 50)  │ │  (3, 40)  │ │  (4, 40)  │ │  (5, 50)  │
    // └───────────┘ └───────────┘ └───────────┘ └───────────┘ └───────────┘ └───────────┘
    //
    // We collapse the first two bins, which don't overflow the bin width, but as soon as we hit the third bin, we
    // overflow (125 > 100), so we capture an overflow bin (key 2, width: 100) and hold on to the remainder (25). We
    // continue iterating over the next two bins which leads to us overflowing again when hitting the fifth bin (105 >
    // 100). We capture a second overflow bin (key: 4, width: 100) and then exit the loop. Finally, our cleanup logic
    // notices we still have a non-zero remainder (5), which we need to add to next bin that comes after the last bin
    // that we iterated over:
    // ┌──────────────────┐ ┌───────────────────┐ ┌──────────────────┐ ┌─────────────────┐
    // │     (2, 100)     │ │      (4, 100)     │ │      (4, 5)      │ │     (5, 55)     │
    // └──────────────────┘ └───────────────────┘ └──────────────────┘ └─────────────────┘
    for bin in bins.iter().take(num_to_remove) {
        let (n, overflow) = bin.n.add_with_overflow(remainder);
        remainder = if overflow > 0 {
            // We overflowed, so we capture an overflow bin with a count of `n`.
            overflow_bins.push(Bin { k: bin.k, n });
            overflow
        } else {
            // We didn't overflow, so keep carrying our sum forward as our "remainder".
            n.to_u64()
        };
    }

    // If we have a non-zero remainder, we add it to the first bin that comes after the bins we iterated over.
    //
    // Similarly, if we would overflow _that_ bin by incrementing it with the remainder, then we have to split it into
    // multiple bins of the same key.
    if remainder > 0 {
        let bin_remove = &mut bins[num_to_remove];
        let final_remainder = bin_remove.increment(remainder);
        if final_remainder > 0 {
            generate_bins(&mut overflow_bins, bin_remove.k, final_remainder);
        }
    }

    let overflow_bins_len = overflow_bins.len();
    let (_, bins_end) = bins.split_at(num_to_remove);
    overflow_bins.extend_from_slice(bins_end);

    // I still don't yet understand how this works, since you'd think bin limit should be the overall limit of the
    // number of bins, but we're allowing more than that.. :thinkies:
    overflow_bins.truncate(bin_limit + overflow_bins_len);

    mem::swap(bins, &mut overflow_bins);
}

fn generate_bins<P: SketchParameters>(bins: &mut SmallVec<[Bin<P>; 4]>, k: P::BinKey, n: u64) {
    // We split `n` into chunks of `P::MAX_BIN_COUNT`, with the remainder bin (the one that is not full) coming first.
    let (full_bins, remainder) = P::BinCount::div_rem_overflow(n);
    if !remainder.is_zero() {
        bins.push(Bin { k, n: remainder });
    }

    for _ in 0..full_bins {
        bins.push(Bin { k, n: P::MAX_BIN_COUNT });
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    const FLOATING_POINT_ACCEPTABLE_ERROR: f64 = 1.0e-10;

    #[test]
    fn test_ddsketch_basic() {
        let mut sketch: DDSketch = DDSketch::default();
        assert!(sketch.is_empty());
        assert_eq!(sketch.count(), 0);
        assert_eq!(sketch.min(), None);
        assert_eq!(sketch.max(), None);
        assert_eq!(sketch.sum(), None);
        assert_eq!(sketch.avg(), None);

        sketch.insert(3.15);
        assert!(!sketch.is_empty());
        assert_eq!(sketch.count(), 1);
        assert_eq!(sketch.min(), Some(3.15));
        assert_eq!(sketch.max(), Some(3.15));
        assert_eq!(sketch.sum(), Some(3.15));
        assert_eq!(sketch.avg(), Some(3.15));

        sketch.insert(2.28);
        assert!(!sketch.is_empty());
        assert_eq!(sketch.count(), 2);
        assert_eq!(sketch.min(), Some(2.28));
        assert_eq!(sketch.max(), Some(3.15));
        assert_eq!(sketch.sum(), Some(5.43));
        assert_eq!(sketch.avg(), Some(2.715));
    }

    #[test]
    fn test_ddsketch_clear() {
        let sketch1: DDSketch = DDSketch::default();
        let mut sketch2 = DDSketch::default();

        assert_eq!(sketch1, sketch2);
        assert!(sketch1.is_empty());
        assert!(sketch2.is_empty());

        sketch2.insert(3.15);
        assert_ne!(sketch1, sketch2);
        assert!(!sketch2.is_empty());

        sketch2.clear();
        assert_eq!(sketch1, sketch2);
        assert!(sketch2.is_empty());
    }

    #[test]
    fn test_ddsketch_neg_to_pos() {
        // This gives us 10k values because otherwise this test runs really slow in debug mode.
        let start = -1.0;
        let end = 1.0;
        let delta = 0.0002;

        let mut sketch: DDSketch = DDSketch::default();

        let mut v = start;
        while v <= end {
            sketch.insert(v);

            v += delta;
        }

        let min = sketch.quantile(0.0).expect("should have value");
        let median = sketch.quantile(0.5).expect("should have value");
        let max = sketch.quantile(1.0).expect("should have value");

        assert_eq!(start, min);
        assert!(median.abs() < FLOATING_POINT_ACCEPTABLE_ERROR);
        assert!((end - max).abs() < FLOATING_POINT_ACCEPTABLE_ERROR);
    }

    #[test]
    fn test_out_of_range_buckets_error() {
        let empty_sketch: DDSketch = DDSketch::default();
        let mut sketch: DDSketch = DDSketch::default();

        let buckets = vec![
            Bucket {
                upper_limit: 5.4,
                count: 32,
            },
            Bucket {
                upper_limit: 5.8,
                count: u64::from(u32::MAX) + 1,
            },
            Bucket {
                upper_limit: 9.2,
                count: 320,
            },
        ];

        assert_eq!(
            Err("bucket size greater than u32::MAX"),
            sketch.insert_interpolate_buckets(buckets)
        );

        // Assert the sketch remains unchanged.
        assert_eq!(empty_sketch, sketch);
    }

    #[test]
    fn test_merge() {
        let mut all_values: DDSketch = DDSketch::default();
        let mut odd_values: DDSketch = DDSketch::default();
        let mut even_values: DDSketch = DDSketch::default();
        let mut all_values_many: DDSketch = DDSketch::default();

        let mut values = Vec::new();
        for i in -50..=50 {
            let v = f64::from(i);

            all_values.insert(v);

            if i & 1 == 0 {
                odd_values.insert(v);
            } else {
                even_values.insert(v);
            }

            values.push(v);
        }

        all_values_many.insert_many(&values);

        odd_values.merge(&even_values);
        let merged_values = odd_values;

        // Number of bins should be equal to the number of values we inserted.
        assert_eq!(all_values.bin_count(), values.len());

        // Values at both ends of the quantile range should be equal.
        let low_end = all_values.quantile(0.01).expect("should have estimated value");
        let high_end = all_values.quantile(0.99).expect("should have estimated value");
        assert_eq!(high_end, -low_end);

        let target_bin_count = all_values.bin_count();
        for sketch in &[all_values, all_values_many, merged_values] {
            assert_eq!(sketch.quantile(0.5), Some(0.0));
            assert_eq!(sketch.quantile(0.0), Some(-50.0));
            assert_eq!(sketch.quantile(1.0), Some(50.0));

            for p in 0..50 {
                let q = f64::from(p) / 100.0;
                let positive = sketch.quantile(q + 0.5).expect("should have estimated value");
                let negative = -sketch.quantile(0.5 - q).expect("should have estimated value");

                assert!(
                    (positive - negative).abs() <= 1.0e-6,
                    "positive vs negative difference too great ({positive} vs {negative})",
                );
            }

            assert_eq!(target_bin_count, sketch.bin_count());
        }
    }

    #[test]
    fn test_relative_accuracy_fast() {
        // These values are based on the Agent's unit tests for asserting relative accuracy of the DDSketch
        // implementation. Notably, it does not seem to test the full extent of values that the open-source
        // implementations do, but then again... all we care about is parity with the Agent version so we can pass them
        // through.
        //
        // Another noteworthy thing: it seems that they don't test from the actual targeted minimum value, which is
        // 1.0e-9, which would give nanosecond granularity vs just microsecond granularity.
        let min_value = 1.0;

        // We don't care about precision loss, just consistency.
        #[allow(clippy::cast_possible_truncation)]
        let max_value = AgentSketchParameters::GAMMA_V.powf(5.0) as f32;

        test_relative_accuracy::<AgentSketchParameters>(min_value, max_value);
    }

    #[test]
    #[cfg(feature = "ddsketch_extended")]
    fn test_relative_accuracy_slow() {
        // These values are based on the Agent's unit tests for asserting relative accuracy of the DDSketch
        // implementation. Notably, it does not seem to test the full extent of values that the open-source
        // implementations do, but then again... all we care about is parity with the Agent version so we can pass them
        // through.
        //
        // Another noteworthy thing: it seems that they don't test from the actual targeted minimum value, which is
        // 1.0e-9, which would give nanosecond granularity vs just microsecond granularity.
        //
        // This test uses a far larger range of values, and takes 60-70 seconds, hence why we've guarded it here behind
        // a cfg flag.
        let min_value = 1.0e-6;
        let max_value = i64::MAX as f32;

        test_relative_accuracy::<AgentSketchParameters>(min_value, max_value);
    }

    fn parse_sketch_from_string_bins(layout: &str) -> DDSketch {
        layout
            .split(' ')
            .filter(|v| !v.is_empty())
            .map(|pair| pair.split(':').map(ToOwned::to_owned).collect::<Vec<_>>())
            .fold(DDSketch::default(), |mut sketch, mut kn| {
                let k = kn.remove(0).parse::<i16>().unwrap();
                let n = kn.remove(0).parse::<u16>().unwrap();

                sketch.insert_raw_bin(k, n);
                sketch
            })
    }

    fn compare_sketches(actual: &DDSketch, expected: &DDSketch, allowed_err: f64) {
        let actual_sum = actual.sum().unwrap();
        let expected_sum = expected.sum().unwrap();
        let actual_avg = actual.avg().unwrap();
        let expected_avg = expected.avg().unwrap();
        let sum_delta = (actual_sum - expected_sum).abs();
        let avg_delta = (actual_avg - expected_avg).abs();
        assert!(sum_delta <= allowed_err);
        assert!(avg_delta <= allowed_err);
        assert_eq!(actual.min(), expected.min());
        assert_eq!(actual.max(), expected.max());
        assert_eq!(actual.count(), expected.count());
        assert_eq!(actual.bins(), expected.bins());
    }

    #[test]
    fn test_sketch_insert_and_overflow() {
        /// values to insert into a sketch
        #[allow(dead_code)]
        enum Value {
            Float(f64),
            Vec(Vec<f64>),
            NFloats(u64, f64),
        }
        /// ways to insert values into a sketch
        #[derive(Debug)]
        enum InsertFn {
            Insert,
            InsertMany,
            InsertN,
        }
        struct Case {
            description: &'static str,
            start: &'static str,
            insert: Value,
            expected: &'static str,
        }

        let cases = &[
            Case {
                description: "baseline: inserting into an empty sketch",
                start: "",
                insert: Value::Float(0.0),
                expected: "0:1",
            },
            Case {
                description: "inserting a value into an existing bin",
                start: "0:1",
                insert: Value::Float(0.0),
                expected: "0:2",
            },
            Case {
                description: "inserting a value into a new bin at the start",
                start: "1338:1",
                insert: Value::Float(0.0),
                expected: "0:1 1338:1",
            },
            Case {
                description: "inserting a value into a new bin in the middle",
                start: "0:1 1338:1",
                insert: Value::Float(0.5),
                expected: "0:1 1293:1 1338:1",
            },
            Case {
                description: "inserting a value into a new bin at the end",
                start: "0:1",
                insert: Value::Float(1.0),
                expected: "0:1 1338:1",
            },
            Case {
                description: "inserting a value into an existing bin and filling it",
                start: "0:65534",
                insert: Value::Float(0.0),
                expected: "0:65535",
            },
            Case {
                description: "inserting a value into an existing bin and causing an overflow",
                start: "0:65535",
                insert: Value::Float(0.0),
                expected: "0:1 0:65535",
            },
            Case {
                description: "inserting many values into a sketch and causing an overflow",
                start: "0:100",
                insert: Value::NFloats(65535, 0.0),
                expected: "0:100 0:65535",
            },
            Case {
                description: "inserting a value into a new bin in the middle and causing an overflow",
                start: "0:1 1338:1",
                insert: Value::NFloats(65536, 0.5),
                expected: "0:1 1293:1 1293:65535 1338:1",
            },
        ];

        for case in cases {
            for insert_fn in &[InsertFn::Insert, InsertFn::InsertMany, InsertFn::InsertN] {
                // Insert each value every way possible.

                let mut sketch = parse_sketch_from_string_bins(case.start);

                match insert_fn {
                    InsertFn::Insert => match &case.insert {
                        Value::Float(v) => sketch.insert(*v),
                        Value::Vec(vs) => {
                            for v in vs {
                                sketch.insert(*v);
                            }
                        }
                        Value::NFloats(n, v) => {
                            for _ in 0..*n {
                                sketch.insert(*v);
                            }
                        }
                    },
                    InsertFn::InsertMany => match &case.insert {
                        Value::Float(v) => sketch.insert_many(&[*v]),
                        Value::Vec(vs) => sketch.insert_many(vs),
                        Value::NFloats(n, v) => {
                            for _ in 0..*n {
                                sketch.insert_many(&[*v]);
                            }
                        }
                    },
                    InsertFn::InsertN => match &case.insert {
                        Value::Float(v) => sketch.insert_n(*v, 1),
                        Value::Vec(vs) => {
                            for v in vs {
                                sketch.insert_n(*v, 1);
                            }
                        }
                        Value::NFloats(n, v) => sketch.insert_n(*v, *n),
                    },
                }

                let expected = parse_sketch_from_string_bins(case.expected);
                assert_eq!(expected.bins(), sketch.bins(), "{}", case.description);
            }
        }
    }

    #[test]
    fn test_histogram_interpolation_agent_similarity() {
        #[derive(Clone)]
        struct Case {
            lower: f64,
            upper: f64,
            count: u64,
            allowed_err: f64,
            expected: &'static str,
        }

        let check_result = |actual: &DDSketch, case: &Case| {
            let expected = parse_sketch_from_string_bins(case.expected);

            assert_eq!(expected.count(), case.count);
            assert_eq!(actual.count(), case.count);
            assert_eq!(actual.bins(), expected.bins());
            compare_sketches(actual, &expected, case.allowed_err);

            let actual_count: u64 = actual.bins.iter().map(|b| u64::from(b.n)).sum();
            assert_eq!(actual_count, case.count);
        };

        let cases = &[
            Case { lower: 0.0, upper: 10.0, count: 2, allowed_err: 0.0, expected: "0:1 1442:1" },
            Case { lower: 10.0, upper: 20.0, count: 4,  allowed_err: 0.0, expected: "1487:1 1502:1 1514:1 1524:1" },
		    Case { lower: -10.0, upper: 10.0, count: 4, allowed_err: 0.0, expected: "-1487:1 -1442:1 -1067:1 1442:1"},
            Case { lower: 0.0, upper: 10.0, count: 100, allowed_err: 0.0, expected: "0:1 1190:1 1235:1 1261:1 1280:1 1295:1 1307:1 1317:1 1326:1 1334:1 1341:1 1347:1 1353:1 1358:1 1363:1 1368:1 1372:2 1376:1 1380:1 1384:1 1388:1 1391:1 1394:1 1397:2 1400:1 1403:1 1406:2 1409:1 1412:1 1415:2 1417:1 1419:1 1421:1 1423:1 1425:1 1427:1 1429:2 1431:1 1433:1 1435:2 1437:1 1439:2 1441:1 1443:2 1445:2 1447:1 1449:2 1451:2 1453:2 1455:2 1457:2 1459:1 1460:1 1461:1 1462:1 1463:1 1464:1 1465:1 1466:1 1467:2 1468:1 1469:1 1470:1 1471:1 1472:2 1473:1 1474:1 1475:1 1476:2 1477:1 1478:2 1479:1 1480:1 1481:2 1482:1 1483:2 1484:1 1485:2 1486:1" },
		    Case { lower: 1_000.0, upper: 100_000.0, count: 1_000_000 - 1, allowed_err: 0.0, expected: "1784:158 1785:162 1786:164 1787:166 1788:170 1789:171 1790:175 1791:177 1792:180 1793:183 1794:185 1795:189 1796:191 1797:195 1798:197 1799:201 1800:203 1801:207 1802:210 1803:214 1804:217 1805:220 1806:223 1807:227 1808:231 1809:234 1810:238 1811:242 1812:245 1813:249 1814:253 1815:257 1816:261 1817:265 1818:270 1819:273 1820:278 1821:282 1822:287 1823:291 1824:295 1825:300 1826:305 1827:310 1828:314 1829:320 1830:324 1831:329 1832:335 1833:340 1834:345 1835:350 1836:356 1837:362 1838:367 1839:373 1840:379 1841:384 1842:391 1843:397 1844:403 1845:409 1846:416 1847:422 1848:429 1849:435 1850:442 1851:449 1852:457 1853:463 1854:470 1855:478 1856:486 1857:493 1858:500 1859:509 1860:516 1861:525 1862:532 1863:541 1864:550 1865:558 1866:567 1867:575 1868:585 1869:594 1870:603 1871:612 1872:622 1873:632 1874:642 1875:651 1876:662 1877:672 1878:683 1879:693 1880:704 1881:716 1882:726 1883:738 1884:749 1885:761 1886:773 1887:785 1888:797 1889:809 1890:823 1891:835 1892:848 1893:861 1894:875 1895:889 1896:902 1897:917 1898:931 1899:945 1900:960 1901:975 1902:991 1903:1006 1904:1021 1905:1038 1906:1053 1907:1071 1908:1087 1909:1104 1910:1121 1911:1138 1912:1157 1913:1175 1914:1192 1915:1212 1916:1231 1917:1249 1918:1269 1919:1290 1920:1309 1921:1329 1922:1351 1923:1371 1924:1393 1925:1415 1926:1437 1927:1459 1928:1482 1929:1506 1930:1529 1931:1552 1932:1577 1933:1602 1934:1626 1935:1652 1936:1678 1937:1704 1938:1731 1939:1758 1940:1785 1941:1813 1942:1841 1943:1870 1944:1900 1945:1929 1946:1959 1947:1990 1948:2021 1949:2052 1950:2085 1951:2117 1952:2150 1953:2184 1954:2218 1955:2253 1956:2287 1957:2324 1958:2360 1959:2396 1960:2435 1961:2472 1962:2511 1963:2550 1964:2589 1965:2631 1966:2671 1967:2714 1968:2755 1969:2799 1970:2842 1971:2887 1972:2932 1973:2978 1974:3024 1975:3071 1976:3120 1977:3168 1978:3218 1979:3268 1980:3319 1981:3371 1982:3423 1983:3477 1984:3532 1985:3586 1986:3643 1987:3700 1988:3757 1989:3816 1990:3876 1991:3936 1992:3998 1993:4060 1994:4124 1995:4188 1996:4253 1997:4320 1998:4388 1999:4456 2000:4526 2001:4596 2002:4668 2003:4741 2004:4816 2005:4890 2006:4967 2007:5044 2008:5124 2009:5203 2010:5285 2011:5367 2012:5451 2013:5536 2014:5623 2015:5711 2016:5800 2017:5890 2018:5983 2019:6076 2020:6171 2021:6267 2022:6365 2023:6465 2024:6566 2025:6668 2026:6773 2027:6878 2028:6986 2029:7095 2030:7206 2031:7318 2032:7433 2033:7549 2034:7667 2035:7786 2036:7909 2037:8032 2038:8157 2039:8285 2040:8414 2041:8546 2042:8679 2043:8815 2044:8953 2045:9092 2046:9235 2047:9379 2048:9525 2049:9675 2050:9825 2051:9979 2052:10135 2053:10293 2054:10454 2055:10618 2056:10783 2057:10952 2058:11123 2059:11297 2060:11473 2061:11653 2062:11834 2063:12020 2064:12207 2065:12398 2066:12592 2067:12788 2068:12989 2069:13191 2070:13397 2071:13607 2072:13819 2073:14036 2074:14254 2075:14478 2076:14703 2077:14933 2078:15167 2079:15403 2080:8942" },
		    Case { lower: 1_000.0, upper: 10_000.0, count: 10_000_000 - 1, allowed_err: 0.00001, expected: "1784:17485 1785:17758 1786:18035 1787:18318 1788:18604 1789:18894 1790:19190 1791:19489 1792:19794 1793:20103 1794:20418 1795:20736 1796:21061 1797:21389 1798:21724 1799:22063 1800:22408 1801:22758 1802:23113 1803:23475 1804:23841 1805:24215 1806:24592 1807:24977 1808:25366 1809:25764 1810:26165 1811:26575 1812:26990 1813:27412 1814:27839 1815:28275 1816:28717 1817:29165 1818:29622 1819:30083 1820:30554 1821:31032 1822:31516 1823:32009 1824:32509 1825:33016 1826:33533 1827:34057 1828:34589 1829:35129 1830:35678 1831:36235 1832:36802 1833:37377 1834:37961 1835:38554 1836:39156 1837:39768 1838:40390 1839:41020 1840:41662 1841:42312 1842:42974 1843:43645 1844:44327 1845:45020 1846:45723 1847:46438 1848:47163 1849:47900 1850:48648 1851:49409 1852:50181 1853:50964 1854:51761 1855:52570 1856:53391 1857:54226 1858:55072 1859:55934 1860:56807 1861:57695 1862:58596 1863:59512 1864:60441 1865:61387 1866:62345 1867:63319 1868:64309 1869:65314 1870:799 1870:65535 1871:1835 1871:65535 1872:2889 1872:65535 1873:3957 1873:65535 1874:5043 1874:65535 1875:6146 1875:65535 1876:7266 1876:65535 1877:8404 1877:65535 1878:9559 1878:65535 1879:10732 1879:65535 1880:11923 1880:65535 1881:13135 1881:65535 1882:14363 1882:65535 1883:15612 1883:65535 1884:16879 1884:65535 1885:18168 1885:65535 1886:19475 1886:65535 1887:20803 1887:65535 1888:22153 1888:65535 1889:23523 1889:65535 1890:24914 1890:65535 1891:26327 1891:65535 1892:27763 1892:65535 1893:29221 1893:65535 1894:30701 1894:65535 1895:32205 1895:65535 1896:33732 1896:65535 1897:35283 1897:65535 1898:36858 1898:65535 1899:38458 1899:65535 1900:40084 1900:65535 1901:41733 1901:65535 1902:43409 1902:65535 1903:45112 1903:65535 1904:46841 1904:65535 1905:48596 1905:65535 1906:50380 1906:65535 1907:52191 1907:65535 1908:54030 1908:65535 1909:55899 1909:65535 1910:57796 1910:65535 1911:59723 1911:65535 1912:61680 1912:65535 1913:63668 1913:65535 1914:152 1914:65535 1914:65535 1915:2202 1915:65535 1915:65535 1916:4285 1916:65535 1916:65535 1917:6399 1917:65535 1917:65535 1918:8547 1918:65535 1918:65535 1919:10729 1919:65535 1919:65535 1920:12945 1920:65535 1920:65535 1921:15195 1921:65535 1921:65535 1922:17480 1922:65535 1922:65535 1923:19801 1923:65535 1923:65535 1924:22158 1924:65535 1924:65535 1925:24553 1925:65535 1925:65535 1926:26985 1926:65535 1926:65535 1927:29453 1927:65535 1927:65535 1928:31963 1928:65535 1928:65535 1929:34509 1929:65535 1929:65535 1930:37097 1930:65535 1930:65535 1931:39724 1931:65535 1931:65535 1932:17411"},
        ];

        let double_insert_cases = &[
            Case { lower: 1_000.0, upper: 10_000.0, count: 10_000_000 - 1, allowed_err: 0.0002, expected: "1784:34970 1785:35516 1786:36070 1787:36636 1788:37208 1789:37788 1790:38380 1791:38978 1792:39588 1793:40206 1794:40836 1795:41472 1796:42122 1797:42778 1798:43448 1799:44126 1800:44816 1801:45516 1802:46226 1803:46950 1804:47682 1805:48430 1806:49184 1807:49954 1808:50732 1809:51528 1810:52330 1811:53150 1812:53980 1813:54824 1814:55678 1815:56550 1816:57434 1817:58330 1818:59244 1819:60166 1820:61108 1821:62064 1822:63032 1823:64018 1824:65018 1825:497 1825:65535 1826:1531 1826:65535 1827:2579 1827:65535 1828:3643 1828:65535 1829:4723 1829:65535 1830:5821 1830:65535 1831:6935 1831:65535 1832:8069 1832:65535 1833:9219 1833:65535 1834:10387 1834:65535 1835:11573 1835:65535 1836:12777 1836:65535 1837:14001 1837:65535 1838:15245 1838:65535 1839:16505 1839:65535 1840:17789 1840:65535 1841:19089 1841:65535 1842:20413 1842:65535 1843:21755 1843:65535 1844:23119 1844:65535 1845:24505 1845:65535 1846:25911 1846:65535 1847:27341 1847:65535 1848:28791 1848:65535 1849:30265 1849:65535 1850:31761 1850:65535 1851:33283 1851:65535 1852:34827 1852:65535 1853:36393 1853:65535 1854:37987 1854:65535 1855:39605 1855:65535 1856:41247 1856:65535 1857:42917 1857:65535 1858:44609 1858:65535 1859:46333 1859:65535 1860:48079 1860:65535 1861:49855 1861:65535 1862:51657 1862:65535 1863:53489 1863:65535 1864:55347 1864:65535 1865:57239 1865:65535 1866:59155 1866:65535 1867:61103 1867:65535 1868:63083 1868:65535 1869:65093 1869:65535 1870:1598 1870:65535 1870:65535 1871:3670 1871:65535 1871:65535 1872:5778 1872:65535 1872:65535 1873:7914 1873:65535 1873:65535 1874:10086 1874:65535 1874:65535 1875:12292 1875:65535 1875:65535 1876:14532 1876:65535 1876:65535 1877:16808 1877:65535 1877:65535 1878:19118 1878:65535 1878:65535 1879:21464 1879:65535 1879:65535 1880:23846 1880:65535 1880:65535 1881:26270 1881:65535 1881:65535 1882:28726 1882:65535 1882:65535 1883:31224 1883:65535 1883:65535 1884:33758 1884:65535 1884:65535 1885:36336 1885:65535 1885:65535 1886:38950 1886:65535 1886:65535 1887:41606 1887:65535 1887:65535 1888:44306 1888:65535 1888:65535 1889:47046 1889:65535 1889:65535 1890:49828 1890:65535 1890:65535 1891:52654 1891:65535 1891:65535 1892:55526 1892:65535 1892:65535 1893:58442 1893:65535 1893:65535 1894:61402 1894:65535 1894:65535 1895:64410 1895:65535 1895:65535 1896:1929 1896:65535 1896:65535 1896:65535 1897:5031 1897:65535 1897:65535 1897:65535 1898:8181 1898:65535 1898:65535 1898:65535 1899:11381 1899:65535 1899:65535 1899:65535 1900:14633 1900:65535 1900:65535 1900:65535 1901:17931 1901:65535 1901:65535 1901:65535 1902:21283 1902:65535 1902:65535 1902:65535 1903:24689 1903:65535 1903:65535 1903:65535 1904:28147 1904:65535 1904:65535 1904:65535 1905:31657 1905:65535 1905:65535 1905:65535 1906:35225 1906:65535 1906:65535 1906:65535 1907:38847 1907:65535 1907:65535 1907:65535 1908:42525 1908:65535 1908:65535 1908:65535 1909:46263 1909:65535 1909:65535 1909:65535 1910:50057 1910:65535 1910:65535 1910:65535 1911:53911 1911:65535 1911:65535 1911:65535 1912:57825 1912:65535 1912:65535 1912:65535 1913:61801 1913:65535 1913:65535 1913:65535 1914:304 1914:65535 1914:65535 1914:65535 1914:65535 1915:4404 1915:65535 1915:65535 1915:65535 1915:65535 1916:8570 1916:65535 1916:65535 1916:65535 1916:65535 1917:12798 1917:65535 1917:65535 1917:65535 1917:65535 1918:17094 1918:65535 1918:65535 1918:65535 1918:65535 1919:21458 1919:65535 1919:65535 1919:65535 1919:65535 1920:25890 1920:65535 1920:65535 1920:65535 1920:65535 1921:30390 1921:65535 1921:65535 1921:65535 1921:65535 1922:34960 1922:65535 1922:65535 1922:65535 1922:65535 1923:39602 1923:65535 1923:65535 1923:65535 1923:65535 1924:44316 1924:65535 1924:65535 1924:65535 1924:65535 1925:49106 1925:65535 1925:65535 1925:65535 1925:65535 1926:53970 1926:65535 1926:65535 1926:65535 1926:65535 1927:58906 1927:65535 1927:65535 1927:65535 1927:65535 1928:63926 1928:65535 1928:65535 1928:65535 1928:65535 1929:3483 1929:65535 1929:65535 1929:65535 1929:65535 1929:65535 1930:8659 1930:65535 1930:65535 1930:65535 1930:65535 1930:65535 1931:13913 1931:65535 1931:65535 1931:65535 1931:65535 1931:65535 1932:34822" },
        ];

        for (idx, case) in cases.iter().enumerate() {
            let mut sketch = DDSketch::default();
            assert!(sketch.is_empty());

            println!("running single insert case #{}...", idx);

            sketch.insert_interpolate_bucket(case.lower, case.upper, case.count);
            check_result(&sketch, case);

            println!("finished single insert case #{}.", idx);
        }

        for (idx, case) in double_insert_cases.iter().enumerate() {
            let mut sketch = DDSketch::default();
            assert!(sketch.is_empty());

            println!("running double insert case #{}...", idx);

            sketch.insert_interpolate_bucket(case.lower, case.upper, case.count);
            sketch.insert_interpolate_bucket(case.lower, case.upper, case.count);

            let mut case = case.clone();
            case.count *= 2;
            check_result(&sketch, &case);

            println!("finished double insert case #{}.", idx);
        }
    }

    fn test_relative_accuracy<P: SketchParameters>(min_value: f32, max_value: f32) {
        let max_observed_rel_acc = check_max_relative_accuracy::<P>(min_value, max_value);
        assert!(
            max_observed_rel_acc <= P::RELATIVE_ACCURACY + FLOATING_POINT_ACCEPTABLE_ERROR,
            "observed out of bound max relative acc: {max_observed_rel_acc}, target rel acc={}",
            P::RELATIVE_ACCURACY
        );
    }

    fn compute_relative_accuracy(target: f64, actual: f64) -> f64 {
        assert!(
            !(target < 0.0 || actual < 0.0),
            "expected/actual values must be greater than 0.0; target={target}, actual={actual}",
        );

        if target == actual {
            0.0
        } else if target == 0.0 {
            if actual == 0.0 {
                0.0
            } else {
                f64::INFINITY
            }
        } else if actual < target {
            (target - actual) / target
        } else {
            (actual - target) / target
        }
    }

    fn check_max_relative_accuracy<P: SketchParameters>(min_value: f32, max_value: f32) -> f64 {
        assert!(min_value < max_value, "min_value must be less than max_value");

        let mut v = min_value;
        let mut max_relative_acc = 0.0;
        while v < max_value {
            let target = f64::from(v);
            let actual = P::bin_lower_bound(P::key(target));

            let relative_acc = compute_relative_accuracy(target, actual);
            if relative_acc > max_relative_acc {
                max_relative_acc = relative_acc;
            }

            v = f32::from_bits(v.to_bits() + 1);
        }

        // Final iteration to make sure we hit the highest value.
        let actual = P::bin_lower_bound(P::key(f64::from(max_value)));
        let relative_acc = compute_relative_accuracy(f64::from(max_value), actual);
        if relative_acc > max_relative_acc {
            max_relative_acc = relative_acc;
        }

        max_relative_acc
    }

    // Merge edge case tests

    #[test]
    fn test_merge_empty_sketches() {
        let mut s1: DDSketch = DDSketch::default();
        let s2: DDSketch = DDSketch::default();
        s1.merge(&s2);
        assert!(s1.is_empty());
        assert_eq!(s1.count(), 0);
    }

    #[test]
    fn test_merge_into_empty() {
        let mut s1: DDSketch = DDSketch::default();
        let mut s2: DDSketch = DDSketch::default();
        s2.insert(1.0);
        s2.insert(2.0);
        s1.merge(&s2);
        assert_eq!(s1.count(), 2);
        assert_eq!(s1.min(), Some(1.0));
        assert_eq!(s1.max(), Some(2.0));
    }

    #[test]
    fn test_merge_from_empty() {
        let mut s1: DDSketch = DDSketch::default();
        s1.insert(1.0);
        s1.insert(2.0);
        let s2: DDSketch = DDSketch::default();
        let original_count = s1.count();
        let original_min = s1.min();
        let original_max = s1.max();
        s1.merge(&s2);
        assert_eq!(s1.count(), original_count);
        assert_eq!(s1.min(), original_min);
        assert_eq!(s1.max(), original_max);
    }

    #[test]
    fn test_merge_no_bin_overlap() {
        let mut s1: DDSketch = DDSketch::default();
        let mut s2: DDSketch = DDSketch::default();
        // Insert values in very different ranges to ensure no bin overlap
        s1.insert(0.001);
        s2.insert(1_000_000.0);
        let bins_before = s1.bin_count() + s2.bin_count();
        s1.merge(&s2);
        // After merge, should have all bins from both sketches since they don't overlap
        assert_eq!(s1.bin_count(), bins_before);
        assert_eq!(s1.count(), 2);
    }

    // Quantile boundary tests

    #[test]
    fn test_quantile_empty_sketch() {
        let sketch: DDSketch = DDSketch::default();
        assert_eq!(sketch.quantile(0.0), None);
        assert_eq!(sketch.quantile(0.5), None);
        assert_eq!(sketch.quantile(1.0), None);
    }

    #[test]
    fn test_quantile_exact_boundaries() {
        let mut sketch: DDSketch = DDSketch::default();
        for i in 1..=100 {
            sketch.insert(f64::from(i));
        }

        // q=0.0 should return min
        assert_eq!(sketch.quantile(0.0), Some(1.0));

        // q=1.0 should return max
        assert_eq!(sketch.quantile(1.0), Some(100.0));

        // q=0.5 should be close to median (within relative accuracy)
        let median = sketch.quantile(0.5).unwrap();
        assert!((median - 50.5).abs() < 2.0, "median {median} should be close to 50.5");
    }

    #[test]
    fn test_quantile_out_of_range_clamps() {
        let mut sketch: DDSketch = DDSketch::default();
        sketch.insert(10.0);
        sketch.insert(20.0);
        sketch.insert(30.0);

        // Negative quantile should clamp to min
        assert_eq!(sketch.quantile(-0.5), Some(10.0));

        // Quantile > 1.0 should clamp to max
        assert_eq!(sketch.quantile(1.5), Some(30.0));
    }

    // Large count tests

    #[test]
    fn test_insert_n_exceeds_bin_count_max() {
        let mut sketch: DDSketch = DDSketch::default();
        // Insert more than u16::MAX (65535) into a single value, which should create multiple bins
        let large_count = u64::from(u16::MAX) + 100;
        sketch.insert_n(1.0, large_count);

        // Should have created multiple bins for the same key
        assert!(sketch.bin_count() >= 2, "should have multiple bins for overflow");
        assert_eq!(sketch.count(), large_count);
    }

    #[test]
    fn test_insert_n_exactly_max() {
        let mut sketch: DDSketch = DDSketch::default();
        // Insert exactly u16::MAX
        sketch.insert_n(1.0, u64::from(u16::MAX));
        assert_eq!(sketch.count(), u64::from(u16::MAX));
        // Should fit in one bin
        assert_eq!(sketch.bin_count(), 1);
    }

    #[test]
    fn test_insert_many_large_batch() {
        let mut sketch: DDSketch = DDSketch::default();
        // Insert a large batch of values
        let values: Vec<f64> = (0..10_000).map(|i| f64::from(i) * 0.1).collect();
        sketch.insert_many(&values);
        assert_eq!(sketch.count(), 10_000);
    }

    // Special float value tests
    // These tests document current behavior with special float values.

    #[test]
    fn test_insert_zero() {
        let mut sketch: DDSketch = DDSketch::default();
        sketch.insert(0.0);
        assert_eq!(sketch.count(), 1);
        assert_eq!(sketch.min(), Some(0.0));
        assert_eq!(sketch.max(), Some(0.0));
        // Zero should map to key 0
        assert_eq!(sketch.bin_count(), 1);
    }

    #[test]
    fn test_insert_negative_zero() {
        let mut sketch: DDSketch = DDSketch::default();
        sketch.insert(-0.0);
        assert_eq!(sketch.count(), 1);
        // Negative zero should be treated the same as zero
        assert_eq!(sketch.bin_count(), 1);
    }

    #[test]
    fn test_insert_very_small_positive() {
        let mut sketch: DDSketch = DDSketch::default();
        // Insert a value smaller than NORM_MIN (should map to key 0)
        sketch.insert(1.0e-15);
        assert_eq!(sketch.count(), 1);
        // Very small values below NORM_MIN map to key 0 (the zero band)
        let key = AgentSketchParameters::key(1.0e-15);
        assert_eq!(key, 0);
    }

    #[test]
    fn test_insert_negative_values() {
        let mut sketch: DDSketch = DDSketch::default();
        sketch.insert(-5.0);
        sketch.insert(-10.0);
        sketch.insert(-1.0);
        assert_eq!(sketch.count(), 3);
        assert_eq!(sketch.min(), Some(-10.0));
        assert_eq!(sketch.max(), Some(-1.0));
        // Negative values should have negative keys
        let key = AgentSketchParameters::key(-5.0);
        assert!(key < 0);
    }
}
