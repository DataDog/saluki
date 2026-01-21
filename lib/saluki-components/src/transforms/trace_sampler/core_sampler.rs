#![allow(dead_code)]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use saluki_common::collections::FastHashMap;
use saluki_common::hash::FastBuildHasher;

use super::signature::Signature;

const NUM_BUCKETS: usize = 6;
const BUCKET_DURATION: Duration = Duration::from_secs(5);
const MAX_RATE_INCREASE: f64 = 1.2;

#[derive(Default)]
pub struct Sampler {
    /// maps each Signature to a circular buffer of per-bucket (bucket_id) counts covering the last NUM_BUCKETS * BUCKET_DURATION window.
    seen: FastHashMap<Signature, [f32; NUM_BUCKETS]>,

    /// allSigSeen counts all signatures in a circular buffer of NUM_BUCKETS of BUCKET_DURATION
    all_sigs_seen: [f32; NUM_BUCKETS],

    last_bucket_id: u64,

    rates: FastHashMap<Signature, f64>,

    lowest_rate: f64,

    // TODO: add comments for the source code, etc.
    target_tps: f64,

    extra_rate: f64,
}

// zeroAndGetMax zeroes expired buckets and returns the max count
// logic taken from here: https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/sampler/coresampler.go#L185
fn zero_and_get_max(buckets: &mut [f32; NUM_BUCKETS], previous_bucket: u64, new_bucket: u64) -> f32 {
    // A bucket is a BUCKET_DURATION slice (5s) that stores the count of traces that fell in the interval.
    // An intuitive understanding of the function is that we start just after previous_buckets and iterate for a full window of buckets (NUM_BUCKETS)
    // and zero out any buckets older then new_buckets (expired), then we compute the max_count amoung the buckets that are in the current window
    let mut max_bucket = 0 as f32;
    for i in (previous_bucket + 1)..=previous_bucket + NUM_BUCKETS as u64 {
        let index = i as usize % NUM_BUCKETS;
        // if a complete rotation (time between previous_bucket and new_bucket is more then NUM_BUCKETS * BUCKET_DURATION) happened between previous_bucket and new_bucket
        // all buckets will be zeroed
        if i < new_bucket {
            buckets[index] = 0.0;
            continue;
        }
        let value = buckets[index];
        if value > max_bucket {
            max_bucket = value;
        }
        // zeroing after taking in account the previous value of the bucket
        // overridden by this rotation. This allows to take in account all buckets
        if i == new_bucket {
            buckets[index] = 0.0;
        }
    }
    max_bucket
}

// compute_tps_per_sig distributes TPS looking at the seen_tps of all signatures.
// By default it spreads uniformly the TPS on all signatures. If a signature
// is low volume and does not use all of its TPS, the remaining is spread uniformly
// on all other signatures. The returned sig_target is the final per_signature TPS target
// logic taken from here: https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/sampler/coresampler.go#L167
fn compute_tps_per_sig(target_tps: f64, seen_tps: &[f64]) -> f64 {
    // Example: target_tps = 30, seen_tps = [5, 10, 100] → sorted stays [5, 10, 100], Initial sig_target = 30 / 3 = 10
    // Loop:
    // 1) c = 5 (< 10), so subtract: target_tps = 30 - 5 = 25
    // Recompute sig_target = 25 / 2 = 12.5
    // 2) c = 10 (< 12.5), subtract: target_tps = 25 - 10 = 15
    // Recompute sig_target = 15 / 1 = 15
    // 3) Next is last element, break.
    // Return sig_target = 15.
    // Interpretation: the low‑volume signatures "use up" 5 and 10 TPS, and the remaining budget (15) is the per‑signature target for the higher‑volume signature(s).
    let mut sorted: Vec<f64> = seen_tps.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    // compute the initial per_signature TPS budget by splitting target_tps across all signatures.
    let mut remaining_tps = target_tps;
    let mut sig_target = remaining_tps / sorted.len() as f64;

    for (i, c) in sorted.iter().enumerate() {
        if *c >= sig_target || i == sorted.len() - 1 {
            break;
        }
        remaining_tps -= c;
        sig_target = remaining_tps / (sorted.len() - i - 1) as f64;
    }
    sig_target
}

impl Sampler {
    pub fn new(extra_rate: f64, target_tps: f64) -> Sampler {
        Self {
            extra_rate,
            target_tps,
            ..Default::default()
        }
    }

    pub(super) fn count_weighted_sig(&mut self, now: SystemTime, signature: &Signature, n: f32) -> bool {
        // All traces within the same `BUCKET_DURATION` interval share the same bucket_id
        let bucket_id = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() / BUCKET_DURATION.as_secs();
        let prev_bucket_id = self.last_bucket_id;
        // If the bucket_id changed then the sliding window advanced and we need to recompute rates
        let update_rate = prev_bucket_id != bucket_id;
        if update_rate {
            self.update_rates(prev_bucket_id, bucket_id);
        }

        let buckets = self.seen.entry(*signature).or_insert([0 as f32; NUM_BUCKETS]);
        self.all_sigs_seen[(bucket_id % (NUM_BUCKETS as u64)) as usize] += n;
        buckets[(bucket_id % (NUM_BUCKETS as u64)) as usize] += n;
        update_rate
    }

