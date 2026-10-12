//! How DogStatsD contexts and their traffic spread over stage 9's key groups and state lanes (plan section 8.6, M5).
//!
//! Stage 9 routes each event to a state lane by its context key: `group = key >> 56`, one of 256 key groups, and the
//! groups are contiguous ranges over the P lanes (plan section 5.3). This resolves lading-like DogStatsD contexts
//! through the source's own decode path (`DatagramDecoder`), takes each context's key as the source computes it, and
//! reports how evenly contexts, and the events of two traffic shapes, fall over groups and over P lanes in 2, 4, 8,
//! and 16: lading's (every context equally often) and power-law (the context of rank `r` with weight `1 / (r + 1)`).
//! The largest lane's share against the mean is what the slowest lane does more than its share; a single hot context
//! bounds it from below under power-law traffic, which only splitting a hot key would remove (a non-goal), and the
//! rest is what moving key groups between lanes (stage 10) could even out.
//!
//! The worker time per metric at equal throughput per P, the other half of M5, needs state lanes; it is measured once
//! they exist (increment 9.4).

use saluki_components::sources::dogstatsd_measurement::DatagramDecoder;
use saluki_core::data_model::event::Event;

/// The key groups of stage 9's key-group table.
const KEY_GROUPS: usize = 256;

/// The state lane counts measured.
const LANES: [usize; 4] = [2, 4, 8, 16];

/// A small deterministic generator, so that every run resolves the same contexts.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn between(&mut self, min: usize, max: usize) -> usize {
        min + (self.next() % (max - min + 1) as u64) as usize
    }

    fn ascii(&mut self, len: usize) -> String {
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";
        (0..len)
            .map(|_| ALPHABET[(self.next() % ALPHABET.len() as u64) as usize] as char)
            .collect()
    }
}

/// Returns the line of context `index`, shaped as the SMP DogStatsD cases' lading generator shapes them: a name of 1
/// to 200 bytes and 2 to 50 tags of 3 to 150 bytes, here made distinct by the index.
fn line(index: usize, rng: &mut XorShift) -> String {
    let name_len = rng.between(1, 200);
    let name = format!("{}.{}", rng.ascii(name_len), index);
    let tags = (0..rng.between(2, 50))
        .map(|_| {
            let len = rng.between(3, 150);
            let key = rng.ascii(len / 3);
            let value = rng.ascii(len - len / 3 - 1);
            format!("{}:{}", key, value)
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{}:1|c|#{}", name, tags)
}

/// Returns the largest and smallest of `values` against their mean.
fn spread(values: &[f64]) -> (f64, f64) {
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let max = values.iter().copied().fold(f64::MIN, f64::max);
    let min = values.iter().copied().fold(f64::MAX, f64::min);
    (max / mean, min / mean)
}

#[test]
#[ignore = "a measurement: run it in release mode, as the parent module's documentation says"]
fn key_skew_over_key_groups_and_state_lanes() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    // The context resolvers start their caches' expiry tasks on the runtime.
    let _runtime = runtime.enter();
    let context_counts = std::env::var("SALUKI_MEASURE_CONTEXTS_LIST")
        .ok()
        .map(|list| list.split(',').map(|count| count.trim().parse().unwrap()).collect())
        .unwrap_or_else(|| vec![3_000usize, 100_000, 1_000_000]);

    println!(
        "| contexts | lanes | contexts per group, max / mean | contexts per lane, max / mean (min / mean) | power-law \
         events per lane, max / mean (min / mean) | hottest context's share of all events |"
    );
    println!("|---|---|---|---|---|---|");
    for contexts in context_counts {
        let mut decoder = DatagramDecoder::with_defaults().expect("the default decoder builds");
        let mut rng = XorShift(0x9e37_79b9_7f4a_7c15 ^ contexts as u64);
        let mut events = Vec::with_capacity(1);
        let keys = (0..contexts)
            .map(|index| {
                events.clear();
                decoder.decode(line(index, &mut rng).as_bytes(), &mut events);
                match events.first() {
                    Some(Event::Metric(metric)) => metric.context().key().as_u64(),
                    other => panic!("the line of context {} decodes to one metric, not {:?}", index, other),
                }
            })
            .collect::<Vec<_>>();
        // The context of rank r, in generation order, which is unrelated to the key, has weight 1 / (r + 1).
        let weights = (0..contexts).map(|rank| 1.0 / (rank + 1) as f64).collect::<Vec<_>>();
        let total_weight = weights.iter().sum::<f64>();

        let mut per_group = vec![0.0; KEY_GROUPS];
        for key in &keys {
            per_group[(key >> 56) as usize] += 1.0;
        }
        let (group_max, _) = spread(&per_group);
        for lanes in LANES {
            let lane_of = |key: u64| (key >> 56) as usize * lanes / KEY_GROUPS;
            let (mut per_lane, mut weight_per_lane) = (vec![0.0; lanes], vec![0.0; lanes]);
            for (key, weight) in keys.iter().zip(&weights) {
                per_lane[lane_of(*key)] += 1.0;
                weight_per_lane[lane_of(*key)] += weight;
            }
            let (contexts_max, contexts_min) = spread(&per_lane);
            let (events_max, events_min) = spread(&weight_per_lane);
            println!(
                "| {} | {} | {:.2} | {:.3} ({:.3}) | {:.3} ({:.3}) | {:.3} |",
                contexts,
                lanes,
                group_max,
                contexts_max,
                contexts_min,
                events_max,
                events_min,
                weights[0] / total_weight
            );
        }
    }
}