    // update_rates distributes TPS on each signature and apply it to the moving
    // max of seen buckets.
    // Rates increase are bounded by 20% increases, it requires 13 evaluations (1.2**13 = 10.6)
    // to increase a sampling rate by 10 fold in about 1min.
    fn update_rates(&mut self, previous_bucket: u64, new_bucket: u64) {
        let seen_len = self.seen.len();
        if seen_len == 0 {
            return;
        }
        let mut rates: FastHashMap<Signature, f64> =
            FastHashMap::with_capacity_and_hasher(seen_len, FastBuildHasher::default());
        // seen_tps is a vector of per-signature peak rates, we get the maximum bucket value (which represents the number/weight of traces in a BUCKET_DURATION interval)
        // in the sliding window and convert that to traces per second. Each element is one TPS per signature.
        let mut seen_tps_vec = Vec::with_capacity(seen_len);
        let mut sigs = Vec::with_capacity(seen_len);
        let mut sigs_to_remove = Vec::new();

        for (sig, buckets) in self.seen.iter_mut() {
            let max_bucket = zero_and_get_max(buckets, previous_bucket, new_bucket);
            let seen_tps = max_bucket as f64 / BUCKET_DURATION.as_secs() as f64;
            seen_tps_vec.push(seen_tps);
            sigs.push(*sig);
        }
        zero_and_get_max(&mut self.all_sigs_seen, previous_bucket, new_bucket);
        let tps_per_sig = compute_tps_per_sig(self.target_tps, &seen_tps_vec);
        self.lowest_rate = 1.0;

        for (i, sig) in sigs.iter().enumerate() {
            let seen_tps = seen_tps_vec[i];
            let mut rate = 1.0;
            if tps_per_sig < seen_tps && seen_tps > 0.0 {
                rate = tps_per_sig / seen_tps;
            }

            // Cap increase rate to 20%
            if let Some(prev_rate) = self.rates.get(sig) {
                if *prev_rate != 0.0 && rate / prev_rate > MAX_RATE_INCREASE {
                    rate = prev_rate * MAX_RATE_INCREASE;
                }
            }

            // Ensure rate doesn't exceed 1.0
            if rate > 1.0 {
                rate = 1.0;
            }

            // No traffic on this signature, mark it for cleanup
            if rate == 1.0 && seen_tps == 0.0 {
                sigs_to_remove.push(*sig);
                continue;
            }

            // Update lowest rate
            if rate < self.lowest_rate {
                self.lowest_rate = rate;
            }

            rates.insert(*sig, rate);
        }

        // Clean up signatures with no traffic
        for sig in sigs_to_remove {
            self.seen.remove(&sig);
        }

        self.rates = rates;
    }

    /// Gets the sampling rate for a specific signature.
    /// Returns the rate multiplied by the extra rate factor.
    pub fn get_signature_sample_rate(&self, sig: &Signature) -> f64 {
        self.rates
            .get(sig)
            .map(|rate| rate * self.extra_rate)
            .unwrap_or_else(|| self.default_rate())
    }

    /// Gets all signature sample rates.
    /// Returns a tuple of (rates map, default rate).
    pub fn get_all_signature_sample_rates(&self) -> (FastHashMap<Signature, f64>, f64) {
        let mut rates = FastHashMap::with_capacity_and_hasher(self.rates.len(), FastBuildHasher::default());
        for (sig, rate) in self.rates.iter() {
            rates.insert(*sig, rate * self.extra_rate);
        }
        (rates, self.default_rate())
    }

    /// Computes the default rate for unknown signatures.
    /// Based on the moving max of all signatures seen and the lowest stored rate.
    fn default_rate(&self) -> f64 {
        if self.target_tps == 0.0 {
            return 0.0;
        }

        let mut max_seen = 0.0_f32;
        for &count in self.all_sigs_seen.iter() {
            if count > max_seen {
                max_seen = count;
            }
        }

        let seen_tps = max_seen as f64 / BUCKET_DURATION.as_secs() as f64;
        let mut rate = 1.0;

        if self.target_tps < seen_tps && seen_tps > 0.0 {
            rate = self.target_tps / seen_tps;
        }

        if self.lowest_rate < rate && self.lowest_rate != 0.0 {
            return self.lowest_rate;
        }

        rate
    }

    /// Updates the target TPS and adjusts all existing rates proportionally.
    pub fn update_target_tps(&mut self, new_target_tps: f64) {
        let previous_target_tps = self.target_tps;
        self.target_tps = new_target_tps;

        if previous_target_tps == 0.0 {
            return;
        }

        let ratio = new_target_tps / previous_target_tps;
        for rate in self.rates.values_mut() {
            *rate = (*rate * ratio).min(1.0);
        }
    }

    /// Get the current target TPS.
    pub fn get_target_tps(&self) -> f64 {
        self.target_tps
    }

    /// Returns the number of signatures being tracked.
    pub fn size(&self) -> i64 {
        self.seen.len() as i64
    }
}
